//! Corsair Bragi protocol over raw hidraw. See docs/DESIGN.md §2–§3.
//!
//! OWNER: foundation. Shared interface: signatures below are a contract used by
//! `plugin`, `daemon`, `ipc` and `rescue`.

use crate::devices::{Keyboard, Model};
use anyhow::{Context, Result, bail};
use std::cell::RefCell;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub const CMD_SET: u8 = 0x01;
pub const CMD_GET: u8 = 0x02;
pub const CMD_CLOSE: u8 = 0x05;
pub const CMD_WRITE: u8 = 0x06;
pub const CMD_WRITE_MORE: u8 = 0x07;
pub const CMD_READ: u8 = 0x08;
pub const CMD_PROBE: u8 = 0x09;
pub const CMD_OPEN: u8 = 0x0D;
pub const CMD_PING: u8 = 0x12;

pub const PROP_POLLING: u8 = 0x01;
pub const PROP_BRIGHTNESS: u8 = 0x02;
pub const PROP_MODE: u8 = 0x03;
pub const PROP_SLEEP_MS: u8 = 0x0E;
pub const PROP_BATTERY: u8 = 0x0F;
pub const PROP_CHARGE: u8 = 0x10;
pub const PROP_VID: u8 = 0x11;
pub const PROP_PID: u8 = 0x12;
pub const PROP_APP_FW: u8 = 0x13;
pub const PROP_BOOT_FW: u8 = 0x14;
pub const PROP_RADIO_FW: u8 = 0x15;
pub const PROP_CHILDREN: u8 = 0x36;
pub const PROP_LAYOUT: u8 = 0x41;
pub const PROP_LOCK_ENABLE: u8 = 0x45;
pub const PROP_LOCK_MASK: u8 = 0x4A;
pub const PROP_BRIGHTNESS_2: u8 = 0x6D;
pub const PROP_MAX_POLLING: u8 = 0x96;

pub const MODE_HARDWARE: u8 = 1;
pub const MODE_SOFTWARE: u8 = 2;

pub const RES_K65_EFFECT: u16 = 0x6D60;
pub const RES_DIRECT_LIGHTING: u16 = 0x0022;

pub const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

/// Settle time between opening a resource handle and writing to it (DESIGN §2 framing).
pub const OPEN_SETTLE: Duration = Duration::from_millis(500);
/// Pause between continuation packets on 1024-byte ("jumbo") devices.
pub const JUMBO_PACE: Duration = Duration::from_millis(2);
/// Pause before the single retry a read (GET) may do after a timeout (DESIGN §3 rule 6).
pub const READ_RETRY_DELAY: Duration = Duration::from_millis(200);
/// Largest resource we are willing to read back.
const MAX_RESOURCE_LEN: usize = 64 * 1024;
/// Handle numbers the host may use (0 = lighting, 1 = generic, 2 = second lighting).
const MAX_HANDLE: u8 = 2;
/// Sleep timeout bounds accepted by SET 0x0E (1 min .. 60 min, the plugin's choices).
const SLEEP_MS_RANGE: std::ops::RangeInclusive<u32> = 60_000..=3_600_000;

/// Device status byte (`reply[2]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    InvalidValue,
    Failed,
    Unsupported,
    /// 6: the keyboard behind the receiver is asleep or disconnected.
    Offline,
    /// 7: not allowed in the current mode (e.g. SET in hardware mode).
    NotAllowed,
    Other(u8),
}

impl Status {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Ok,
            1 => Self::InvalidValue,
            3 => Self::Failed,
            5 => Self::Unsupported,
            6 => Self::Offline,
            7 => Self::NotAllowed,
            other => Self::Other(other),
        }
    }
}

/// A non-Ok status returned by the keyboard, with user-facing text. Downcast from
/// `anyhow::Error` (see [`is_offline`]) to react to specific statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusError {
    pub cmd: u8,
    pub arg: u8,
    pub status: Status,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (cmd, arg) = (self.cmd, self.arg);
        match self.status {
            Status::Offline => write!(f, "Keyboard is asleep or disconnected — press a key on it"),
            Status::NotAllowed => write!(
                f,
                "Not allowed while the keyboard is in hardware mode (command {cmd:02x} {arg:02x})"
            ),
            Status::InvalidValue => {
                write!(
                    f,
                    "Keyboard rejected the value (command {cmd:02x} {arg:02x})"
                )
            }
            Status::Failed => write!(
                f,
                "Keyboard reported a failure or busy handle (command {cmd:02x} {arg:02x})"
            ),
            Status::Unsupported => {
                write!(
                    f,
                    "Not supported by this keyboard (command {cmd:02x} {arg:02x})"
                )
            }
            Status::Ok => write!(f, "Keyboard reported success (command {cmd:02x} {arg:02x})"),
            Status::Other(byte) => write!(
                f,
                "Keyboard returned status 0x{byte:02x} (command {cmd:02x} {arg:02x})"
            ),
        }
    }
}

impl std::error::Error for StatusError {}

/// No matching reply arrived within [`REPLY_TIMEOUT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyTimeout {
    pub cmd: u8,
}

impl fmt::Display for ReplyTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "No reply from the keyboard within {} ms (command {:02x})",
            REPLY_TIMEOUT.as_millis(),
            self.cmd
        )
    }
}

impl std::error::Error for ReplyTimeout {}

/// True when `error` (anywhere in its chain) is status 6: keyboard asleep or disconnected.
pub fn is_offline(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<StatusError>())
        .any(|status| status.status == Status::Offline)
}

/// True when `error` (anywhere in its chain) is a reply timeout.
pub fn is_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ReplyTimeout>())
}

/// Polling rate code (prop 0x01) -> Hz.
pub fn polling_hz(code: u32) -> Option<u32> {
    match code {
        1..=7 => Some(125 << (code - 1)),
        _ => None,
    }
}

/// Hz -> polling rate code (prop 0x01).
pub fn polling_code(hz: u32) -> Option<u8> {
    (1..=7u8).find(|code| polling_hz(u32::from(*code)) == Some(hz))
}

/// One reply report as read from the command node (64 bytes for 64-byte devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub raw: Vec<u8>,
}

impl Reply {
    pub fn status(&self) -> Status {
        self.raw
            .get(2)
            .map_or(Status::Other(0xff), |byte| Status::from_byte(*byte))
    }
    /// Little-endian value starting at raw[3], up to 4 bytes (missing bytes are 0).
    pub fn value_u32(&self) -> u32 {
        let mut bytes = [0u8; 4];
        for (slot, byte) in bytes.iter_mut().zip(self.data()) {
            *slot = *byte;
        }
        u32::from_le_bytes(bytes)
    }
    /// raw[3..]
    pub fn data(&self) -> &[u8] {
        self.raw.get(3..).unwrap_or(&[])
    }
}

/// A request has one monotonic work deadline and a separate bounded cleanup reserve.
#[derive(Clone)]
pub struct RequestBudget {
    pub work: Instant,
    pub cleanup: Instant,
    pub cancelled: Arc<AtomicBool>,
}
thread_local! {
    static REQUEST_BUDGET: RefCell<Option<RequestBudget>> = const { RefCell::new(None) };
}
pub fn with_request_budget<R>(budget: RequestBudget, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<RequestBudget>);
    impl Drop for Reset {
        fn drop(&mut self) {
            REQUEST_BUDGET.with(|b| *b.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(REQUEST_BUDGET.with(|b| b.replace(Some(budget))));
    f()
}
pub fn work_deadline() -> Option<Instant> {
    REQUEST_BUDGET.with(|b| b.borrow().as_ref().map(|b| b.work))
}
pub fn bounded_deadline(limit: Duration) -> Instant {
    work_deadline().map_or_else(|| Instant::now() + limit, |d| d.min(Instant::now() + limit))
}
fn cleanup_deadline(work: Option<Instant>) -> Option<Instant> {
    REQUEST_BUDGET
        .with(|b| b.borrow().as_ref().map(|b| b.cleanup))
        .or_else(|| work.map(|d| d + Duration::from_secs(3)))
}
pub fn check_deadline(deadline: Instant) -> Result<()> {
    let cancelled = REQUEST_BUDGET.with(|b| {
        b.borrow()
            .as_ref()
            .is_some_and(|b| deadline <= b.work && b.cancelled.load(Ordering::Acquire))
    });
    if cancelled || Instant::now() >= deadline {
        bail!("Request deadline expired; unsent work cancelled");
    }
    Ok(())
}
#[derive(Debug)]
pub struct Disconnected;
impl fmt::Display for Disconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Keyboard disconnected — refresh")
    }
}
impl std::error::Error for Disconnected {}
pub fn is_terminal_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.is::<Disconnected>()
            || e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.raw_os_error(),
                    Some(libc::ENODEV | libc::ENXIO | libc::EIO | libc::EBADF)
                )
            })
    })
}

