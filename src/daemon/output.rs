//! Virtual input devices (uinput) and the event vocabulary shared by dial emulation, remaps and
//! macros. The uinput device is driven with raw ioctls so it can advertise LEDs (the compositor's
//! Caps/Num Lock state is read back from it and forwarded to the physical keyboard).

use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_MSC: u16 = 0x04;
pub const EV_LED: u16 = 0x11;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_WHEEL_HI_RES: u16 = 0x0b;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_0: u16 = 11;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_NEXTSONG: u16 = 163;
pub const KEY_PLAYPAUSE: u16 = 164;
pub const KEY_PREVIOUSSONG: u16 = 165;
pub const KEY_STOPCD: u16 = 166;
/// Highest "keyboard" keycode (KEY_* below the BTN_* range).
pub const KEY_MAX_KEYBOARD: u16 = 0xff;
/// LED codes 0..=4: Num, Caps, Scroll, Compose, Kana.
pub const LED_COUNT: u16 = 5;

/// One input event without timestamp: (type, code, value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ev {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl Ev {
    pub const fn key(code: u16, value: i32) -> Self {
        Self {
            kind: EV_KEY,
            code,
            value,
        }
    }
    pub const fn rel(code: u16, value: i32) -> Self {
        Self {
            kind: EV_REL,
            code,
            value,
        }
    }
}

/// A virtual device that takes batches of events; each `emit` is followed by one SYN_REPORT.
pub trait EventSink: Send {
    fn emit(&mut self, events: &[Ev]) -> io::Result<()>;
    /// Readable fd delivering events written *to* the device (EV_LED), if any.
    fn feedback_fd(&self) -> Option<RawFd> {
        None
    }
    /// Read pending feedback events (non-blocking) into `out`.
    fn read_feedback(&mut self, _out: &mut Vec<Ev>) -> io::Result<()> {
        Ok(())
    }
    /// Keycodes the device can emit.
    fn keys(&self) -> &[u16];
}

