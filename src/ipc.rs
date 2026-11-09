//! Plugin <-> companion IPC over a unix socket (NDJSON). See docs/DESIGN.md §7.
//!
//! OWNER: foundation (types + client). The daemon owner implements the server in src/daemon.

use crate::bragi::{Reply, Transport};
use crate::storage;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Includes worker cleanup and reply delivery; the server work window is shorter.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(6);
pub const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Operation {
    Xfer {
        payload: Vec<u8>,
    },
    ReadResource {
        resource: u16,
    },
    WriteResource {
        resource: u16,
        declared_len: u32,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    Hello,
    Xfer {
        device: String,
        payload: String,
    },
    Rpc {
        device: String,
        operation: Operation,
        timeout_ms: u64,
    },
    Reload,
    Keys {
        device: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub id: String,
    pub owned: bool,
    pub mode: u8,
    pub grabbed: bool,
    pub features: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub generation: Option<u64>,
    #[serde(default)]
    pub frame_applied: Option<bool>,
    #[serde(default)]
    pub remaps_active: Option<bool>,
    #[serde(default)]
    pub config_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<DeviceStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

/// One frame of the `keys` subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeysFrame {
    pub pressed: Vec<String>,
}

/// Longest response line the client accepts.
const MAX_LINE: u64 = 1024 * 1024;

/// `$XDG_RUNTIME_DIR/hyprdeck-corsair/daemon.sock`
pub fn socket_path() -> Result<PathBuf> {
    Ok(storage::runtime_dir()?.join("daemon.sock"))
}

/// Connect to the daemon; Ok(None) when nobody is listening.
fn connect(path: &Path) -> Result<Option<UnixStream>> {
    let deadline = crate::bragi::bounded_deadline(CLIENT_TIMEOUT);
    crate::bragi::check_deadline(deadline)?;
    // Nonblocking AF_UNIX connect avoids waiting forever on a full listen backlog.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() {
        bail!("Companion socket path too long");
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as libc::c_char;
    }
    // SAFETY: socket creates one fresh descriptor; OwnedFd takes sole responsibility for it.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    loop {
        crate::bragi::check_deadline(deadline)?;
        // SAFETY: initialized sockaddr_un contains the byte-exact NUL-terminated path.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            Some(libc::ENOENT | libc::ECONNREFUSED) => return Ok(None),
            Some(libc::EINTR | libc::EAGAIN) => {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Some(libc::EINPROGRESS | libc::EALREADY) => {
                let mut poll = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let wait = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, 50) as i32;
                // SAFETY: poll references one valid stack pollfd.
                unsafe {
                    libc::poll(&mut poll, 1, wait);
                }
            }
            _ => {
                return Err(error).with_context(|| {
                    format!(
                        "Connecting to the live-mode companion at {}",
                        path.display()
                    )
                });
            }
        }
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(Some(stream))
}

fn send(stream: &mut UnixStream, request: &Request) -> Result<()> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .context("Sending to the live-mode companion")?;
    stream.flush()?;
    Ok(())
}

/// Send one request to the socket at `path`, read one response line. Ok(None) when no daemon
/// is listening.
pub(crate) fn call_at(path: &Path, request: &Request) -> Result<Option<Response>> {
    let Some(mut stream) = connect(path)? else {
        return Ok(None);
    };
    let deadline = crate::bragi::bounded_deadline(CLIENT_TIMEOUT);
    crate::bragi::check_deadline(deadline)?;
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    send(&mut stream, request)?;
    let line = read_line_until(&mut stream, deadline, MAX_LINE as usize)?;
    let response =
        serde_json::from_str(&line).context("Malformed reply from the live-mode companion")?;
    Ok(Some(response))
}

/// Read a bounded NDJSON line under one total monotonic deadline (not per-byte timeout).
pub(crate) fn read_line_until(
    stream: &mut UnixStream,
    deadline: Instant,
    max: usize,
) -> Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        crate::bragi::check_deadline(deadline)?;
        stream.set_read_timeout(Some(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))?;
        match stream.read(&mut chunk) {
            Ok(0) => bail!("Connection closed before complete request/reply"),
            Ok(n) => {
                let end = chunk[..n].iter().position(|b| *b == b'\n');
                let take = end.unwrap_or(n);
                if bytes.len() + take > max {
                    bail!("Request/reply too long");
                }
                bytes.extend_from_slice(&chunk[..take]);
                if end.is_some() {
                    return String::from_utf8(bytes).context("Invalid UTF-8 request/reply");
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("Reading bounded request/reply"),
        }
    }
}