/// Moves one Bragi command (payload = `[cmd, args…]`, without report id and target)
/// and returns the matching reply. Implementations add report id, target and padding.
pub trait Transport {
    fn xfer(&mut self, payload: &[u8]) -> Result<Reply>;
    /// Real transports must bound all waits by this absolute deadline. Default keeps mocks compatible.
    fn xfer_until(&mut self, payload: &[u8], deadline: Instant) -> Result<Reply> {
        check_deadline(deadline)?;
        self.xfer(payload)
    }
    /// Atomic resource hooks: None means the local Bragi session implements the transaction.
    fn read_resource_until(
        &mut self,
        _resource: u16,
        _deadline: Instant,
    ) -> Option<Result<Vec<u8>>> {
        None
    }
    fn write_resource_until(
        &mut self,
        _resource: u16,
        _declared_len: u32,
        _bytes: &[u8],
        _deadline: Instant,
    ) -> Option<Result<()>> {
        None
    }
    fn sleep_until(&mut self, duration: Duration, deadline: Instant) -> Result<()> {
        check_deadline(deadline)?;
        self.sleep(duration.min(deadline.saturating_duration_since(Instant::now())));
        check_deadline(deadline)
    }
    /// Wait between protocol steps (settle after open, jumbo pacing, read retry).
    /// Mock transports override this to keep tests fast.
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// HIDIOCGRAWINFO = _IOR('H', 0x03, struct hidraw_devinfo)
const HIDIOCGRAWINFO: libc::c_ulong = 0x8008_4803;

#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

/// Exclusive hidraw session on a command node (IF1).
pub struct Hidraw {
    file: File,
    frame_len: usize,
    target: u8,
}

impl Hidraw {
    /// Open `node` (O_RDWR|O_NONBLOCK|O_CLOEXEC), verify vid/pid via HIDIOCGRAWINFO,
    /// and take flock(LOCK_EX|LOCK_NB) retried for up to `lock_wait`.
    /// Errors must tell the user about the udev rule on EACCES and about "busy" on lock timeout.
    pub fn open(
        node: &Path,
        vid: u16,
        pid: u16,
        frame_len: usize,
        target: u8,
        lock_wait: Duration,
    ) -> Result<Self> {
        if !(8..=4096).contains(&frame_len) {
            bail!("Invalid Bragi frame length {frame_len}");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(node)
            .map_err(|error| match error.kind() {
                ErrorKind::PermissionDenied => anyhow::anyhow!(
                    "Permission denied opening {} — install the hyprdeck-corsair udev rule \
                     (70-hyprdeck-corsair.rules) and replug the keyboard",
                    node.display()
                ),
                ErrorKind::NotFound => {
                    anyhow::anyhow!("Keyboard node {} disappeared — refresh", node.display())
                }
                _ => anyhow::Error::new(error).context(format!("Opening {}", node.display())),
            })?;
        let mut info = HidrawDevinfo::default();
        // SAFETY: HidrawDevinfo has the Linux hidraw_devinfo layout and `file` is a live fd.
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), HIDIOCGRAWINFO, &mut info) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("Reading HID identity of {}", node.display()));
        }
        let (found_vid, found_pid) = (info.vendor as u16, info.product as u16);
        if (found_vid, found_pid) != (vid, pid) {
            bail!(
                "{} is {found_vid:04x}:{found_pid:04x}, expected {vid:04x}:{pid:04x} — refresh",
                node.display()
            );
        }
        let deadline = bounded_deadline(lock_wait);
        loop {
            // SAFETY: plain flock on a live fd.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.kind() != ErrorKind::Interrupted
            {
                return Err(error).with_context(|| format!("Locking {}", node.display()));
            }
            if Instant::now() >= deadline {
                bail!(
                    "Keyboard busy — another hyprdeck-corsair process is using it; refresh in a moment"
                );
            }
            check_deadline(deadline)?;
            std::thread::sleep(
                Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        Ok(Self {
            file,
            frame_len,
            target,
        })
    }

    /// Convenience: open the keyboard's command node with its frame length and target.
    pub fn open_keyboard(keyboard: &Keyboard, lock_wait: Duration) -> Result<Self> {
        Self::open(
            &keyboard.cmd_node,
            keyboard.vid,
            keyboard.pid,
            keyboard.frame_len,
            keyboard.target,
            lock_wait,
        )
    }

    /// Discard any input reports already queued (stale replies, late answers).
    fn drain(&mut self, deadline: Instant) -> Result<()> {
        let mut buffer = vec![0u8; self.frame_len + 1];
        for _ in 0..64 {
            check_deadline(deadline)?;
            match self.file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        Ok(())
    }
}

impl Transport for Hidraw {
    fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
        self.xfer_until(payload, bounded_deadline(REPLY_TIMEOUT))
    }
    fn xfer_until(&mut self, payload: &[u8], deadline: Instant) -> Result<Reply> {
        check_deadline(deadline)?;
        let deadline = deadline.min(Instant::now() + REPLY_TIMEOUT);
        let frame = encode_frame(self.target, payload, self.frame_len)?;
        let cmd = payload[0];
        self.drain(deadline)?;
        let written = loop {
            check_deadline(deadline)?;
            match self.file.write(&frame) {
                Ok(written) => break written,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("Writing to the keyboard"),
            }
        };
        if written != frame.len() {
            bail!(
                "Short write to the keyboard ({written} of {} bytes)",
                frame.len()
            );
        }
        let mut buffer = vec![0u8; self.frame_len.max(64) + 1];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(ReplyTimeout { cmd }.into());
            }
            let wait = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for a live fd.
            let ready = unsafe { libc::poll(&mut pollfd, 1, wait) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("Waiting for the keyboard");
            }
            if ready == 0 {
                continue;
            }
            if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(Disconnected.into());
            }
            match self.file.read(&mut buffer) {
                Ok(read) if read >= 3 && buffer[1] == cmd && buffer[0] & 7 == self.target & 7 => {
                    return Ok(Reply {
                        raw: buffer[..read].to_vec(),
                    });
                }
                Ok(0) => return Err(Disconnected.into()),
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(error) => return Err(error).context("Reading from the keyboard"),
            }
        }
    }
}

/// Encode a full output report: `[0x00, target, payload…, zero padding]`, length `frame_len + 1`.
pub fn encode_frame(target: u8, payload: &[u8], frame_len: usize) -> Result<Vec<u8>> {
    if payload.is_empty() {
        bail!("Empty Bragi payload");
    }
    if payload.len() + 1 > frame_len {
        bail!(
            "Bragi payload of {} bytes does not fit a {frame_len}-byte frame",
            payload.len()
        );
    }
    let mut frame = vec![0u8; frame_len + 1];
    frame[1] = target;
    frame[2..2 + payload.len()].copy_from_slice(payload);
    Ok(frame)
}

fn get_props(model: Model) -> &'static [u8] {
    match model {
        Model::K65PlusWireless => &[
            PROP_POLLING,
            PROP_BRIGHTNESS,
            PROP_MODE,
            PROP_SLEEP_MS,
            PROP_BATTERY,
            PROP_CHARGE,
            PROP_VID,
            PROP_PID,
            PROP_APP_FW,
            PROP_BOOT_FW,
            PROP_RADIO_FW,
            PROP_CHILDREN,
            PROP_LAYOUT,
            PROP_LOCK_ENABLE,
            PROP_LOCK_MASK,
            PROP_MAX_POLLING,
        ],
        Model::K100Rgb => &[
            PROP_POLLING,
            PROP_BRIGHTNESS,
            PROP_MODE,
            PROP_VID,
            PROP_PID,
            PROP_APP_FW,
            PROP_BOOT_FW,
            PROP_RADIO_FW,
            PROP_LAYOUT,
            PROP_LOCK_ENABLE,
            PROP_LOCK_MASK,
            PROP_BRIGHTNESS_2,
            PROP_MAX_POLLING,
        ],
    }
}