/// An output ownership token, unique across physical nodes and synthetic jobs.
pub type OutputOwner = u64;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
pub fn new_owner() -> OutputOwner {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// Shared arbitration boundary. A failed batch may have reached the kernel partially: ownership
/// and potentially-down keys survive until confirmed release or virtual-device destruction.
pub struct SharedOutput {
    device: Option<Box<dyn EventSink>>,
    owners: HashMap<OutputOwner, BTreeSet<u16>>,
    uncertain: BTreeSet<u16>,
    failed: bool,
    waker: Option<Arc<super::util::Waker>>,
}
pub type SharedSink = Arc<Mutex<SharedOutput>>;

pub fn share(sink: Box<dyn EventSink>) -> SharedSink {
    Arc::new(Mutex::new(SharedOutput {
        device: Some(sink),
        owners: HashMap::new(),
        uncertain: BTreeSet::new(),
        failed: false,
        waker: None,
    }))
}
impl SharedOutput {
    pub fn keys(&self) -> &[u16] {
        self.device.as_ref().map_or(&[], |device| device.keys())
    }
    pub fn feedback_fd(&self) -> Option<RawFd> {
        self.device.as_ref().and_then(|device| device.feedback_fd())
    }
    pub fn read_feedback(&mut self, out: &mut Vec<Ev>) -> io::Result<()> {
        self.device
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
            .read_feedback(out)
    }
    /// Replacement is permitted only after all old producers have stopped.
    pub fn replace(&mut self, device: Box<dyn EventSink>) {
        self.device = Some(device);
        self.owners.clear();
        self.uncertain.clear();
        self.failed = false;
    }
    pub fn set_waker(&mut self, waker: Arc<super::util::Waker>) {
        self.waker = Some(waker);
    }
    pub fn failed(&self) -> bool {
        self.failed
    }
    fn emit(&mut self, owner: OutputOwner, events: &[Ev]) -> io::Result<()> {
        if self.failed
            && events
                .iter()
                .any(|event| event.kind != EV_KEY || event.value != 0)
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let previous = self.owners.clone();
        let mut batch = Vec::with_capacity(events.len());
        for event in events {
            if event.kind != EV_KEY {
                batch.push(*event);
                continue;
            }
            let held_elsewhere = self
                .owners
                .iter()
                .any(|(id, keys)| *id != owner && keys.contains(&event.code));
            let keys = self.owners.entry(owner).or_default();
            match event.value {
                1 => {
                    if keys.insert(event.code) && !held_elsewhere {
                        self.uncertain.insert(event.code);
                        batch.push(*event);
                    }
                }
                0 => {
                    if keys.remove(&event.code) && !held_elsewhere {
                        batch.push(*event);
                    }
                }
                2 if keys.contains(&event.code) => batch.push(*event),
                _ => {}
            }
        }
        let result = if batch.is_empty() {
            Ok(())
        } else {
            self.device
                .as_mut()
                .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?
                .emit(&batch)
        };
        if result.is_ok() {
            for event in batch
                .iter()
                .filter(|event| event.kind == EV_KEY && event.value == 0)
            {
                self.uncertain.remove(&event.code);
            }
            self.owners.retain(|_, keys| !keys.is_empty());
        } else {
            // Keep both the pre-write holds and attempted new downs; never forget failed key-ups.
            for (id, keys) in previous {
                self.owners.entry(id).or_default().extend(keys);
            }
            self.failed = true;
            if let Some(waker) = &self.waker {
                waker.wake();
            }
        }
        result
    }
    /// Terminal output failure: destroy even when producers still hold Arc clones.
    pub fn destroy(&mut self) {
        let release: Vec<Ev> = self
            .uncertain
            .iter()
            .map(|code| Ev::key(*code, 0))
            .collect();
        if let Some(device) = &mut self.device
            && !release.is_empty()
        {
            let _ = device.emit(&release);
        }
        self.device = None;
        self.failed = true;
        self.owners.clear();
        self.uncertain.clear();
    }
}
pub fn emit_owned(sink: &SharedSink, owner: OutputOwner, events: &[Ev]) -> io::Result<()> {
    sink.lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .emit(owner, events)
}
pub fn release_owned(sink: &SharedSink, owner: OutputOwner) -> io::Result<()> {
    let mut guard = sink.lock().unwrap_or_else(|poison| poison.into_inner());
    let events: Vec<Ev> = guard
        .owners
        .get(&owner)
        .into_iter()
        .flatten()
        .rev()
        .map(|code| Ev::key(*code, 0))
        .collect();
    guard.emit(owner, &events)
}
/// Keycodes every virtual keyboard supports: all keyboard keys (1..=255) plus `extra`.
pub fn keyboard_keys(extra: impl IntoIterator<Item = u16>) -> Vec<u16> {
    let mut keys: Vec<u16> = (1..=KEY_MAX_KEYBOARD).collect();
    keys.extend(extra);
    keys.sort_unstable();
    keys.dedup();
    keys
}

// ---- raw uinput -------------------------------------------------------------------------------

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
/// _IOW('U', 3, struct uinput_setup) — sizeof = 8 + 80 + 4 = 92.
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_LEDBIT: libc::c_ulong = 0x4004_5569;
const BUS_VIRTUAL: u16 = 0x06;
const UINPUT_PATH: &str = "/dev/uinput";

/// What a new uinput device advertises.
pub struct DeviceSpec<'a> {
    pub name: &'a str,
    pub keys: &'a [u16],
    pub rel: &'a [u16],
    pub leds: bool,
    pub product: u16,
}

/// A live uinput device; destroyed on drop.
pub struct Uinput {
    file: File,
    keys: Vec<u16>,
    buffer: Vec<libc::input_event>,
}

