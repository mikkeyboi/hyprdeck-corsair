//! IPC server: NDJSON requests on the daemon socket (DESIGN §7), one request per connection.

use crate::ipc::{self, DeviceStatus, KeysFrame, Request, Response};
use anyhow::Result;
use std::io::Write;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

/// Longest request line accepted.
const MAX_REQUEST: u64 = 64 * 1024;
/// A client must send its request within this time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// How often an idle key stream checks whether the client went away.
const STREAM_CHECK: Duration = Duration::from_millis(500);

/// What the server needs from the daemon.
pub trait Backend: Send + Sync {
    fn hello(&self) -> Vec<DeviceStatus>;
    fn xfer(&self, device: &str, payload: &[u8]) -> Result<Vec<u8>>;
    fn rpc(
        &self,
        device: &str,
        operation: &ipc::Operation,
        deadline: std::time::Instant,
    ) -> Result<Vec<u8>>;
    fn reload(&self) -> Result<()>;
    fn subscribe(&self, device: &str) -> Result<Receiver<Vec<String>>>;
}

pub enum Outcome {
    Reply(Response),
    Stream(Receiver<Vec<String>>),
}

fn ok() -> Response {
    Response {
        ok: true,
        error: None,
        version: None,
        devices: None,
        reply: None,
    }
}

pub fn error(text: impl Into<String>) -> Response {
    Response {
        ok: false,
        error: Some(text.into()),
        ..ok()
    }
}

/// Handle one request line.
pub fn handle_line(line: &str, backend: &dyn Backend) -> Outcome {
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(_) => return Outcome::Reply(error("Malformed request")),
    };
    let result = match request {
        Request::Hello => Ok(Response {
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            devices: Some(backend.hello()),
            ..ok()
        }),
        Request::Reload => backend.reload().map(|()| ok()),
        Request::Xfer { device, payload } => ipc::from_hex(&payload)
            .and_then(|payload| backend.xfer(&device, &payload))
            .map(|reply| Response {
                reply: Some(ipc::to_hex(&reply)),
                ..ok()
            }),
        Request::Rpc {
            device,
            operation,
            timeout_ms,
        } => {
            let timeout = Duration::from_millis(timeout_ms).min(ipc::OPERATION_TIMEOUT);
            backend
                .rpc(&device, &operation, std::time::Instant::now() + timeout)
                .map(|reply| Response {
                    reply: Some(ipc::to_hex(&reply)),
                    ..ok()
                })
        }
        Request::Keys { device } => match backend.subscribe(&device) {
            Ok(frames) => return Outcome::Stream(frames),
            Err(error) => Err(error),
        },
    };
    Outcome::Reply(result.unwrap_or_else(|failure| error(format!("{failure:#}"))))
}

/// uid of the peer process (SO_PEERCRED).
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    // SAFETY: zeroed plain C struct.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid pointers to a ucred of `len` bytes for a live socket.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

fn write_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()
}

/// True when the client hung up (or sent more data, which this protocol does not allow).
fn client_gone(stream: &UnixStream) -> bool {
    let mut fds = [super::util::pollfd(
        stream.as_raw_fd(),
        libc::POLLIN | libc::POLLRDHUP,
    )];
    matches!(super::util::poll(&mut fds, Some(Duration::ZERO)), Ok(1))
        && fds[0].revents & (libc::POLLIN | libc::POLLRDHUP | super::util::POLL_GONE) != 0
}