fn max_polling_code(model: Model) -> u32 {
    match model {
        Model::K65PlusWireless => 4,
        Model::K100Rgb => 7,
    }
}

/// Resources that may be opened for reading.
fn readable_resources(model: Model) -> &'static [u16] {
    match model {
        Model::K65PlusWireless => &[RES_K65_EFFECT],
        Model::K100Rgb => &[],
    }
}

/// Resources that may be opened for writing.
fn writable_resources(model: Model) -> &'static [u16] {
    match model {
        Model::K65PlusWireless => &[RES_K65_EFFECT, RES_DIRECT_LIGHTING],
        Model::K100Rgb => &[RES_DIRECT_LIGHTING],
    }
}

/// Byte width of each settable property.
fn set_width(prop: u8) -> usize {
    match prop {
        PROP_BRIGHTNESS | PROP_BRIGHTNESS_2 => 2,
        PROP_SLEEP_MS => 4,
        _ => 1,
    }
}

fn check_set(model: Model, prop: u8, value: u32) -> Result<()> {
    let ok = match (model, prop) {
        (_, PROP_POLLING) => (1..=max_polling_code(model)).contains(&value),
        (_, PROP_BRIGHTNESS) => value <= 1000,
        (_, PROP_MODE) => value == u32::from(MODE_HARDWARE) || value == u32::from(MODE_SOFTWARE),
        (Model::K65PlusWireless, PROP_SLEEP_MS) => SLEEP_MS_RANGE.contains(&value),
        (_, PROP_LOCK_ENABLE) => value <= 1,
        (_, PROP_LOCK_MASK) => value <= 0x0F,
        (Model::K100Rgb, PROP_BRIGHTNESS_2) => value <= 1000,
        _ => bail!("SET of property 0x{prop:02x} is not allowed on {model:?}"),
    };
    if !ok {
        bail!("SET of property 0x{prop:02x} to {value} is not allowed on {model:?}");
    }
    Ok(())
}

fn check_handle(payload: &[u8]) -> Result<u8> {
    let handle = *payload.get(1).context("Bragi handle missing")?;
    if handle > MAX_HANDLE {
        bail!("Bragi handle {handle} is not allowed");
    }
    Ok(handle)
}

/// Returns Err if `payload` is not allowed for `model` (DESIGN §3 rule 1).
/// Checked by `Bragi` before every write and by the daemon for proxied `xfer`s.
pub fn check_allowed(model: Model, payload: &[u8]) -> Result<()> {
    let Some(&cmd) = payload.first() else {
        bail!("Empty Bragi payload")
    };
    if payload.len() + 1 > model.frame_len() {
        bail!(
            "Bragi payload of {} bytes is too long for {model:?}",
            payload.len()
        );
    }
    let zero_tail = |from: usize| {
        payload
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .all(|byte| *byte == 0)
    };
    match cmd {
        CMD_GET => {
            let prop = *payload.get(1).context("GET without a property")?;
            if !get_props(model).contains(&prop) || !zero_tail(2) {
                bail!("GET of property 0x{prop:02x} is not allowed on {model:?}");
            }
        }
        CMD_SET => {
            let prop = *payload.get(1).context("SET without a property")?;
            if payload.len() < 3 + set_width(prop)
                || payload[2] != 0
                || !zero_tail(3 + set_width(prop))
            {
                bail!("Malformed SET of property 0x{prop:02x}");
            }
            let mut bytes = [0u8; 4];
            for (slot, byte) in bytes.iter_mut().zip(&payload[3..]) {
                *slot = *byte;
            }
            check_set(model, prop, u32::from_le_bytes(bytes))?;
        }
        CMD_OPEN => {
            check_handle(payload)?;
            if payload.len() < 4 || !zero_tail(4) {
                bail!("Malformed resource open");
            }
            let resource = u16::from_le_bytes([payload[2], payload[3]]);
            if !readable_resources(model).contains(&resource)
                && !writable_resources(model).contains(&resource)
            {
                bail!("Opening resource 0x{resource:04x} is not allowed on {model:?}");
            }
        }
        CMD_PROBE | CMD_READ => {
            check_handle(payload)?;
            if !zero_tail(2) {
                bail!("Malformed resource command 0x{cmd:02x}");
            }
        }
        CMD_WRITE | CMD_WRITE_MORE => {
            check_handle(payload)?;
        }
        CMD_CLOSE => {
            if payload.get(1) != Some(&1) {
                bail!("Malformed handle close");
            }
            let handle = *payload.get(2).context("Close without a handle")?;
            if handle > MAX_HANDLE || !zero_tail(3) {
                bail!("Closing handle {handle} is not allowed");
            }
        }
        CMD_PING => {
            if !zero_tail(1) {
                bail!("Malformed ping");
            }
        }
        other => bail!("Bragi command 0x{other:02x} is not allowed"),
    }
    Ok(())
}

fn ensure_ok(payload: &[u8], reply: &Reply) -> Result<()> {
    match reply.status() {
        Status::Ok => Ok(()),
        status => Err(StatusError {
            cmd: payload[0],
            arg: payload.get(1).copied().unwrap_or(0),
            status,
        }
        .into()),
    }
}

/// High-level, allowlisted operations on one keyboard.
pub struct Bragi<T: Transport> {
    pub transport: T,
    pub model: Model,
    deadline: Option<Instant>,
    /// Includes uncertain OPENs. A binding is never inferred from the device's stale handles.
    bindings: [Option<u16>; 3],
    confirmed: [bool; 3],
}