fn ioctl_int(fd: RawFd, request: libc::c_ulong, value: libc::c_int) -> io::Result<()> {
    // SAFETY: uinput "set bit" ioctls take an int by value on a live fd.
    if unsafe { libc::ioctl(fd, request, value) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Uinput {
    pub fn create(spec: &DeviceSpec) -> Result<Self> {
        let path = std::env::var_os("HYPRDECK_CORSAIR_DEV").map_or_else(
            || std::path::PathBuf::from(UINPUT_PATH),
            |root| std::path::PathBuf::from(root).join("uinput"),
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound => anyhow::anyhow!(
                    "No access to {UINPUT_PATH} — install the opt-in input rule \
                     (71-hyprdeck-corsair-input.rules)"
                ),
                _ => anyhow::Error::new(error).context(format!("Opening {UINPUT_PATH}")),
            })?;
        let fd = file.as_raw_fd();
        let setup = || -> io::Result<()> {
            if !spec.keys.is_empty() {
                ioctl_int(fd, UI_SET_EVBIT, EV_KEY.into())?;
                for key in spec.keys {
                    ioctl_int(fd, UI_SET_KEYBIT, (*key).into())?;
                }
            }
            if !spec.rel.is_empty() {
                ioctl_int(fd, UI_SET_EVBIT, EV_REL.into())?;
                for axis in spec.rel {
                    ioctl_int(fd, UI_SET_RELBIT, (*axis).into())?;
                }
            }
            if spec.leds {
                ioctl_int(fd, UI_SET_EVBIT, EV_LED.into())?;
                for led in 0..LED_COUNT {
                    ioctl_int(fd, UI_SET_LEDBIT, led.into())?;
                }
            }
            // SAFETY: zeroed plain C struct.
            let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
            setup.id.bustype = BUS_VIRTUAL;
            setup.id.vendor = crate::devices::CORSAIR_VID;
            setup.id.product = spec.product;
            setup.id.version = 1;
            for (slot, byte) in setup
                .name
                .iter_mut()
                .zip(spec.name.bytes().take(libc::UINPUT_MAX_NAME_SIZE - 1))
            {
                *slot = byte as libc::c_char;
            }
            // SAFETY: UI_DEV_SETUP takes a pointer to uinput_setup; UI_DEV_CREATE takes no arg.
            if unsafe { libc::ioctl(fd, UI_DEV_SETUP, &setup) } < 0
                || unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        setup().with_context(|| format!("Creating virtual input device {:?}", spec.name))?;
        Ok(Self {
            file,
            keys: spec.keys.to_vec(),
            buffer: Vec::with_capacity(64),
        })
    }

    fn write_all(&mut self) -> io::Result<()> {
        let bytes = self.buffer.len() * std::mem::size_of::<libc::input_event>();
        let mut written = 0usize;
        while written < bytes {
            // SAFETY: buffer holds `bytes` bytes of plain input_event structs.
            let rc = unsafe {
                libc::write(
                    self.file.as_raw_fd(),
                    (self.buffer.as_ptr() as *const u8).add(written) as *const libc::c_void,
                    bytes - written,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if rc == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            written += rc as usize;
        }
        Ok(())
    }
}

fn raw_event(event: Ev) -> libc::input_event {
    libc::input_event {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: event.kind,
        code: event.code,
        value: event.value,
    }
}

impl EventSink for Uinput {
    fn emit(&mut self, events: &[Ev]) -> io::Result<()> {
        self.buffer.clear();
        self.buffer.extend(events.iter().copied().map(raw_event));
        self.buffer.push(raw_event(Ev {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }));
        self.write_all()
    }
    fn feedback_fd(&self) -> Option<RawFd> {
        Some(self.file.as_raw_fd())
    }
    fn read_feedback(&mut self, out: &mut Vec<Ev>) -> io::Result<()> {
        let mut events = [raw_event(Ev::key(0, 0)); 16];
        loop {
            // SAFETY: reading plain structs into a correctly sized stack buffer.
            let rc = unsafe {
                libc::read(
                    self.file.as_raw_fd(),
                    events.as_mut_ptr() as *mut libc::c_void,
                    std::mem::size_of_val(&events),
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::WouldBlock => Ok(()),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(error),
                };
            }
            let count = rc as usize / std::mem::size_of::<libc::input_event>();
            if count == 0 {
                return Ok(());
            }
            out.extend(events[..count].iter().map(|event| Ev {
                kind: event.type_,
                code: event.code,
                value: event.value,
            }));
        }
    }
    fn keys(&self) -> &[u16] {
        &self.keys
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        // SAFETY: plain ioctl on a live fd; the fd is closed right after by File's drop.
        unsafe { libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY) };
    }
}

/// Name of the companion's virtual keyboard: "hyprdeck-corsair K65 PLUS Wireless".
pub fn keyboard_name(model: crate::devices::Model) -> String {
    let name = model.display_name();
    format!(
        "hyprdeck-corsair {}",
        name.strip_prefix("Corsair ").unwrap_or(name)
    )
}

/// Create the virtual keyboard (keys + consumer keys + LEDs).
pub fn create_keyboard(model: crate::devices::Model, keys: &[u16]) -> Result<Box<dyn EventSink>> {
    let name = keyboard_name(model);
    Ok(Box::new(Uinput::create(&DeviceSpec {
        name: &name,
        keys,
        rel: &[],
        leds: true,
        product: 0xfe01,
    })?))
}

/// Create the virtual wheel used for Scroll/Zoom dial modes. It advertises pointer motion and
/// buttons so libinput classifies it as a mouse (it only ever sends wheel events).
pub fn create_wheel(model: crate::devices::Model) -> Result<Box<dyn EventSink>> {
    let name = format!("{} dial", keyboard_name(model));
    Ok(Box::new(Uinput::create(&DeviceSpec {
        name: &name,
        keys: &[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
        rel: &[REL_X, REL_Y, REL_WHEEL, REL_WHEEL_HI_RES],
        leds: false,
        product: 0xfe02,
    })?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records every emitted batch.
    pub(crate) struct Recorder {
        pub batches: Arc<Mutex<Vec<Vec<Ev>>>>,
        pub keys: Vec<u16>,
    }

    impl Recorder {
        pub fn new() -> (Self, Arc<Mutex<Vec<Vec<Ev>>>>) {
            let batches = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    batches: batches.clone(),
                    keys: keyboard_keys([]),
                },
                batches,
            )
        }
    }

    impl EventSink for Recorder {
        fn emit(&mut self, events: &[Ev]) -> io::Result<()> {
            self.batches.lock().unwrap().push(events.to_vec());
            Ok(())
        }
        fn keys(&self) -> &[u16] {
            &self.keys
        }
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<libc::uinput_setup>(), 92);
        assert_eq!(
            UI_DEV_SETUP,
            (1 << 30) | (92 << 16) | ((b'U' as libc::c_ulong) << 8) | 3
        );
        assert_eq!(UI_SET_LEDBIT & 0xff, 105);
        assert_eq!(std::mem::size_of::<libc::input_event>(), 24);
    }

    #[test]
    fn names_and_key_sets() {
        assert_eq!(
            keyboard_name(crate::devices::Model::K65PlusWireless),
            "hyprdeck-corsair K65 PLUS Wireless"
        );
        assert_eq!(
            keyboard_name(crate::devices::Model::K100Rgb),
            "hyprdeck-corsair K100 RGB"
        );
        let keys = keyboard_keys([0x1d0, 30]);
        assert_eq!(keys.len(), 256);
        assert_eq!(keys[0], 1);
        assert!(keys.contains(&KEY_PLAYPAUSE) && keys.contains(&0x1d0));
    }
}

#[cfg(test)]
mod ownership_regressions {
    use super::tests::Recorder;
    use super::*;
    struct FailOnceUp {
        recorder: Recorder,
        failed: bool,
    }
    impl EventSink for FailOnceUp {
        fn emit(&mut self, events: &[Ev]) -> io::Result<()> {
            self.recorder.emit(events)?;
            if !self.failed
                && events
                    .iter()
                    .any(|event| event.kind == EV_KEY && event.value == 0)
            {
                self.failed = true;
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            Ok(())
        }
        fn keys(&self) -> &[u16] {
            self.recorder.keys()
        }
    }
    #[test]
    fn physical_macro_and_dial_owners_cannot_release_each_other() {
        let (recorder, batches) = Recorder::new();
        let sink = share(Box::new(recorder));
        let a = new_owner();
        let b = new_owner();
        let c = new_owner();
        for owner in [a, b, c] {
            emit_owned(&sink, owner, &[Ev::key(29, 1)]).unwrap();
        }
        release_owned(&sink, a).unwrap();
        release_owned(&sink, b).unwrap();
        assert_eq!(*batches.lock().unwrap(), vec![vec![Ev::key(29, 1)]]);
        release_owned(&sink, c).unwrap();
        assert_eq!(
            batches.lock().unwrap().last().unwrap(),
            &vec![Ev::key(29, 0)]
        );
    }
    #[test]
    fn failed_release_is_retained_until_retry_or_shared_device_destruction() {
        let (recorder, batches) = Recorder::new();
        let sink = share(Box::new(FailOnceUp {
            recorder,
            failed: false,
        }));
        let owner = new_owner();
        emit_owned(&sink, owner, &[Ev::key(29, 1)]).unwrap();
        assert!(release_owned(&sink, owner).is_err());
        {
            let output = sink.lock().unwrap();
            assert!(
                output.failed
                    && output.owners[&owner].contains(&29)
                    && output.uncertain.contains(&29)
            );
        }
        release_owned(&sink, owner).unwrap();
        assert!(sink.lock().unwrap().owners.is_empty());
        assert_eq!(
            batches
                .lock()
                .unwrap()
                .iter()
                .filter(|batch| batch.contains(&Ev::key(29, 0)))
                .count(),
            2
        );
        let clone = sink.clone();
        sink.lock().unwrap().destroy();
        assert!(clone.lock().unwrap().device.is_none());
    }
}
