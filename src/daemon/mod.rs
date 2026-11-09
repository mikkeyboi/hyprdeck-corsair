//! Opt-in live-mode companion (`hyprdeck-corsair daemon`, systemd --user).
//! See docs/DESIGN.md §7.
//!
//! OWNER: daemon. Shared entry point used by main.rs.
//!
//! Threads: the main thread polls the signal self-pipe and the IPC socket and rescans sysfs every
//! 2 s; each keyboard gets a worker thread (software-mode ownership, IF2 events, dial emulation,
//! lighting frames); a grabbed keyboard gets a forwarder thread and a macro thread; each IPC
//! connection gets a short-lived thread. Everything blocks in poll(2) or on channels: idle CPU ≈ 0.

mod actions;
mod events;
pub(crate) mod frame;
mod hub;
mod output;
mod owner;
mod remap;
mod server;
mod signals;
mod util;
mod worker;

use crate::config::{self, LiveConfig};
use crate::devices::{self, Keyboard};
use crate::ipc::{self, DeviceStatus};
use crate::storage;
use anyhow::{Context, Result, bail};
use hub::KeyHub;
use server::Backend;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use worker::{Command, Mailbox, Platform, RealPlatform, Worker, WorkerSetup};

/// Hot-plug rescan period.
const SCAN_PERIOD: Duration = Duration::from_secs(2);
/// Bounded wait for workers on exit; ExecStopPost independently attempts remaining rescue.
const SHUTDOWN_BUDGET: Duration = Duration::from_millis(1900);
/// Plugin xfer requests wait at most this long for the worker.
const XFER_TIMEOUT: Duration = Duration::from_secs(3);
/// Concurrent IPC connections.
const MAX_CONNECTIONS: usize = 16;

/// Run until SIGTERM/SIGINT; common exit cleanup attempts ungrab and verified mode restoration.
pub fn run() -> anyhow::Result<()> {
    let runtime = storage::runtime_dir()?;
    let _lock = single_instance(&runtime.join("daemon.lock"))?;
    let signals = signals::Signals::install()?;
    let socket = ipc::socket_path()?;
    let _socket_cleanup = SocketCleanup(socket.clone());
    let listener = bind(&socket)?;
    let initial = config::load_live().unwrap_or_else(|error| {
        eprintln!("live.json not loaded ({error:#}); running with no live features");
        empty_config()
    });
    let daemon = Arc::new(Daemon::new(
        Arc::new(RealPlatform),
        initial,
        Box::new(config::load_live),
    ));
    eprintln!(
        "hyprdeck-corsair companion {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    );
    let _daemon_cleanup = DaemonCleanup(daemon.clone());
    let connections = Arc::new(AtomicUsize::new(0));
    daemon.sync_devices(devices::discover());
    let mut next_scan = Instant::now() + SCAN_PERIOD;
    let main_result = (|| -> Result<()> {
        'main: loop {
            let now = Instant::now();
            let mut fds = [
                util::pollfd(signals.fd(), libc::POLLIN),
                util::pollfd(listener.as_raw_fd(), libc::POLLIN),
            ];
            util::poll(&mut fds, Some(next_scan.saturating_duration_since(now)))
                .context("Waiting for events")?;
            if fds[0].revents != 0 {
                for signal in signals.take() {
                    if signal == libc::SIGHUP {
                        if let Err(error) = daemon.reload() {
                            eprintln!("reload failed: {error:#}");
                        }
                    } else {
                        eprintln!("signal {signal}: releasing keyboards and exiting");
                        break 'main;
                    }
                }
            }
            if fds[1].revents & libc::POLLIN != 0 {
                accept_all(&listener, &daemon, &connections);
            }
            if Instant::now() >= next_scan {
                daemon.sync_devices(devices::discover());
                next_scan = Instant::now() + SCAN_PERIOD;
            }
        }
        Ok(())
    })();
    daemon.shutdown(Instant::now() + SHUTDOWN_BUDGET);
    let _ = std::fs::remove_file(&socket);
    eprintln!("hyprdeck-corsair companion stopped");
    main_result
}