impl<T: Transport> Bragi<T> {
    pub fn new(transport: T, model: Model) -> Self {
        Self {
            transport,
            model,
            deadline: work_deadline(),
            bindings: [None; 3],
            confirmed: [false; 3],
        }
    }
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    fn until(&self) -> Instant {
        self.deadline
            .unwrap_or_else(|| bounded_deadline(Duration::from_secs(10)))
    }
    fn pause(&mut self, duration: Duration) -> Result<()> {
        self.transport.sleep_until(duration, self.until())
    }
    /// Temporarily use the reserved cleanup time, never extending it per packet.
    fn cleanup<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let old = self.deadline;
        self.deadline =
            cleanup_deadline(old).or_else(|| Some(Instant::now() + Duration::from_secs(3)));
        let out = f(self);
        self.deadline = old;
        out
    }
    pub fn has_bindings(&self) -> bool {
        self.bindings.iter().any(Option::is_some)
    }
    pub fn cleanup_handles(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for h in 0..=MAX_HANDLE {
            if self.bindings[h as usize].is_some()
                && let Err(e) = self.close_handle(h)
            {
                failures.push(format!("handle {h}: {e:#}"));
            }
        }
        if !failures.is_empty() {
            bail!("Resource cleanup failed: {}", failures.join("; "));
        }
        Ok(())
    }
    /// Allowlist-checked raw transfer.
    pub fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
        check_allowed(self.model, payload)?;
        match payload[0] {
            CMD_READ | CMD_PROBE | CMD_WRITE | CMD_WRITE_MORE => {
                if !self.confirmed[payload[1] as usize] {
                    bail!("Unknown or uncertain resource binding; close before reuse");
                }
                let resource = self.bindings[payload[1] as usize]
                    .context("Unknown resource binding; use an atomic resource operation")?;
                let allowed = match payload[0] {
                    CMD_READ => readable_resources(self.model).contains(&resource),
                    CMD_WRITE | CMD_WRITE_MORE => {
                        writable_resources(self.model).contains(&resource)
                    }
                    _ => {
                        readable_resources(self.model).contains(&resource)
                            || writable_resources(self.model).contains(&resource)
                    }
                };
                if !allowed {
                    bail!("Resource operation is not allowed on 0x{resource:04x}");
                }
            }
            CMD_OPEN => {
                let slot = &mut self.bindings[payload[1] as usize];
                if slot.is_some() {
                    bail!("Resource handle already bound; close it before reuse");
                }
                *slot = Some(u16::from_le_bytes([payload[2], payload[3]]));
                self.confirmed[payload[1] as usize] = false;
            }
            _ => {}
        }
        // Unknown OPEN and busy (status 3) outcomes stay tracked; other definite
        // rejections did not create a binding. Only an evidenced not-open CLOSE clears it.
        let result = self.transport.xfer_until(payload, self.until());
        if payload[0] == CMD_OPEN {
            match result.as_ref().map(Reply::status) {
                Ok(Status::Ok) => self.confirmed[payload[1] as usize] = true,
                Ok(Status::Failed) | Err(_) => {}
                Ok(_) => self.bindings[payload[1] as usize] = None,
            }
        }
        if payload[0] == CMD_CLOSE
            && result
                .as_ref()
                .is_ok_and(|reply| matches!(reply.status(), Status::Ok | Status::Failed))
        {
            self.bindings[payload[2] as usize] = None;
            self.confirmed[payload[2] as usize] = false;
        }
        if payload[0] == CMD_CLOSE
            && !result
                .as_ref()
                .is_ok_and(|reply| matches!(reply.status(), Status::Ok | Status::Failed))
        {
            self.confirmed[payload[2] as usize] = false;
        }
        result
    }
    /// Allowlisted transfer that requires an Ok status.
    fn command(&mut self, payload: &[u8]) -> Result<Reply> {
        let reply = self.xfer(payload)?;
        ensure_ok(payload, &reply)?;
        Ok(reply)
    }
    /// GET; Err on non-Ok status (with friendly text for Offline/NotAllowed).
    pub fn get(&mut self, prop: u8) -> Result<Reply> {
        let payload = [CMD_GET, prop];
        check_allowed(self.model, &payload)?;
        let reply = match self.xfer(&payload) {
            Err(error) if is_timeout(&error) => {
                // Reads may retry once (DESIGN §3 rule 6).
                self.pause(READ_RETRY_DELAY)?;
                self.xfer(&payload)?
            }
            other => other?,
        };
        ensure_ok(&payload, &reply)?;
        Ok(reply)
    }
    pub fn get_u32(&mut self, prop: u8) -> Result<u32> {
        Ok(self.get(prop)?.value_u32())
    }
    /// SET `value` as `width` little-endian bytes. Does not read-compare (callers do).
    pub fn set(&mut self, prop: u8, value: u32, width: usize) -> Result<()> {
        if !(1..=4).contains(&width) || (width < 4 && value >> (8 * width) != 0) {
            bail!("Value {value} does not fit {width} byte(s) for property 0x{prop:02x}");
        }
        let mut payload = vec![CMD_SET, prop, 0];
        payload.extend_from_slice(&value.to_le_bytes()[..width]);
        check_allowed(self.model, &payload)?;
        if prop == PROP_POLLING {
            let max = self.get_u32(PROP_MAX_POLLING)?;
            if value > max {
                bail!("Polling rate code {value} exceeds the keyboard's maximum ({max})");
            }
        }
        self.command(&payload)?;
        Ok(())
    }
    pub fn ping(&mut self) -> Result<()> {
        self.command(&[CMD_PING])?;
        Ok(())
    }
    pub fn mode(&mut self) -> Result<u8> {
        Ok(self.get_u32(PROP_MODE)? as u8)
    }
    /// Run `f` in software mode. If the device is in hardware mode, enter software mode,
    /// run `f`, close handle 1 and restore hardware mode (also on error), verifying the mode.
    /// If already in software mode (companion owns it), just run `f`.
    pub fn with_software_mode<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        match self.mode()? {
            MODE_SOFTWARE => return f(self),
            MODE_HARDWARE => {}
            other => bail!("Keyboard is in unexpected mode {other} — run Rescue"),
        }
        // The guard restores hardware mode when dropped (error via `?` or panic unwind).
        let mut guard = SoftwareMode {
            bragi: self,
            armed: true,
        };
        if let Err(error) = guard.set(PROP_MODE, u32::from(MODE_SOFTWARE), 1) {
            // A definite status means the mode did not change: nothing to restore. On a timeout
            // the outcome is unknown, so the guard still tries to restore hardware mode.
            if error.chain().any(|cause| cause.is::<StatusError>()) {
                guard.armed = false;
            }
            return Err(error);
        }
        let result = f(&mut guard);
        let restored = guard.restore();
        match (result, restored) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore)) => Err(error.context(format!(
                "also failed to restore hardware mode ({restore:#}) — run Rescue"
            ))),
        }
    }
    /// Open `handle` on `resource` (allowlisted; software mode).
    pub fn open_handle(&mut self, handle: u8, resource: u16) -> Result<()> {
        let [low, high] = resource.to_le_bytes();
        self.command(&[CMD_OPEN, handle, low, high])?;
        Ok(())
    }
    /// Size of the resource open on `handle` (probe, u32 LE at reply[5..9]).
    pub fn probe(&mut self, handle: u8) -> Result<u32> {
        let reply = self.command(&[CMD_PROBE, handle])?;
        let bytes = reply.raw.get(5..9).context("Short probe reply")?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    /// Read a whole resource via handle 1 (must already be in software mode). Allowlisted.
    pub fn read_resource(&mut self, resource: u16) -> Result<Vec<u8>> {
        if !readable_resources(self.model).contains(&resource) {
            bail!(
                "Reading resource 0x{resource:04x} is not allowed on {:?}",
                self.model
            );
        }
        const HANDLE: u8 = 1;
        let deadline = self.until();
        if let Some(result) = self.transport.read_resource_until(resource, deadline) {
            return result;
        }
        let opened = self.open_handle(HANDLE, resource);
        let result = opened.and_then(|()| {
            (|| -> Result<Vec<u8>> {
                let size = self.probe(HANDLE)? as usize;
                if size > MAX_RESOURCE_LEN {
                    bail!("Resource 0x{resource:04x} reports an implausible size of {size} bytes");
                }
                let mut out = Vec::with_capacity(size);
                while out.len() < size {
                    let reply = self.command(&[CMD_READ, HANDLE])?;
                    let data = reply.data();
                    if data.is_empty() {
                        bail!("Empty read from resource 0x{resource:04x}");
                    }
                    let take = data.len().min(size - out.len());
                    out.extend_from_slice(&data[..take]);
                }
                Ok(out)
            })()
        });
        let closed = self.cleanup(|s| s.close_handle(HANDLE));
        combine_cleanup(result, closed)
    }
    /// Write `bytes` to a resource via `handle` (software mode). `declared_len` is the length
    /// field placed in the first chunk (see DESIGN §2 framing). Closes the handle unless `keep_open`.
    pub fn write_resource(
        &mut self,
        handle: u8,
        resource: u16,
        declared_len: u32,
        bytes: &[u8],
        keep_open: bool,
    ) -> Result<()> {
        if !writable_resources(self.model).contains(&resource) {
            bail!(
                "Writing resource 0x{resource:04x} is not allowed on {:?}",
                self.model
            );
        }
        let deadline = self.until();
        if let Some(result) =
            self.transport
                .write_resource_until(resource, declared_len, bytes, deadline)
        {
            return result;
        }
        let opened = self.open_handle(handle, resource);
        let result = opened.and_then(|()| {
            (|| -> Result<()> {
                self.pause(OPEN_SETTLE)?;
                self.probe(handle)?;
                self.write_handle(handle, declared_len, bytes)
            })()
        });
        if result.is_err() || !keep_open {
            let closed = self.cleanup(|s| s.close_handle(handle));
            return combine_cleanup(result, closed);
        }
        result
    }
    /// Write one buffer to a handle that is already open on a writable resource:
    /// `06 h` + first chunk of `[declared_len u32 LE, bytes…]`, then `07 h` continuations of
    /// `frame_len - 3` bytes (61 on 64-byte devices), paced on jumbo devices.
    pub fn write_handle(&mut self, handle: u8, declared_len: u32, bytes: &[u8]) -> Result<()> {
        let mut stream = declared_len.to_le_bytes().to_vec();
        stream.extend_from_slice(bytes);
        let frame_len = self.model.frame_len();
        let chunk = frame_len - 3;
        for (index, part) in stream.chunks(chunk).enumerate() {
            if index > 0 && frame_len > 64 {
                self.pause(JUMBO_PACE)?;
            }
            let cmd = if index == 0 {
                CMD_WRITE
            } else {
                CMD_WRITE_MORE
            };
            let mut payload = Vec::with_capacity(part.len() + 2);
            payload.extend_from_slice(&[cmd, handle]);
            payload.extend_from_slice(part);
            self.command(&payload)?;
        }
        Ok(())
    }
    /// Close handle `h`; ignores "not open" statuses.
    pub fn close_handle(&mut self, handle: u8) -> Result<()> {
        let payload = [CMD_CLOSE, 1, handle];
        let reply = self.xfer(&payload)?;
        match reply.status() {
            // Hardware evidence: CLOSE of an unused handle answers status 3.
            Status::Ok | Status::Failed => {
                self.bindings[handle as usize] = None;
                Ok(())
            }
            _ => ensure_ok(&payload, &reply),
        }
    }
}
fn combine_cleanup<R>(result: Result<R>, cleanup: Result<()>) -> Result<R> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("also failed cleanup ({cleanup:#}) — run Rescue")))
        }
    }
}