/// Serve one connection: read one request, answer (or stream key frames until hangup).
pub fn serve(mut stream: UnixStream, backend: &dyn Backend) {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if peer_uid(&stream) != Some(uid) {
        let _ = write_line(&mut stream, &error("Permission denied"));
        return;
    }
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    let line = match ipc::read_line_until(&mut stream, deadline, MAX_REQUEST as usize) {
        Ok(line) => line,
        Err(error) => {
            let _ = write_line(&mut stream, &self::error(format!("{error:#}")));
            return;
        }
    };
    match handle_line(&line, backend) {
        Outcome::Reply(response) => {
            let _ = write_line(&mut stream, &response);
        }
        Outcome::Stream(frames) => loop {
            match frames.recv_timeout(STREAM_CHECK) {
                Ok(pressed) => {
                    if write_line(&mut stream, &KeysFrame { pressed }).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if client_gone(&stream) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::sync::Mutex;
    use std::sync::mpsc::{SyncSender, sync_channel};

    #[derive(Default)]
    struct FakeBackend {
        xfers: Mutex<Vec<(String, Vec<u8>)>>,
        reloads: Mutex<u32>,
        keys: Mutex<Option<SyncSender<Vec<String>>>>,
    }

    impl Backend for FakeBackend {
        fn hello(&self) -> Vec<DeviceStatus> {
            vec![DeviceStatus {
                id: "k65plus-a".into(),
                owned: true,
                mode: 2,
                grabbed: false,
                features: vec!["dial".into()],
                error: None,
                generation: None,
                frame_applied: None,
                remaps_active: None,
                config_revision: None,
            }]
        }
        fn xfer(&self, device: &str, payload: &[u8]) -> Result<Vec<u8>> {
            if device != "k65plus-a" {
                bail!("Keyboard {device} is not owned by the live-mode companion");
            }
            crate::bragi::check_allowed(crate::devices::Model::K65PlusWireless, payload)?;
            self.xfers
                .lock()
                .unwrap()
                .push((device.into(), payload.to_vec()));
            Ok(vec![0x01, payload[0], 0x00, 0xa4, 0x01])
        }
        fn rpc(
            &self,
            device: &str,
            operation: &ipc::Operation,
            deadline: std::time::Instant,
        ) -> Result<Vec<u8>> {
            crate::bragi::check_deadline(deadline)?;
            match operation {
                ipc::Operation::Xfer { payload } => self.xfer(device, payload),
                _ => bail!("no mock resource"),
            }
        }
        fn reload(&self) -> Result<()> {
            *self.reloads.lock().unwrap() += 1;
            Ok(())
        }
        fn subscribe(&self, device: &str) -> Result<Receiver<Vec<String>>> {
            if device != "k65plus-a" {
                bail!("unknown device");
            }
            let (tx, rx) = sync_channel(4);
            tx.send(vec![]).unwrap();
            *self.keys.lock().unwrap() = Some(tx);
            Ok(rx)
        }
    }

    fn reply(line: &str, backend: &FakeBackend) -> serde_json::Value {
        match handle_line(line, backend) {
            Outcome::Reply(response) => serde_json::to_value(response).unwrap(),
            Outcome::Stream(_) => panic!("unexpected stream"),
        }
    }

    #[test]
    fn requests_are_dispatched() {
        let backend = FakeBackend::default();
        let hello = reply(r#"{"op":"hello"}"#, &backend);
        assert_eq!(hello["ok"], true);
        assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(hello["devices"][0]["id"], "k65plus-a");
        assert_eq!(hello["devices"][0]["features"][0], "dial");
        assert_eq!(
            reply(r#"{"op":"reload"}"#, &backend),
            serde_json::json!({"ok": true})
        );
        assert_eq!(*backend.reloads.lock().unwrap(), 1);
        let xfer = reply(
            r#"{"op":"xfer","device":"k65plus-a","payload":"020f"}"#,
            &backend,
        );
        assert_eq!(xfer, serde_json::json!({"ok": true, "reply": "010200a401"}));
        assert_eq!(backend.xfers.lock().unwrap()[0].1, vec![0x02, 0x0f]);
    }

    #[test]
    fn bad_requests_get_errors_and_never_reach_the_device() {
        let backend = FakeBackend::default();
        for line in [
            "nope",
            r#"{"op":"format-flash"}"#,
            r#"{"op":"xfer","device":"k65plus-a","payload":"zz"}"#,
            r#"{"op":"xfer","device":"k65plus-a","payload":"0d01020000"}"#, // key table
            r#"{"op":"xfer","device":"k65plus-a","payload":"0b"}"#,
            r#"{"op":"xfer","device":"other","payload":"020f"}"#,
            r#"{"op":"keys","device":"other"}"#,
        ] {
            let response = reply(line, &backend);
            assert_eq!(response["ok"], false, "{line}");
            assert!(
                response["error"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
        }
        assert!(backend.xfers.lock().unwrap().is_empty());
    }

    #[test]
    fn connections_round_trip_and_stream_keys_until_hangup() {
        let backend = std::sync::Arc::new(FakeBackend::default());
        let (mut client, server) = UnixStream::pair().unwrap();
        let shared = backend.clone();
        let thread = std::thread::spawn(move || serve(server, &*shared));
        client.write_all(b"{\"op\":\"hello\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let response: Response = serde_json::from_str(&line).unwrap();
        assert!(response.ok);
        thread.join().unwrap();

        let (mut client, server) = UnixStream::pair().unwrap();
        let shared = backend.clone();
        let thread = std::thread::spawn(move || serve(server, &*shared));
        client
            .write_all(b"{\"op\":\"keys\",\"device\":\"k65plus-a\"}\n")
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"pressed":[]}"#);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while backend.keys.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let tx = backend.keys.lock().unwrap().clone().unwrap();
        tx.send(vec!["a".into(), "l-shift".into()]).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"pressed":["a","l-shift"]}"#);
        drop(reader);
        drop(client);
        let started = std::time::Instant::now();
        thread.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hangup ends the stream"
        );
    }

    #[test]
    fn real_ipc_client_talks_to_the_server() {
        let backend = std::sync::Arc::new(FakeBackend::default());
        let tmp = crate::devices::tests::TempDir::new("daemon-server");
        let path = tmp.short_path("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let shared = backend.clone();
        let thread = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                serve(stream, &*shared);
            }
        });
        let devices = ipc::hello_at(&path).unwrap().unwrap();
        assert_eq!(devices[0].id, "k65plus-a");
        assert!(ipc::reload_at(&path).unwrap());
        thread.join().unwrap();
    }
}

#[cfg(test)]
mod read_deadline_regressions {
    use super::*;
    use std::time::Instant;
    #[test]
    fn slow_bytes_cannot_reset_total_admission_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..12 {
                if client.write_all(b" ").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        });
        let start = Instant::now();
        assert!(
            ipc::read_line_until(&mut server, start + Duration::from_millis(60), 1024).is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(150));
        drop(server);
        writer.join().unwrap();
    }
}