fn empty_config() -> LiveConfig {
    LiveConfig {
        version: config::LIVE_VERSION,
        devices: BTreeMap::new(),
    }
}

/// flock on the runtime lock file: one companion per user session.
fn single_instance(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))?;
    // SAFETY: plain flock on a live fd.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another hyprdeck-corsair companion is already running");
    }
    Ok(file)
}

/// Bind the socket (0600), replacing a stale socket file left by a crashed instance.
fn bind(path: &Path) -> Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("{} exists and is not a socket", path.display());
            }
            // We hold the instance lock, so nobody is serving it any more.
            std::fs::remove_file(path).with_context(|| format!("Removing {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("Inspecting {}", path.display())),
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("Listening on {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn accept_all<P: Platform>(
    listener: &UnixListener,
    daemon: &Arc<Daemon<P>>,
    active: &Arc<AtomicUsize>,
) {
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("accept failed: {error}");
                return;
            }
        };
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let mut stream = stream;
            let _ = std::io::Write::write_all(
                &mut stream,
                b"{\"ok\":false,\"error\":\"Companion busy\"}\n",
            );
            continue;
        }
        let daemon = daemon.clone();
        let admission = Admission(active.clone());
        let spawned = std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || {
                let _admission = admission;
                server::serve(stream, &*daemon);
            });
        if spawned.is_err() {
            eprintln!("could not start an IPC thread");
        }
    }
}

struct DaemonCleanup<P: Platform>(Arc<Daemon<P>>);
impl<P: Platform> Drop for DaemonCleanup<P> {
    fn drop(&mut self) {
        self.0.shutdown(Instant::now() + SHUTDOWN_BUDGET);
    }
}