/// Scope guard for [`Bragi::with_software_mode`]: restores hardware mode on drop.
struct SoftwareMode<'a, T: Transport> {
    bragi: &'a mut Bragi<T>,
    armed: bool,
}

impl<T: Transport> SoftwareMode<'_, T> {
    /// Close handle 1, SET 03=1 and verify GET 03 == 1. Disarms the guard.
    fn restore(&mut self) -> Result<()> {
        self.armed = false;
        self.bragi.cleanup(|bragi| {
            let closed = bragi.cleanup_handles();
            let restored = bragi.set(PROP_MODE, u32::from(MODE_HARDWARE), 1).and_then(|()| {
                let mode = bragi.mode()?;
                if mode != MODE_HARDWARE { bail!("Keyboard stayed in mode {mode} after restoring hardware mode — run Rescue"); }
                Ok(())
            });
            combine_cleanup(restored, closed)
        })
    }
}

impl<T: Transport> Deref for SoftwareMode<'_, T> {
    type Target = Bragi<T>;
    fn deref(&self) -> &Bragi<T> {
        self.bragi
    }
}

impl<T: Transport> DerefMut for SoftwareMode<'_, T> {
    fn deref_mut(&mut self) -> &mut Bragi<T> {
        self.bragi
    }
}

impl<T: Transport> Drop for SoftwareMode<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.restore();
        }
    }
}

