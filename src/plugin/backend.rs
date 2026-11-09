//! Everything the plugin touches outside its own memory, behind one trait so tests can run the
//! whole request path against fake keyboards, a fake daemon and a fake `systemctl`.

use crate::bragi::{Bragi, Hidraw, Reply, Transport};
use crate::devices::{self, Keyboard};
use crate::ipc::{self, DaemonTransport, DeviceStatus};
use crate::storage;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hidraw lock wait for plugin requests (DESIGN §6).
pub const LOCK_WAIT: Duration = Duration::from_millis(750);
/// Receiver bitmask of connected children (GET 0x36 on target 0x08).
const RECEIVER_TARGET: u8 = 0x08;
/// Longest a single `systemctl --user` call may take.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(8);

/// A boxed transport usable with the generic `Bragi<T>`.
pub struct AnyTransport(pub Box<dyn Transport>);

impl Transport for AnyTransport {
    fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
        self.0.xfer(payload)
    }
    fn xfer_until(&mut self, payload: &[u8], deadline: Instant) -> Result<Reply> {
        self.0.xfer_until(payload, deadline)
    }
    fn read_resource_until(&mut self, resource: u16, deadline: Instant) -> Option<Result<Vec<u8>>> {
        self.0.read_resource_until(resource, deadline)
    }
    fn write_resource_until(
        &mut self,
        resource: u16,
        declared_len: u32,
        bytes: &[u8],
        deadline: Instant,
    ) -> Option<Result<()>> {
        self.0
            .write_resource_until(resource, declared_len, bytes, deadline)
    }
    fn sleep_until(&mut self, duration: Duration, deadline: Instant) -> Result<()> {
        self.0.sleep_until(duration, deadline)
    }
    fn sleep(&mut self, duration: Duration) {
        self.0.sleep(duration);
    }
}

pub type Session = Bragi<AnyTransport>;

pub trait Backend {
    /// Connected supported keyboards (sysfs discovery).
    fn keyboards(&self) -> Vec<Keyboard>;
    /// Recognised but unsupported Corsair devices.
    fn unsupported(&self) -> Vec<(u16, String)>;
    /// Daemon `hello`: Ok(None) when the companion is not running.
    fn daemon_hello(&self) -> Result<Option<Vec<DeviceStatus>>>;
    /// Ask the companion to reload live.json; Ok(false) when it is not running.
    fn daemon_reload(&self) -> Result<bool>;
    /// Command transport for the keyboard: through the daemon when it owns the device.
    fn open(&self, keyboard: &Keyboard, via_daemon: bool) -> Result<Box<dyn Transport>>;
    /// Receiver session (target 0x08); only called for receiver links.
    fn open_receiver(&self, keyboard: &Keyboard) -> Result<Box<dyn Transport>>;
    /// Whether the opt-in input rule looks installed (uinput accessible).
    fn input_rule_installed(&self) -> bool;
    /// Run `systemctl --user <args>`.
    fn systemctl(&self, args: &[&str]) -> Result<()>;
    /// `~/.config/systemd/user/hyprdeck-corsair.service`
    fn unit_path(&self) -> Result<PathBuf>;
    /// Canonical path of the running executable.
    fn exe(&self) -> Result<PathBuf>;
    /// Seconds since the UNIX epoch.
    fn now(&self) -> u64 {
        storage::unix_now()
    }
}

/// Open an allowlisted session on `keyboard`.
pub fn session(backend: &dyn Backend, keyboard: &Keyboard, via_daemon: bool) -> Result<Session> {
    let transport = backend.open(keyboard, via_daemon)?;
    Ok(Bragi::new(AnyTransport(transport), keyboard.model))
}

/// The real system.
pub struct System;

impl Backend for System {
    fn keyboards(&self) -> Vec<Keyboard> {
        devices::discover()
    }
    fn unsupported(&self) -> Vec<(u16, String)> {
        devices::unsupported_corsair_devices()
    }
    fn daemon_hello(&self) -> Result<Option<Vec<DeviceStatus>>> {
        ipc::hello()
    }
    fn daemon_reload(&self) -> Result<bool> {
        ipc::reload()
    }
    fn open(&self, keyboard: &Keyboard, via_daemon: bool) -> Result<Box<dyn Transport>> {
        if via_daemon {
            return Ok(Box::new(DaemonTransport {
                device: keyboard.id.clone(),
            }));
        }
        Ok(Box::new(Hidraw::open_keyboard(keyboard, LOCK_WAIT)?))
    }
    fn open_receiver(&self, keyboard: &Keyboard) -> Result<Box<dyn Transport>> {
        Ok(Box::new(Hidraw::open(
            &keyboard.cmd_node,
            keyboard.vid,
            keyboard.pid,
            keyboard.frame_len,
            RECEIVER_TARGET,
            LOCK_WAIT,
        )?))
    }
    fn input_rule_installed(&self) -> bool {
        let dev =
            std::env::var_os(devices::ENV_DEV).map_or_else(|| PathBuf::from("/dev"), PathBuf::from);
        access_rw(&dev.join("uinput"))
    }
    fn systemctl(&self, args: &[&str]) -> Result<()> {
        run_systemctl(args)
    }
    fn unit_path(&self) -> Result<PathBuf> {
        let config = storage::config_dir()?;
        let base = config
            .parent()
            .context("XDG config directory has no parent")?;
        Ok(base.join("systemd/user").join(super::service::UNIT_NAME))
    }
    fn exe(&self) -> Result<PathBuf> {
        let exe = std::env::current_exe().context("Locating the hyprdeck-corsair executable")?;
        exe.canonicalize()
            .with_context(|| format!("Resolving {}", exe.display()))
    }
}

/// access(2) R_OK|W_OK without opening the node.
fn access_rw(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: valid NUL-terminated path; access has no other preconditions.
    unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let mut child = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Running systemctl --user (is systemd available?)")?;
    let deadline = crate::bragi::bounded_deadline(SYSTEMCTL_TIMEOUT);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("systemctl --user {} did not finish in time", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.by_ref().take(4096).read_to_string(&mut stderr);
        }
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            stderr.trim().lines().next().unwrap_or("unknown error")
        );
    }
    Ok(())
}