fn into_result(response: Response) -> Result<Response> {
    if response.ok {
        Ok(response)
    } else {
        bail!(
            "Live-mode companion: {}",
            response.error.as_deref().unwrap_or("request failed")
        )
    }
}

/// `hello` convenience: Ok(None) when the daemon is not running.
pub fn hello() -> Result<Option<Vec<DeviceStatus>>> {
    hello_at(&socket_path()?)
}

pub(crate) fn hello_at(path: &Path) -> Result<Option<Vec<DeviceStatus>>> {
    match call_at(path, &Request::Hello)? {
        None => Ok(None),
        Some(response) => Ok(Some(into_result(response)?.devices.unwrap_or_default())),
    }
}

/// Ask a running daemon to reload live.json; Ok(false) when no daemon is running.
pub fn reload() -> Result<bool> {
    reload_at(&socket_path()?)
}

pub(crate) fn reload_at(path: &Path) -> Result<bool> {
    match call_at(path, &Request::Reload)? {
        None => Ok(false),
        Some(response) => into_result(response).map(|_| true),
    }
}

pub fn subscribe_keys_timed(
    device: &str,
    timeout: Duration,
) -> Result<Option<Box<dyn Iterator<Item = Result<KeysFrame>>>>> {
    subscribe_keys_at_with_timeout(&socket_path()?, device, Some(timeout))
}

#[cfg(test)]
pub(crate) fn subscribe_keys_at(
    path: &Path,
    device: &str,
) -> Result<Option<Box<dyn Iterator<Item = Result<KeysFrame>>>>> {
    subscribe_keys_at_with_timeout(path, device, None)
}
fn subscribe_keys_at_with_timeout(
    path: &Path,
    device: &str,
    timeout: Option<Duration>,
) -> Result<Option<Box<dyn Iterator<Item = Result<KeysFrame>>>>> {
    let Some(mut stream) = connect(path)? else {
        return Ok(None);
    };
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
    send(
        &mut stream,
        &Request::Keys {
            device: device.to_string(),
        },
    )?;
    // Frames arrive only on key changes: block without a read timeout until the daemon hangs up.
    stream.set_read_timeout(timeout)?;
    let mut reader = BufReader::new(stream);
    let mut done = false;
    let mut pending = Vec::new();
    let mut line_started: Option<Instant> = None;
    let frames = std::iter::from_fn(move || {
        if done {
            return None;
        }
        let item = loop {
            if line_started.is_some_and(|at| at.elapsed() >= CLIENT_TIMEOUT) {
                break Err(anyhow::anyhow!("Incomplete key frame deadline expired"));
            }
            match reader.fill_buf() {
                Ok([]) => {
                    done = true;
                    return None;
                }
                Ok(chunk) => {
                    line_started.get_or_insert_with(Instant::now);
                    let end = chunk.iter().position(|byte| *byte == b'\n');
                    let take = end.unwrap_or(chunk.len());
                    if pending.len() + take > MAX_LINE as usize {
                        break Err(anyhow::anyhow!("Oversized key frame"));
                    }
                    pending.extend_from_slice(&chunk[..take]);
                    reader.consume(take + usize::from(end.is_some()));
                    if end.is_some() {
                        let parsed = String::from_utf8(std::mem::take(&mut pending))
                            .context("Invalid UTF-8 key frame")
                            .and_then(|line| parse_keys_line(&line));
                        line_started = None;
                        break parsed;
                    }
                }
                Err(error) => {
                    let timed_out = timeout.is_some()
                        && matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut);
                    if !timed_out {
                        done = true;
                    }
                    return Some(Err(error.into()));
                }
            }
        };
        done = item.is_err();
        Some(item)
    });
    Ok(Some(Box::new(frames)))
}

fn parse_keys_line(line: &str) -> Result<KeysFrame> {
    if let Ok(frame) = serde_json::from_str::<KeysFrame>(line) {
        return Ok(frame);
    }
    match serde_json::from_str::<Response>(line) {
        Ok(response) => {
            into_result(response)?;
            bail!("Unexpected reply in the key stream")
        }
        Err(_) => bail!("Malformed key frame from the live-mode companion"),
    }
}

/// Transport that proxies each xfer through the daemon (only valid when the daemon owns the device).
pub struct DaemonTransport {
    pub device: String,
}