/// GET 0x36 on an already-open receiver session (target 0x08): bit 1 = keyboard online.
pub fn receiver_children_online<T: Transport>(receiver: &mut Bragi<T>) -> Result<bool> {
    Ok(receiver.get_u32(PROP_CHILDREN)? & 0x02 != 0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Scripted fake keyboard: models hardware/software mode rules from DESIGN §2.
    #[derive(Default)]
    pub(crate) struct FakeKeyboard {
        pub mode: u8,
        pub props: HashMap<u8, u32>,
        pub resources: HashMap<u16, Vec<u8>>,
        pub open: HashMap<u8, u16>,
        pub written: HashMap<u16, Vec<u8>>,
        pub log: Vec<Vec<u8>>,
        pub sleeps: Vec<Duration>,
        /// Commands (first byte) that should fail with this status.
        pub fail_cmd: HashMap<u8, u8>,
        /// Number of GETs that should time out before answering.
        pub timeouts: u32,
        pub offline: bool,
        /// SET 03=1 is ignored (mode stays) — simulates a stuck device.
        pub stuck: bool,
        read_pos: usize,
    }

    impl FakeKeyboard {
        pub fn new() -> Self {
            let mut fake = Self {
                mode: MODE_HARDWARE,
                ..Self::default()
            };
            fake.props.insert(PROP_BRIGHTNESS, 700);
            fake.props.insert(PROP_MAX_POLLING, 4);
            fake.props.insert(PROP_POLLING, 4);
            fake.resources
                .insert(RES_K65_EFFECT, vec![0x7e, 0xa0, 0x02, 0x03]);
            fake
        }
        fn reply(cmd: u8, status: u8, data: &[u8]) -> Reply {
            let mut raw = vec![0u8; 64];
            raw[0] = 1;
            raw[1] = cmd;
            raw[2] = status;
            raw[3..3 + data.len()].copy_from_slice(data);
            Reply { raw }
        }
        pub fn cmds(&self) -> Vec<u8> {
            self.log.iter().map(|payload| payload[0]).collect()
        }
    }

    impl Transport for FakeKeyboard {
        fn sleep(&mut self, duration: Duration) {
            self.sleeps.push(duration);
        }
        fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
            self.log.push(payload.to_vec());
            let cmd = payload[0];
            if self.offline {
                return Ok(Self::reply(cmd, 6, &[]));
            }
            if let Some(status) = self.fail_cmd.get(&cmd) {
                return Ok(Self::reply(cmd, *status, &[]));
            }
            let hardware = self.mode == MODE_HARDWARE;
            match cmd {
                CMD_GET => {
                    if self.timeouts > 0 {
                        self.timeouts -= 1;
                        return Err(ReplyTimeout { cmd }.into());
                    }
                    let prop = payload[1];
                    if prop == PROP_MODE {
                        return Ok(Self::reply(cmd, 0, &[self.mode]));
                    }
                    match self.props.get(&prop) {
                        Some(value) => Ok(Self::reply(cmd, 0, &value.to_le_bytes())),
                        None => Ok(Self::reply(cmd, 7, &[])),
                    }
                }
                CMD_SET => {
                    let prop = payload[1];
                    let mut bytes = [0u8; 4];
                    for (slot, byte) in bytes.iter_mut().zip(&payload[3..]) {
                        *slot = *byte;
                    }
                    let value = u32::from_le_bytes(bytes);
                    if prop == PROP_MODE {
                        if !(self.stuck && value == 1) {
                            self.mode = value as u8;
                        }
                        return Ok(Self::reply(cmd, 0, &[]));
                    }
                    if hardware {
                        return Ok(Self::reply(cmd, 7, &[]));
                    }
                    self.props.insert(prop, value);
                    Ok(Self::reply(cmd, 0, &[]))
                }
                _ if hardware && !matches!(cmd, CMD_PING | CMD_CLOSE) => {
                    Ok(Self::reply(cmd, 7, &[]))
                }
                CMD_OPEN => {
                    let handle = payload[1];
                    if self.open.contains_key(&handle) {
                        return Ok(Self::reply(cmd, 3, &[]));
                    }
                    self.open
                        .insert(handle, u16::from_le_bytes([payload[2], payload[3]]));
                    self.read_pos = 0;
                    Ok(Self::reply(cmd, 0, &[]))
                }
                CMD_PROBE => {
                    let Some(resource) = self.open.get(&payload[1]) else {
                        return Ok(Self::reply(cmd, 3, &[]));
                    };
                    let size = self.resources.get(resource).map_or(0, Vec::len) as u32;
                    let mut data = vec![0, 0];
                    data.extend_from_slice(&size.to_le_bytes());
                    Ok(Self::reply(cmd, 0, &data))
                }
                CMD_READ => {
                    let resource = self.open[&payload[1]];
                    let content = self.resources.get(&resource).cloned().unwrap_or_default();
                    let end = (self.read_pos + 61).min(content.len());
                    let chunk = content[self.read_pos..end].to_vec();
                    self.read_pos = end;
                    Ok(Self::reply(cmd, 0, &chunk))
                }
                CMD_WRITE | CMD_WRITE_MORE => {
                    let Some(resource) = self.open.get(&payload[1]).copied() else {
                        return Ok(Self::reply(cmd, 3, &[]));
                    };
                    let entry = self.written.entry(resource).or_default();
                    if cmd == CMD_WRITE {
                        entry.clear();
                    }
                    entry.extend_from_slice(&payload[2..]);
                    Ok(Self::reply(cmd, 0, &[]))
                }
                CMD_CLOSE => {
                    if let Some(resource) = self.open.get(&payload[2]).copied()
                        && resource == RES_K65_EFFECT
                        && let Some(written) = self.written.get(&resource)
                    {
                        self.resources.insert(resource, written[4..].to_vec());
                    }
                    let status = if self.open.remove(&payload[2]).is_some() {
                        0
                    } else {
                        3
                    };
                    Ok(Self::reply(cmd, status, &[]))
                }
                CMD_PING => Ok(Self::reply(cmd, 0, &[])),
                _ => Ok(Self::reply(cmd, 5, &[])),
            }
        }
    }

    fn k65() -> Bragi<FakeKeyboard> {
        Bragi::new(FakeKeyboard::new(), Model::K65PlusWireless)
    }

    #[test]
    fn status_bytes_map() {
        assert_eq!(Status::from_byte(0), Status::Ok);
        assert_eq!(Status::from_byte(1), Status::InvalidValue);
        assert_eq!(Status::from_byte(3), Status::Failed);
        assert_eq!(Status::from_byte(5), Status::Unsupported);
        assert_eq!(Status::from_byte(6), Status::Offline);
        assert_eq!(Status::from_byte(7), Status::NotAllowed);
        assert_eq!(Status::from_byte(9), Status::Other(9));
    }

    #[test]
    fn friendly_status_errors() {
        let mut bragi = k65();
        bragi.transport.offline = true;
        let error = bragi.get(PROP_BATTERY).unwrap_err();
        assert!(is_offline(&error));
        assert_eq!(
            error.to_string(),
            "Keyboard is asleep or disconnected — press a key on it"
        );
        let mut bragi = k65();
        let error = bragi.set(PROP_BRIGHTNESS, 500, 2).unwrap_err();
        assert!(!is_offline(&error));
        assert!(
            error
                .to_string()
                .starts_with("Not allowed while the keyboard is in hardware mode")
        );
    }

    #[test]
    fn reply_accessors() {
        let reply = Reply {
            raw: vec![0x01, 0x02, 0x00, 0xe8, 0x03],
        };
        assert_eq!(reply.status(), Status::Ok);
        assert_eq!(reply.value_u32(), 1000);
        assert_eq!(reply.data(), &[0xe8, 0x03]);
        let short = Reply {
            raw: vec![0x00, 0x02],
        };
        assert_eq!(short.status(), Status::Other(0xff));
        assert_eq!(short.value_u32(), 0);
        assert!(short.data().is_empty());
        let firmware = Reply {
            raw: vec![1, 2, 0, 0x05, 0x1a, 0x9a, 0, 0, 0xff],
        };
        assert_eq!(firmware.value_u32(), 0x009a_1a05);
    }

    #[test]
    fn frames_are_padded_with_report_id_and_target() {
        let frame = encode_frame(0x09, &[CMD_GET, PROP_BATTERY], 64).unwrap();
        assert_eq!(frame.len(), 65);
        assert_eq!(&frame[..4], &[0x00, 0x09, 0x02, 0x0f]);
        assert!(frame[4..].iter().all(|byte| *byte == 0));
        assert_eq!(encode_frame(0x08, &[CMD_PING], 1024).unwrap().len(), 1025);
        assert!(encode_frame(0x08, &[0u8; 64], 64).is_err());
        assert!(encode_frame(0x08, &[], 64).is_err());
    }

    #[test]
    fn allowlist_accepts_the_documented_operations() {
        let k65 = Model::K65PlusWireless;
        let k100 = Model::K100Rgb;
        for payload in [
            vec![CMD_GET, PROP_BATTERY],
            vec![CMD_GET, PROP_CHILDREN],
            vec![CMD_SET, PROP_BRIGHTNESS, 0, 0xe8, 0x03],
            vec![CMD_SET, PROP_MODE, 0, 2],
            vec![CMD_SET, PROP_MODE, 0, 1],
            vec![CMD_SET, PROP_POLLING, 0, 4],
            vec![CMD_SET, PROP_SLEEP_MS, 0, 0x60, 0xea, 0x00, 0x00],
            vec![CMD_SET, PROP_LOCK_ENABLE, 0, 1],
            vec![CMD_SET, PROP_LOCK_MASK, 0, 0x0f],
            vec![CMD_OPEN, 1, 0x60, 0x6d],
            vec![CMD_OPEN, 1, 0x22, 0x00],
            vec![CMD_PROBE, 1],
            vec![CMD_READ, 1],
            vec![CMD_WRITE, 1, 93, 0, 0, 0, 0x7e],
            vec![CMD_WRITE_MORE, 1, 0, 0],
            vec![CMD_CLOSE, 1, 1],
            vec![CMD_CLOSE, 1, 2],
            vec![CMD_PING],
        ] {
            check_allowed(k65, &payload).unwrap_or_else(|error| panic!("{payload:02x?}: {error}"));
        }
        check_allowed(k100, &[CMD_SET, PROP_BRIGHTNESS_2, 0, 0xe8, 0x03]).unwrap();
        check_allowed(k100, &[CMD_SET, PROP_POLLING, 0, 7]).unwrap();
        check_allowed(k100, &[CMD_OPEN, 0, 0x22, 0x00]).unwrap();
        let mut long = vec![CMD_WRITE_MORE, 0];
        long.resize(1023, 0xaa);
        check_allowed(k100, &long).unwrap();
        assert!(check_allowed(k65, &long).is_err());
    }

    #[test]
    fn allowlist_rejects_forbidden_operations() {
        let k65 = Model::K65PlusWireless;
        let k100 = Model::K100Rgb;
        let forbidden: &[(Model, &[u8])] = &[
            (k65, &[CMD_OPEN, 1, 0x02, 0x00]), // key table (read dropped the keyboard)
            (k65, &[CMD_OPEN, 1, 0x05, 0x00]), // pairing
            (k65, &[CMD_OPEN, 1, 0x06, 0x00]), // pairing
            (k65, &[CMD_OPEN, 1, 0x13, 0x00]), // directory
            (k65, &[CMD_OPEN, 3, 0x60, 0x6d]), // handle out of range
            (k100, &[CMD_OPEN, 1, 0x60, 0x6d]), // K65-only resource
            (k65, &[0x0B, 0x01]),
            (k65, &[0x15, 0x01]),
            (k65, &[0x00]),
            (k65, &[]),
            (k65, &[CMD_SET, 0x3A, 0, 2]), // Bluetooth switch
            (k65, &[CMD_SET, PROP_MODE, 0, 6]),
            (k65, &[CMD_SET, PROP_MODE, 0, 0]),
            (k65, &[CMD_SET, PROP_BRIGHTNESS, 0, 0xe9, 0x03]), // 1001
            (k65, &[CMD_SET, PROP_POLLING, 0, 5]),
            (k65, &[CMD_SET, PROP_LOCK_ENABLE, 0, 2]),
            (k65, &[CMD_SET, PROP_LOCK_MASK, 0, 0x10]),
            (k65, &[CMD_SET, PROP_SLEEP_MS, 0, 1, 0, 0, 0]),
            (k65, &[CMD_SET, PROP_BRIGHTNESS_2, 0, 1, 0]),
            (k100, &[CMD_SET, PROP_SLEEP_MS, 0, 0x60, 0xea, 0, 0]),
            (k100, &[CMD_SET, PROP_POLLING, 0, 8]),
            (k65, &[CMD_SET, PROP_MODE, 1, 2]), // malformed reserved byte
            (k65, &[CMD_SET, PROP_LOCK_ENABLE, 0, 1, 1]), // extra value bytes
            (k65, &[CMD_GET, 0x3A]),
            (k100, &[CMD_GET, PROP_BATTERY]),
            (k65, &[CMD_CLOSE, 1, 3]),
            (k65, &[CMD_CLOSE, 0, 1]),
        ];
        for (model, payload) in forbidden {
            assert!(
                check_allowed(*model, payload).is_err(),
                "{model:?} {payload:02x?} must be rejected"
            );
        }
    }

    #[test]
    fn forbidden_operations_error_before_any_write() {
        let mut bragi = k65();
        for payload in [
            &[CMD_OPEN, 1, 0x02, 0x00][..],
            &[CMD_OPEN, 1, 0x05, 0x00],
            &[0x0B],
            &[0x15],
            &[CMD_SET, 0x3A, 0, 1],
            &[CMD_SET, PROP_MODE, 0, 6],
        ] {
            assert!(bragi.xfer(payload).is_err());
        }
        assert!(bragi.set(PROP_MODE, 6, 1).is_err());
        assert!(bragi.set(0x3A, 1, 1).is_err());
        assert!(bragi.read_resource(0x0002).is_err());
        assert!(bragi.write_resource(1, 0x0002, 4, &[0; 4], false).is_err());
        assert!(bragi.write_resource(1, 0x0005, 4, &[0; 4], false).is_err());
        assert!(bragi.get(0x3A).is_err());
        assert!(
            bragi.transport.log.is_empty(),
            "{:02x?}",
            bragi.transport.log
        );
    }

    #[test]
    fn get_retries_once_after_timeout_only() {
        let mut bragi = k65();
        bragi.transport.timeouts = 1;
        assert_eq!(bragi.get_u32(PROP_BRIGHTNESS).unwrap(), 700);
        assert_eq!(bragi.transport.sleeps, vec![READ_RETRY_DELAY]);
        let mut bragi = k65();
        bragi.transport.timeouts = 2;
        assert!(is_timeout(&bragi.get(PROP_BRIGHTNESS).unwrap_err()));
        assert_eq!(bragi.transport.log.len(), 2);
        let mut bragi = k65();
        bragi.transport.offline = true;
        assert!(bragi.get(PROP_BRIGHTNESS).is_err());
        assert_eq!(bragi.transport.log.len(), 1, "status 6 is never retried");
    }

    #[test]
    fn polling_set_is_capped_by_max_polling_property() {
        let mut bragi = k65();
        bragi.transport.mode = MODE_SOFTWARE;
        bragi.transport.props.insert(PROP_MAX_POLLING, 3);
        assert!(bragi.set(PROP_POLLING, 4, 1).is_err());
        assert!(!bragi.transport.cmds().contains(&CMD_SET));
        bragi.set(PROP_POLLING, 3, 1).unwrap();
        assert_eq!(bragi.transport.props[&PROP_POLLING], 3);
        assert_eq!(polling_hz(4), Some(1000));
        assert_eq!(polling_hz(7), Some(8000));
        assert_eq!(polling_code(125), Some(1));
        assert_eq!(polling_code(300), None);
    }

    #[test]
    fn software_mode_scope_restores_on_ok() {
        let mut bragi = k65();
        let value = bragi
            .with_software_mode(|bragi| {
                assert_eq!(bragi.mode()?, MODE_SOFTWARE);
                bragi.set(PROP_BRIGHTNESS, 300, 2)?;
                Ok(42)
            })
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
        assert_eq!(bragi.transport.props[&PROP_BRIGHTNESS], 300);
        let log = &bragi.transport.log;
        // GET 03, SET 03=2, ..., close h1, SET 03=1, GET 03 verify
        assert_eq!(log[0], vec![CMD_GET, PROP_MODE]);
        assert_eq!(log[1], vec![CMD_SET, PROP_MODE, 0, 2]);
        let n = log.len();
        assert!(
            !log.iter().any(|payload| payload[0] == CMD_CLOSE),
            "no tracked handles need closing"
        );
        assert_eq!(log[n - 2], vec![CMD_SET, PROP_MODE, 0, 1]);
        assert_eq!(log[n - 1], vec![CMD_GET, PROP_MODE]);
    }

    #[test]
    fn software_mode_scope_restores_on_error() {
        let mut bragi = k65();
        let error = bragi
            .with_software_mode(|bragi| -> Result<()> {
                bragi.open_handle(1, RES_K65_EFFECT)?;
                bail!("boom")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "boom");
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
        assert!(
            bragi.transport.open.is_empty(),
            "handle 1 closed before restoring"
        );
        let log = &bragi.transport.log;
        assert_eq!(log[log.len() - 2], vec![CMD_SET, PROP_MODE, 0, 1]);
    }

    #[test]
    fn software_mode_scope_restores_on_panic() {
        let mut bragi = k65();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = bragi.with_software_mode(|bragi| -> Result<()> {
                bragi.open_handle(1, RES_K65_EFFECT)?;
                panic!("bug inside scope");
            });
        }));
        assert!(outcome.is_err());
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
        assert!(bragi.transport.open.is_empty());
    }

    #[test]
    fn software_mode_scope_is_a_no_op_when_companion_owns_mode() {
        let mut bragi = k65();
        bragi.transport.mode = MODE_SOFTWARE;
        bragi
            .with_software_mode(|bragi| bragi.set(PROP_BRIGHTNESS, 100, 2))
            .unwrap();
        assert_eq!(bragi.transport.mode, MODE_SOFTWARE);
        assert_eq!(
            bragi.transport.log,
            vec![
                vec![CMD_GET, PROP_MODE],
                vec![CMD_SET, PROP_BRIGHTNESS, 0, 100, 0]
            ]
        );
    }

    #[test]
    fn software_mode_scope_reports_failed_verification() {
        let mut bragi = k65();
        bragi.transport.stuck = true;
        let error = bragi.with_software_mode(|_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("Rescue"), "{error}");
        let mut bragi = k65();
        bragi.transport.stuck = true;
        let error = bragi
            .with_software_mode(|_| -> Result<()> { bail!("first") })
            .unwrap_err();
        assert!(format!("{error:#}").contains("first"));
        assert!(format!("{error:#}").contains("restore hardware mode"));
    }

    #[test]
    fn software_mode_refuses_unexpected_mode_without_writing() {
        let mut bragi = k65();
        bragi.transport.mode = 5;
        assert!(bragi.with_software_mode(|_| Ok(())).is_err());
        assert_eq!(bragi.transport.log.len(), 1, "unexpected mode: no writes");
    }

    #[test]
    fn rejected_mode_entry_writes_nothing_more() {
        let mut bragi = k65();
        bragi.transport.fail_cmd.insert(CMD_SET, 6);
        let error = bragi.with_software_mode(|_| Ok(())).unwrap_err();
        assert!(is_offline(&error));
        assert_eq!(
            bragi.transport.cmds(),
            vec![CMD_GET, CMD_SET],
            "no restore writes after status 6"
        );
    }

    #[test]
    fn resource_read_needs_software_mode_and_reads_compact_effect() {
        let mut bragi = k65();
        let error = bragi.read_resource(RES_K65_EFFECT).unwrap_err();
        assert!(error.to_string().contains("hardware mode"));
        let data = bragi
            .with_software_mode(|bragi| bragi.read_resource(RES_K65_EFFECT))
            .unwrap();
        assert_eq!(data, vec![0x7e, 0xa0, 0x02, 0x03]);
        assert!(bragi.transport.open.is_empty());
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
    }

    #[test]
    fn resource_read_spans_multiple_chunks() {
        let mut bragi = k65();
        bragi.transport.mode = MODE_SOFTWARE;
        let content: Vec<u8> = (0..150u8).collect();
        bragi
            .transport
            .resources
            .insert(RES_K65_EFFECT, content.clone());
        assert_eq!(bragi.read_resource(RES_K65_EFFECT).unwrap(), content);
        assert_eq!(
            bragi
                .transport
                .cmds()
                .iter()
                .filter(|cmd| **cmd == CMD_READ)
                .count(),
            3
        );
        assert_eq!(*bragi.transport.log.last().unwrap(), vec![CMD_CLOSE, 1, 1]);
    }

    #[test]
    fn resource_write_uses_documented_framing() {
        // Static red: header 7e 20 01, data 00 00 00 01 ff 00 00 ff padded to 93, len 93.
        let mut bytes = vec![0x7e, 0x20, 0x01];
        let mut data = vec![0x00, 0x00, 0x00, 0x01, 0xff, 0x00, 0x00, 0xff];
        data.resize(93, 0);
        bytes.extend_from_slice(&data);
        let mut bragi = k65();
        bragi
            .with_software_mode(|bragi| bragi.write_resource(1, RES_K65_EFFECT, 93, &bytes, false))
            .unwrap();
        let log = &bragi.transport.log;
        let pos = log
            .iter()
            .position(|payload| payload[0] == CMD_OPEN)
            .unwrap();
        assert_eq!(log[pos], vec![CMD_OPEN, 1, 0x60, 0x6d]);
        assert_eq!(log[pos + 1], vec![CMD_PROBE, 1]);
        let first = &log[pos + 2];
        assert_eq!(
            &first[..10],
            &[CMD_WRITE, 1, 93, 0, 0, 0, 0x7e, 0x20, 0x01, 0x00]
        );
        assert_eq!(first.len(), 63, "06 h + 61 bytes");
        let second = &log[pos + 3];
        assert_eq!(&second[..2], &[CMD_WRITE_MORE, 1]);
        assert_eq!(second.len(), 2 + (4 + 96 - 61));
        assert_eq!(log[pos + 4], vec![CMD_CLOSE, 1, 1]);
        assert_eq!(
            bragi.transport.sleeps,
            vec![OPEN_SETTLE],
            "500 ms after open, no pacing on 64-byte frames"
        );
        let mut expected = 93u32.to_le_bytes().to_vec();
        expected.extend_from_slice(&bytes);
        assert_eq!(bragi.transport.written[&RES_K65_EFFECT], expected);
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
    }

    #[test]
    fn jumbo_writes_use_1021_byte_chunks_with_pacing_and_can_keep_handle() {
        let mut bragi = Bragi::new(FakeKeyboard::new(), Model::K100Rgb);
        bragi.transport.mode = MODE_SOFTWARE;
        let frame = vec![0x5a; 2 + 193 * 3 + 1500];
        bragi
            .write_resource(0, RES_DIRECT_LIGHTING, frame.len() as u32, &frame, true)
            .unwrap();
        let writes: Vec<&Vec<u8>> = bragi
            .transport
            .log
            .iter()
            .filter(|payload| matches!(payload[0], CMD_WRITE | CMD_WRITE_MORE))
            .collect();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[0].len(), 2 + 1021);
        assert_eq!(writes[1].len(), 2 + 1021);
        assert_eq!(
            bragi.transport.sleeps,
            vec![OPEN_SETTLE, JUMBO_PACE, JUMBO_PACE]
        );
        assert!(
            bragi.transport.open.contains_key(&0),
            "keep_open leaves the handle open"
        );
        bragi.write_handle(0, 4, &[1, 2, 3, 4]).unwrap();
        bragi.close_handle(0).unwrap();
        assert!(bragi.transport.open.is_empty());
    }

    #[test]
    fn failed_write_closes_handle_even_with_keep_open() {
        let mut bragi = k65();
        bragi.transport.mode = MODE_SOFTWARE;
        bragi.transport.fail_cmd.insert(CMD_WRITE_MORE, 3);
        let error = bragi
            .write_resource(1, RES_K65_EFFECT, 93, &[0u8; 96], true)
            .unwrap_err();
        assert!(error.to_string().contains("failure"), "{error}");
        assert_eq!(*bragi.transport.log.last().unwrap(), vec![CMD_CLOSE, 1, 1]);
    }

    #[test]
    fn close_ignores_not_open_but_reports_offline() {
        let mut bragi = k65();
        bragi.transport.mode = MODE_SOFTWARE;
        bragi.close_handle(1).unwrap();
        bragi.transport.offline = true;
        assert!(is_offline(&bragi.close_handle(1).unwrap_err()));
    }

    #[test]
    fn receiver_children_bitmask() {
        let mut bragi = k65();
        bragi.transport.props.insert(PROP_CHILDREN, 0x02);
        assert!(receiver_children_online(&mut bragi).unwrap());
        bragi.transport.props.insert(PROP_CHILDREN, 0x00);
        assert!(!receiver_children_online(&mut bragi).unwrap());
    }
}