struct SocketCleanup(std::path::PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        use std::os::unix::fs::FileTypeExt;
        if std::fs::symlink_metadata(&self.0).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

struct Admission(Arc<AtomicUsize>);
impl Drop for Admission {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Entry {
    keyboard: Keyboard,
    status: Arc<Mutex<DeviceStatus>>,
    hub: Arc<KeyHub>,
    mailbox: Mailbox,
    thread: JoinHandle<()>,
}

type Loader = Box<dyn Fn() -> Result<LiveConfig> + Send + Sync>;

/// Device registry shared by the main loop and IPC threads.
pub(crate) struct Daemon<P: Platform> {
    platform: Arc<P>,
    config: Mutex<LiveConfig>,
    load: Loader,
    reload_lock: Mutex<()>,
    devices: Mutex<BTreeMap<String, Entry>>,
    retiring: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

impl<P: Platform> Daemon<P> {
    pub(crate) fn new(platform: Arc<P>, config: LiveConfig, load: Loader) -> Self {
        Self {
            platform,
            config: Mutex::new(config),
            load,
            reload_lock: Mutex::new(()),
            devices: Mutex::new(BTreeMap::new()),
            retiring: Mutex::new(Vec::new()),
        }
    }

    fn spawn(&self, keyboard: Keyboard) -> Result<Entry> {
        let (mailbox, rx) = Mailbox::new()?;
        let status = Arc::new(Mutex::new(worker::idle_status(&keyboard.id)));
        let hub = Arc::new(KeyHub::default());
        let live = lock(&self.config)
            .devices
            .get(&keyboard.id)
            .cloned()
            .unwrap_or_default();
        let worker = Worker::new(WorkerSetup {
            platform: self.platform.clone(),
            keyboard: keyboard.clone(),
            live,
            rx,
            mailbox: mailbox.clone(),
            status: status.clone(),
            hub: hub.clone(),
        });
        let thread = std::thread::Builder::new()
            .name(format!(
                "dev-{}",
                keyboard.id.chars().take(10).collect::<String>()
            ))
            .spawn(move || worker.run())
            .context("Starting a device worker")?;
        Ok(Entry {
            keyboard,
            status,
            hub,
            mailbox,
            thread,
        })
    }

    /// Start workers for new keyboards, stop workers for keyboards that are gone or changed.
    pub(crate) fn sync_devices(&self, found: Vec<Keyboard>) {
        let mut devices = lock(&self.devices);
        let stale: Vec<String> = devices
            .iter()
            .filter(|(id, entry)| {
                entry.thread.is_finished()
                    || !found
                        .iter()
                        .any(|keyboard| &keyboard.id == *id && *keyboard == entry.keyboard)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            if let Some(entry) = devices.remove(&id) {
                eprintln!("{id}: keyboard removed or re-enumerated");
                entry.mailbox.send(Command::Shutdown);
                entry.hub.close();
                lock(&self.retiring).push(entry.thread);
            }
        }
        for keyboard in found {
            if devices.contains_key(&keyboard.id) {
                continue;
            }
            let id = keyboard.id.clone();
            eprintln!("{id}: {} found", keyboard.name);
            match self.spawn(keyboard) {
                Ok(entry) => {
                    devices.insert(id, entry);
                }
                Err(error) => eprintln!("{id}: {error:#}"),
            }
        }
        lock(&self.retiring).retain(|thread| !thread.is_finished());
    }

    /// Stop every worker; waits until `deadline` for them to release their keyboards.
    pub(crate) fn shutdown(&self, deadline: Instant) {
        let entries: Vec<Entry> = std::mem::take(&mut *lock(&self.devices))
            .into_values()
            .collect();
        let mut threads: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.retiring));
        for entry in entries {
            entry.mailbox.send(Command::Shutdown);
            entry.hub.close();
            threads.push(entry.thread);
        }
        while threads.iter().any(|thread| !thread.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for thread in threads {
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                eprintln!(
                    "a keyboard worker did not finish in time; `rescue` must attempt restoration"
                );
            }
        }
    }

    fn mailbox(&self, device: &str) -> Result<(Mailbox, DeviceStatus, Arc<KeyHub>)> {
        let devices = lock(&self.devices);
        let entry = devices
            .get(device)
            .with_context(|| format!("Keyboard {device} is not connected"))?;
        let status = lock(&entry.status).clone();
        Ok((entry.mailbox.clone(), status, entry.hub.clone()))
    }
}

impl<P: Platform> server::Backend for Daemon<P> {
    fn hello(&self) -> Vec<DeviceStatus> {
        lock(&self.devices)
            .values()
            .map(|entry| lock(&entry.status).clone())
            .collect()
    }

    fn xfer(&self, device: &str, payload: &[u8]) -> Result<Vec<u8>> {
        self.rpc(
            device,
            &ipc::Operation::Xfer {
                payload: payload.to_vec(),
            },
            Instant::now() + ipc::OPERATION_TIMEOUT,
        )
    }

    fn rpc(&self, device: &str, operation: &ipc::Operation, deadline: Instant) -> Result<Vec<u8>> {
        crate::bragi::check_deadline(deadline)?;
        let (mailbox, status, _) = self.mailbox(device)?;
        if !status.owned {
            bail!("Keyboard {device} is not owned by the live-mode companion");
        }
        let (reply, result) = sync_channel(1);
        if !mailbox.send(Command::Rpc {
            operation: operation.clone(),
            deadline,
            reply,
        }) {
            bail!("Keyboard {device} is going away — refresh");
        }
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()) + XFER_TIMEOUT)
            .context("The live-mode companion did not answer in time")?
    }

    fn reload(&self) -> Result<()> {
        let _serial = lock(&self.reload_lock);
        let fresh = (self.load)()
            .context("live.json could not be loaded; keeping the previous settings")?;
        let devices = lock(&self.devices);
        for (id, entry) in devices.iter() {
            entry.mailbox.send(Command::Reload(
                fresh.devices.get(id).cloned().unwrap_or_default(),
            ));
        }
        *lock(&self.config) = fresh;
        Ok(())
    }

    fn subscribe(&self, device: &str) -> Result<Receiver<Vec<String>>> {
        let (_, status, hub) = self.mailbox(device)?;
        if (!status.owned || status.mode != crate::bragi::MODE_SOFTWARE) && !status.grabbed {
            bail!("The live-mode companion is not reading keys from {device}");
        }
        Ok(hub.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bragi::{MODE_HARDWARE, MODE_SOFTWARE};
    use crate::config::{DeviceLive, DialMode};
    use crate::devices::LinkKind;
    use worker::tests::{FakePlatform, keyboard};

    fn config_with(mode: DialMode) -> LiveConfig {
        let mut config = empty_config();
        let mut live = DeviceLive::default();
        live.dial.mode = mode;
        config.devices.insert("k65plus-test".into(), live);
        config
    }

    fn wait(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn hotplug_reload_xfer_and_shutdown() {
        let platform = Arc::new(FakePlatform::new());
        let next = Arc::new(Mutex::new(config_with(DialMode::Volume)));
        let source = next.clone();
        let daemon = Daemon::new(
            platform.clone(),
            config_with(DialMode::Volume),
            Box::new(move || Ok(source.lock().unwrap().clone())),
        );
        assert!(daemon.hello().is_empty());
        daemon.sync_devices(vec![keyboard(LinkKind::Receiver)]);
        assert_eq!(daemon.hello().len(), 1);
        assert!(!daemon.hello()[0].owned, "Volume mode needs no live mode");
        assert!(daemon.xfer("k65plus-test", &[0x02, 0x0f]).is_err());
        assert!(daemon.subscribe("k65plus-test").is_err());
        assert!(daemon.xfer("missing", &[0x02, 0x0f]).is_err());

        *next.lock().unwrap() = config_with(DialMode::Media);
        daemon.reload().unwrap();
        wait("ownership", || daemon.hello()[0].owned);
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_SOFTWARE);
        let reply = daemon.xfer("k65plus-test", &[0x02, 0x02]).unwrap();
        assert_eq!(reply[2], 0);
        assert!(
            daemon
                .xfer("k65plus-test", &[0x0d, 0x01, 0x02, 0x00])
                .is_err()
        );
        let keys = daemon.subscribe("k65plus-test").unwrap();
        assert_eq!(
            keys.recv_timeout(Duration::from_secs(1)).unwrap(),
            Vec::<String>::new()
        );
        platform.report(&[0x01, 0x02, 0x10]); // bit 4 = "a"
        assert_eq!(
            keys.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec!["a".to_string()]
        );

        // A malformed live.json keeps the previous settings.
        let failing = Daemon::new(
            platform.clone(),
            empty_config(),
            Box::new(|| bail!("Malformed")),
        );
        assert!(failing.reload().is_err());

        // Unplug: the worker releases the keyboard.
        daemon.sync_devices(Vec::new());
        assert!(daemon.hello().is_empty());
        wait("release after unplug", || {
            platform.keyboard.with(|k| k.mode) == MODE_HARDWARE
        });
        while let Ok(frame) = keys.recv_timeout(Duration::from_secs(2)) {
            assert!(frame.is_empty());
        }

        // Plug back in, then a full shutdown restores hardware mode within the budget.
        daemon.sync_devices(vec![keyboard(LinkKind::Receiver)]);
        wait("ownership again", || {
            daemon.hello().first().is_some_and(|d| d.owned)
        });
        let started = Instant::now();
        daemon.shutdown(Instant::now() + SHUTDOWN_BUDGET);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
        assert!(daemon.hello().is_empty());
    }

    #[test]
    fn single_instance_lock_and_stale_socket() {
        let tmp = crate::devices::tests::TempDir::new("daemon-lock");
        let first = single_instance(&tmp.0.join("daemon.lock")).unwrap();
        assert!(single_instance(&tmp.0.join("daemon.lock")).is_err());
        drop(first);
        assert!(single_instance(&tmp.0.join("daemon.lock")).is_ok());
        let path = tmp.short_path("daemon.sock");
        drop(UnixListener::bind(&path).unwrap()); // stale socket file
        let listener = bind(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        std::fs::write(tmp.0.join("not-a-socket"), b"x").unwrap();
        assert!(bind(&tmp.short_path("not-a-socket")).is_err());
    }
}

#[cfg(test)]
mod admission_regressions {
    use super::*;
    #[test]
    fn admission_is_returned_when_spawn_closure_drops_or_panics() {
        let active = Arc::new(AtomicUsize::new(1));
        let admission = Admission(active.clone());
        let not_spawned = move || {
            let _admission = admission;
        };
        drop(not_spawned);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        active.store(1, Ordering::SeqCst);
        let admission = Admission(active.clone());
        assert!(
            std::thread::spawn(move || {
                let _admission = admission;
                panic!("mock connection panic");
            })
            .join()
            .is_err()
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }
}

#[cfg(test)]
mod reload_serialization_regressions {
    use super::*;
    use crate::config::{DeviceLive, DialMode};
    use worker::tests::FakePlatform;
    #[test]
    fn load_and_apply_are_one_serialized_order_not_stale_reapplication() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let (entered, wait) = std::sync::mpsc::sync_channel(1);
        let daemon = Arc::new(Daemon::new(
            Arc::new(FakePlatform::new()),
            empty_config(),
            Box::new(move || {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    entered.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(40));
                }
                let mut config = empty_config();
                let mut live = DeviceLive::default();
                live.dial.mode = if n == 0 {
                    DialMode::Media
                } else {
                    DialMode::Zoom
                };
                config.devices.insert("k65-test".into(), live);
                Ok(config)
            }),
        ));
        let first = daemon.clone();
        let a = std::thread::spawn(move || first.reload().unwrap());
        wait.recv().unwrap();
        let second = daemon.clone();
        let b = std::thread::spawn(move || second.reload().unwrap());
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(
            lock(&daemon.config).devices["k65-test"].dial.mode,
            DialMode::Zoom
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod atomic_server_regressions {
    use super::*;
    use crate::config::{DeviceLive, DialMode};
    use worker::tests::{FakePlatform, keyboard};
    #[test]
    fn serialized_atomic_rpc_routes_through_daemon_worker_and_returns_no_handle() {
        let platform = Arc::new(FakePlatform::new());
        let mut config = empty_config();
        let mut live = DeviceLive::default();
        live.dial.mode = DialMode::Media;
        config.devices.insert("k65plus-test".into(), live);
        let daemon = Daemon::new(platform.clone(), config, Box::new(|| Ok(empty_config())));
        daemon.sync_devices(vec![keyboard(crate::devices::LinkKind::Usb)]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !daemon.hello()[0].owned && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let line = r#"{"op":"rpc","device":"k65plus-test","timeout_ms":1000,"operation":{"kind":"read-resource","resource":28000}}"#;
        let server::Outcome::Reply(response) = server::handle_line(line, &daemon) else {
            panic!("stream")
        };
        assert!(response.ok, "{:?}", response.error);
        assert_eq!(response.reply.as_deref(), Some("7ea00203"));
        assert!(platform.keyboard.with(|k| k.open.is_empty()));
        let denied = r#"{"op":"rpc","device":"k65plus-test","timeout_ms":1000,"operation":{"kind":"xfer","payload":[8,1]}}"#;
        let server::Outcome::Reply(response) = server::handle_line(denied, &daemon) else {
            panic!("stream")
        };
        assert!(!response.ok);
        daemon.shutdown(Instant::now() + SHUTDOWN_BUDGET);
        assert_eq!(
            platform.keyboard.with(|k| k.mode),
            crate::bragi::MODE_HARDWARE
        );
    }
}