impl DaemonTransport {
    #[cfg(test)]
    fn xfer_at(&self, path: &Path, payload: &[u8]) -> Result<Reply> {
        let request = Request::Xfer {
            device: self.device.clone(),
            payload: to_hex(payload),
        };
        let response =
            call_at(path, &request)?.context("Live-mode companion is not running — refresh")?;
        let response = into_result(response)?;
        let raw = from_hex(
            response
                .reply
                .as_deref()
                .context("Live-mode companion sent no reply bytes")?,
        )?;
        if raw.len() < 3 || raw[1] != payload.first().copied().unwrap_or(0) {
            bail!("Live-mode companion returned a mismatched reply");
        }
        Ok(Reply { raw })
    }
}

impl DaemonTransport {
    fn operation(&self, operation: Operation, deadline: Instant) -> Result<Vec<u8>> {
        crate::bragi::check_deadline(deadline)?;
        let request = Request::Rpc {
            device: self.device.clone(),
            operation,
            timeout_ms: deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(OPERATION_TIMEOUT.as_millis()) as u64,
        };
        let response =
            call_at(&socket_path()?, &request)?.context("Live-mode companion is not running")?;
        let response = into_result(response)?;
        from_hex(
            response
                .reply
                .as_deref()
                .context("Live-mode companion omitted the reply")?,
        )
    }
}
impl Transport for DaemonTransport {
    fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
        self.xfer_until(payload, crate::bragi::bounded_deadline(OPERATION_TIMEOUT))
    }
    fn xfer_until(&mut self, payload: &[u8], deadline: Instant) -> Result<Reply> {
        let raw = self.operation(
            Operation::Xfer {
                payload: payload.to_vec(),
            },
            deadline,
        )?;
        if raw.len() < 3 || raw[1] != payload.first().copied().unwrap_or(0) {
            bail!("Companion returned a mismatched reply");
        }
        Ok(Reply { raw })
    }
    fn read_resource_until(&mut self, resource: u16, deadline: Instant) -> Option<Result<Vec<u8>>> {
        Some(self.operation(Operation::ReadResource { resource }, deadline))
    }
    fn write_resource_until(
        &mut self,
        resource: u16,
        declared_len: u32,
        bytes: &[u8],
        deadline: Instant,
    ) -> Option<Result<()>> {
        Some(
            self.operation(
                Operation::WriteResource {
                    resource,
                    declared_len,
                    bytes: bytes.to_vec(),
                },
                deadline,
            )
            .map(|_| ()),
        )
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
/// Parse hex (case-insensitive); ASCII whitespace between digits is ignored.
pub fn from_hex(text: &str) -> Result<Vec<u8>> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !digits.len().is_multiple_of(2) {
        bail!("Hex string has an odd number of digits");
    }
    digits
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok().context("Invalid hex")?;
            u8::from_str_radix(pair, 16)
                .ok()
                .with_context(|| format!("Invalid hex byte {pair:?}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::tests::TempDir;
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    /// Mock daemon: answers each connection's first line with `respond(request)` lines.
    fn mock_daemon(
        path: &Path,
        connections: usize,
        respond: impl Fn(Request) -> Vec<String> + Send + 'static,
    ) -> JoinHandle<Vec<Request>> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                seen.push(request.clone());
                let mut writer = stream;
                for reply in respond(request) {
                    writer.write_all(reply.as_bytes()).unwrap();
                    writer.write_all(b"\n").unwrap();
                }
            }
            seen
        })
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0x7e, 0xa0, 0x02, 0x03]), "7ea00203");
        assert_eq!(
            from_hex("7E a0 02 03").unwrap(),
            vec![0x7e, 0xa0, 0x02, 0x03]
        );
        assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
        assert!(from_hex("7").is_err());
        assert!(from_hex("zz").is_err());
    }

    #[test]
    fn request_wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Hello).unwrap(),
            r#"{"op":"hello"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Reload).unwrap(),
            r#"{"op":"reload"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Xfer {
                device: "k".into(),
                payload: "0203".into()
            })
            .unwrap(),
            r#"{"op":"xfer","device":"k","payload":"0203"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Keys { device: "k".into() }).unwrap(),
            r#"{"op":"keys","device":"k"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response {
                ok: true,
                error: None,
                version: None,
                devices: None,
                reply: None
            })
            .unwrap(),
            r#"{"ok":true}"#
        );
    }

    #[test]
    fn missing_or_stale_socket_means_no_daemon() {
        let tmp = TempDir::new("ipc-missing");
        let path = tmp.short_path("daemon.sock");
        assert_eq!(call_at(&path, &Request::Hello).unwrap(), None);
        assert_eq!(hello_at(&path).unwrap(), None);
        assert!(!reload_at(&path).unwrap());
        assert!(subscribe_keys_at(&path, "k").unwrap().is_none());
        // A socket file nobody listens on (daemon crashed) is "not running" too.
        drop(UnixListener::bind(&path).unwrap());
        assert_eq!(call_at(&path, &Request::Hello).unwrap(), None);
        let error = DaemonTransport { device: "k".into() }
            .xfer_at(&path, &[0x02, 0x0f])
            .unwrap_err();
        assert!(error.to_string().contains("not running"));
    }

    #[test]
    fn hello_and_reload_round_trip() {
        let tmp = TempDir::new("ipc-hello");
        let path = tmp.short_path("daemon.sock");
        let server = mock_daemon(&path, 3, |request| {
            match request {
            Request::Hello => vec![r#"{"ok":true,"version":"0.1.0","devices":[{"id":"k65plus-a","owned":true,"mode":2,"grabbed":false,"features":["dial"]}]}"#.into()],
            Request::Reload => vec![r#"{"ok":true}"#.into()],
            _ => vec![r#"{"ok":false,"error":"nope"}"#.into()],
        }
        });
        let devices = hello_at(&path).unwrap().unwrap();
        assert_eq!(
            devices,
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
        );
        assert!(reload_at(&path).unwrap());
        let error = call_at(&path, &Request::Keys { device: "x".into() })
            .unwrap()
            .unwrap();
        assert!(!error.ok);
        assert_eq!(
            server.join().unwrap(),
            vec![
                Request::Hello,
                Request::Reload,
                Request::Keys { device: "x".into() }
            ]
        );
    }

    #[test]
    fn daemon_transport_proxies_hex_payloads() {
        let tmp = TempDir::new("ipc-xfer");
        let path = tmp.short_path("daemon.sock");
        let server = mock_daemon(&path, 2, |request| match request {
            Request::Xfer { payload, .. } if payload == "020f" => {
                vec![r#"{"ok":true,"reply":"010200a401"}"#.into()]
            }
            _ => vec![r#"{"ok":false,"error":"device not owned"}"#.into()],
        });
        let transport = DaemonTransport {
            device: "k65plus-a".into(),
        };
        let reply = transport.xfer_at(&path, &[0x02, 0x0f]).unwrap();
        assert_eq!(reply.value_u32(), 420);
        let error = transport.xfer_at(&path, &[0x02, 0x02]).unwrap_err();
        assert!(error.to_string().contains("device not owned"), "{error}");
        let seen = server.join().unwrap();
        assert_eq!(
            seen[0],
            Request::Xfer {
                device: "k65plus-a".into(),
                payload: "020f".into()
            }
        );
    }

    #[test]
    fn silent_daemon_times_out() {
        let tmp = TempDir::new("ipc-timeout");
        let path = tmp.short_path("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let holder = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(CLIENT_TIMEOUT + Duration::from_millis(500));
            drop(stream);
        });
        let started = std::time::Instant::now();
        assert!(call_at(&path, &Request::Hello).is_err());
        assert!(started.elapsed() < CLIENT_TIMEOUT + Duration::from_secs(1));
        holder.join().unwrap();
    }

    #[test]
    fn key_subscription_streams_frames_until_hangup() {
        let tmp = TempDir::new("ipc-keys");
        let path = tmp.short_path("daemon.sock");
        let server = mock_daemon(&path, 2, |request| match request {
            Request::Keys { device } if device == "k65plus-a" => {
                vec![
                    r#"{"pressed":[]}"#.into(),
                    r#"{"pressed":["a","l-shift"]}"#.into(),
                ]
            }
            _ => vec![r#"{"ok":false,"error":"unknown device"}"#.into()],
        });
        let frames: Vec<KeysFrame> = subscribe_keys_at(&path, "k65plus-a")
            .unwrap()
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(
            frames,
            vec![
                KeysFrame { pressed: vec![] },
                KeysFrame {
                    pressed: vec!["a".into(), "l-shift".into()]
                }
            ]
        );
        let mut stream = subscribe_keys_at(&path, "other").unwrap().unwrap();
        let error = stream.next().unwrap().unwrap_err();
        assert!(error.to_string().contains("unknown device"), "{error}");
        assert!(stream.next().is_none());
        server.join().unwrap();
    }
}