#[cfg(test)]
mod deadline_binding_regressions {
    use super::tests::FakeKeyboard;
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    struct LostOpen {
        fake: FakeKeyboard,
        lost: bool,
    }
    impl Transport for LostOpen {
        fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
            let reply = self.fake.xfer(payload)?;
            if payload[0] == CMD_OPEN && !self.lost {
                self.lost = true;
                return Err(ReplyTimeout { cmd: CMD_OPEN }.into());
            }
            Ok(reply)
        }
    }
    #[test]
    fn lost_open_ack_is_closed_and_forbidden_unknown_reads_send_nothing() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        let mut bragi = Bragi::new(LostOpen { fake, lost: false }, Model::K65PlusWireless);
        assert!(bragi.open_handle(0, RES_DIRECT_LIGHTING).is_err());
        let before = bragi.transport.fake.log.len();
        assert!(bragi.xfer(&[CMD_READ, 0]).is_err());
        assert!(bragi.xfer(&[CMD_READ, 2]).is_err());
        assert_eq!(bragi.transport.fake.log.len(), before);
        bragi.cleanup_handles().unwrap();
        assert!(bragi.transport.fake.open.is_empty());
    }
    #[test]
    fn lost_effect_open_ack_never_authorizes_read_of_an_unknown_device_binding() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        let mut bragi = Bragi::new(LostOpen { fake, lost: false }, Model::K65PlusWireless);
        assert!(bragi.open_handle(1, RES_K65_EFFECT).is_err());
        let before = bragi.transport.fake.log.len();
        assert!(bragi.xfer(&[CMD_READ, 1]).is_err());
        assert!(bragi.xfer(&[CMD_PROBE, 1]).is_err());
        assert_eq!(bragi.transport.fake.log.len(), before);
        bragi.cleanup_handles().unwrap();
    }
    #[test]
    fn rejected_close_remains_tracked_for_independent_retry() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        let mut bragi = Bragi::new(fake, Model::K65PlusWireless);
        bragi.open_handle(1, RES_K65_EFFECT).unwrap();
        bragi.transport.fail_cmd.insert(CMD_CLOSE, 7);
        assert!(bragi.close_handle(1).is_err());
        assert!(bragi.open_handle(1, RES_K65_EFFECT).is_err());
        bragi.transport.fail_cmd.clear();
        bragi.cleanup_handles().unwrap();
        assert!(bragi.transport.open.is_empty());
    }
    #[test]
    fn absolute_budget_stops_work_but_joined_guard_restores_in_cleanup_reserve() {
        let started = Instant::now();
        let budget = RequestBudget {
            work: started + Duration::from_millis(25),
            cleanup: started + Duration::from_millis(250),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let thread = std::thread::spawn(move || {
            with_request_budget(budget, || {
                let mut bragi = Bragi::new(FakeKeyboard::new(), Model::K65PlusWireless);
                let result = bragi.with_software_mode(|bragi| {
                    std::thread::sleep(Duration::from_millis(35));
                    bragi.set(PROP_BRIGHTNESS, 500, 2)
                });
                assert!(result.is_err());
                assert_eq!(bragi.transport.mode, MODE_HARDWARE);
                assert_eq!(bragi.transport.props[&PROP_BRIGHTNESS], 700);
            })
        });
        thread.join().unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
    }
    #[test]
    fn cancellation_does_not_cancel_reserved_restoration() {
        let flag = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        with_request_budget(
            RequestBudget {
                work: now + Duration::from_secs(1),
                cleanup: now + Duration::from_secs(2),
                cancelled: flag.clone(),
            },
            || {
                let mut bragi = Bragi::new(FakeKeyboard::new(), Model::K65PlusWireless);
                let result = bragi.with_software_mode(|bragi| {
                    flag.store(true, Ordering::Release);
                    bragi.set(PROP_BRIGHTNESS, 300, 2)
                });
                assert!(result.is_err());
                assert_eq!(bragi.transport.mode, MODE_HARDWARE);
            },
        );
    }
}
