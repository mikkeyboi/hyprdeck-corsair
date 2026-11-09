//! Software-mode ownership of one keyboard: mode 2 entry, keepalive, offline/wake handling,
//! per-key frames on resource 0x22 (handle 1 kept open), brightness steps and proxied plugin
//! transfers. Releasing (explicitly or on drop) closes handle 1 and restores hardware mode.

use crate::bragi::{
    self, Bragi, CMD_OPEN, CMD_SET, MODE_HARDWARE, MODE_SOFTWARE, PROP_BRIGHTNESS,
    PROP_BRIGHTNESS_2, PROP_MODE, RES_DIRECT_LIGHTING, Status, StatusError, Transport,
};
use crate::devices::{LinkKind, Model};
use crate::ipc::Operation;
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

/// Handle used for the direct-lighting resource while owned (DESIGN §7).
pub const FRAME_HANDLE: u8 = 1;
/// Retry period while the keyboard is offline or a frame write failed.
pub const RETRY: Duration = Duration::from_secs(2);
/// Consecutive keepalive timeouts that count as "offline".
const MAX_TIMEOUTS: u32 = 2;

pub fn keepalive_interval(link: LinkKind) -> Duration {
    match link {
        LinkKind::Receiver => Duration::from_secs(5),
        LinkKind::Usb => Duration::from_secs(15),
    }
}

/// Counts one persistent write (storage::WriteBudget::record in production).
pub type Budget = Box<dyn FnMut(&str) -> Result<u32> + Send>;

pub fn real_budget() -> Budget {
    Box::new(crate::storage::WriteBudget::record)
}

/// Step the backlight by `delta_percent` with read-compare-skip and the write budget.
/// Must run in software mode. Returns the new raw value (0..=1000) when something was written.
pub fn adjust_brightness<T: Transport>(
    bragi: &mut Bragi<T>,
    device_id: &str,
    delta_percent: i32,
    budget: &mut Budget,
) -> Result<Option<u32>> {
    let current = bragi.get_u32(PROP_BRIGHTNESS)?.min(1000);
    let target = (current as i32 + delta_percent * 10).clamp(0, 1000) as u32;
    if target == current {
        return Ok(None);
    }
    budget(device_id)?;
    bragi.set(PROP_BRIGHTNESS, target, 2)?;
    if bragi.model == Model::K100Rgb && bragi.get_u32(PROP_BRIGHTNESS_2).ok() != Some(target) {
        bragi.set(PROP_BRIGHTNESS_2, target, 2)?;
    }
    Ok(Some(target))
}

pub struct Owner<T: Transport> {
    bragi: Bragi<T>,
    device_id: String,
    keepalive: Duration,
    budget: Budget,
    online: bool,
    /// We put (or found) the keyboard in software mode and must restore hardware mode.
    entered: bool,
    released: bool,
    last_ping: Instant,
    next_retry: Instant,
    timeouts: u32,
    handle_open: bool,
    written: Option<Vec<u8>>,
    desired: Option<Vec<u8>>,
    frame_retry_at: Option<Instant>,
    descriptor_stale: bool,
    restoration_pending: bool,
    sleep_ms: Option<u32>,
    last_error: Option<String>,
}

impl<T: Transport> Owner<T> {
    /// Take ownership: enter software mode now, or wait (offline) if the keyboard is asleep.
    pub fn acquire(
        transport: T,
        model: Model,
        link: LinkKind,
        device_id: &str,
        budget: Budget,
        now: Instant,
    ) -> Result<Self> {
        let mut owner = Self {
            bragi: Bragi::new(transport, model),
            device_id: device_id.to_string(),
            keepalive: keepalive_interval(link),
            budget,
            online: false,
            entered: false,
            released: false,
            last_ping: now,
            next_retry: now,
            timeouts: 0,
            handle_open: false,
            written: None,
            desired: None,
            frame_retry_at: None,
            descriptor_stale: false,
            restoration_pending: false,
            sleep_ms: None,
            last_error: None,
        };
        match owner.enter() {
            Ok(()) => {
                owner.online = true;
                owner.last_ping = now;
            }
            Err(error) if bragi::is_offline(&error) || bragi::is_timeout(&error) => {
                owner.go_offline(now);
            }
            Err(error) if owner.entered => {
                // Preserve the recovery owner if acquisition cleanup cannot prove restoration.
                owner.note_error("Acquiring live mode failed", &error);
                if owner.release().is_ok() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        Ok(owner)
    }

    /// GET 03; SET 03=2 when in hardware mode. Device-side handles are unknown afterwards.
    fn enter(&mut self) -> Result<()> {
        self.handle_open = false;
        self.written = None;
        match self.bragi.mode()? {
            MODE_HARDWARE => {
                // A timeout leaves the outcome unknown: restore on release either way.
                self.entered = true;
                if let Err(error) = self.bragi.set(PROP_MODE, u32::from(MODE_SOFTWARE), 1) {
                    if error.chain().any(|cause| cause.is::<StatusError>()) {
                        self.entered = false;
                    }
                    return Err(error);
                }
                let mode = self.bragi.mode()?;
                if mode != MODE_SOFTWARE {
                    bail!("Keyboard did not enter software mode (mode {mode})");
                }
            }
            // Left over from a crashed companion: take it over (and restore it later).
            MODE_SOFTWARE => self.entered = true,
            other => bail!("Keyboard is in unexpected mode {other} — run Rescue"),
        }
        // Clean stale handles before exposing any resource transaction, without reading bindings.
        for handle in 0..=2 {
            self.bragi.close_handle(handle)?;
        }
        Ok(())
    }

    fn go_offline(&mut self, now: Instant) {
        if self.online {
            eprintln!(
                "{}: keyboard asleep or disconnected; retrying every 2 s",
                self.device_id
            );
        }
        self.online = false;
        self.handle_open = false;
        self.written = None;

        self.timeouts = 0;
        self.next_retry = now + RETRY;
    }

    fn note_error(&mut self, what: &str, error: &anyhow::Error) {
        let text = format!("{what}: {error:#}");
        if self.last_error.as_deref() != Some(text.as_str()) {
            eprintln!("{}: {text}", self.device_id);
            self.last_error = Some(text);
        }
    }

    /// React to an error from a device operation; true when it meant "offline".
    fn check_offline(&mut self, error: &anyhow::Error, now: Instant) -> bool {
        if bragi::is_terminal_disconnect(error) {
            self.descriptor_stale = true;
            self.note_error("Terminal HID disconnect; reacquire required", error);
            self.go_offline(now);
            return true;
        }
        if bragi::is_offline(error) {
            self.go_offline(now);
            return true;
        }
        false
    }

    pub fn terminal_disconnect(&self) -> bool {
        self.descriptor_stale
    }
    pub fn restoration_pending(&self) -> bool {
        self.restoration_pending
    }
    pub fn mode_status(&self) -> u8 {
        if self.restoration_pending || !self.online && !self.released {
            0
        } else if self.released {
            MODE_HARDWARE
        } else {
            MODE_SOFTWARE
        }
    }
    pub fn frame_applied(&self) -> bool {
        self.online && self.handle_open && self.desired.is_some() && self.desired == self.written
    }
    pub fn is_online(&self) -> bool {
        self.online
    }
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// When `tick` next needs to run.
    pub fn next_deadline(&self) -> Instant {
        if !self.online {
            return self.next_retry;
        }
        let mut deadline = self.last_ping + self.keepalive;
        if let Some(at) = self.frame_retry_at {
            deadline = deadline.min(at);
        }
        deadline
    }

    /// Keepalive, offline retry and pending frame retries. Returns true when the keyboard came
    /// back online (mode and frame were re-applied).
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.restoration_pending || self.descriptor_stale {
            return false;
        }
        if !self.online {
            if now < self.next_retry {
                return false;
            }
            self.next_retry = now + RETRY;
            let woke = self.bragi.ping().and_then(|()| self.enter());
            return match woke {
                Ok(()) => {
                    eprintln!("{}: keyboard is back; live mode re-applied", self.device_id);
                    self.online = true;
                    self.last_ping = now;
                    self.last_error = None;
                    self.sync_frame(now);
                    true
                }
                Err(error) => {
                    self.check_offline(&error, now);
                    if !bragi::is_offline(&error) && !bragi::is_timeout(&error) {
                        self.note_error("Re-entering live mode", &error);
                    }
                    false
                }
            };
        }
        if now >= self.last_ping + self.keepalive {
            self.last_ping = now;
            let health = self.bragi.ping().and_then(|()| {
                let mode = self.bragi.mode()?;
                if mode != MODE_SOFTWARE {
                    self.enter()?;
                    self.frame_retry_at = Some(now);
                }
                Ok(())
            });
            match health {
                Ok(()) => self.timeouts = 0,
                Err(error) if bragi::is_timeout(&error) => {
                    self.timeouts += 1;
                    if self.timeouts >= MAX_TIMEOUTS {
                        self.go_offline(now);
                        return false;
                    }
                }
                Err(error) => {
                    if !self.check_offline(&error, now) {
                        self.note_error("Keepalive", &error);
                    }
                    if !self.online {
                        return false;
                    }
                }
            }
        }
        if self.frame_retry_at.is_some_and(|at| now >= at) {
            self.sync_frame(now);
        }
        false
    }

    /// Set (or clear with None) the per-key frame data; written now if it changed.
    pub fn set_frame(&mut self, frame: Option<Vec<u8>>, now: Instant) {
        self.desired = frame;
        self.sync_frame(now);
    }

    fn sync_frame(&mut self, now: Instant) {
        self.frame_retry_at = None;
        if !self.online || self.descriptor_stale || self.restoration_pending {
            return;
        }
        // An earlier uncertain OPEN/CLOSE must be resolved even without a desired frame.
        if !self.handle_open
            && self.bragi.has_bindings()
            && let Err(error) = self.bragi.cleanup_handles()
        {
            self.check_offline(&error, now);
            self.note_error("Retrying uncertain resource cleanup", &error);
            self.frame_retry_at = Some(now + RETRY);
            return;
        }
        let Some(desired) = self.desired.clone() else {
            if self.handle_open {
                self.written = None;
                match self.bragi.close_handle(FRAME_HANDLE) {
                    Ok(()) => self.handle_open = false,
                    Err(error) => {
                        self.check_offline(&error, now);
                        self.note_error("Closing lighting handle", &error);
                        self.frame_retry_at = Some(now + RETRY);
                    }
                }
            }
            return;
        };
        if self.handle_open && self.written.as_ref() == Some(&desired) {
            return;
        }
        let declared = desired.len() as u32;
        let result = if self.handle_open {
            self.bragi.write_handle(FRAME_HANDLE, declared, &desired)
        } else {
            let first = self.bragi.write_resource(
                FRAME_HANDLE,
                RES_DIRECT_LIGHTING,
                declared,
                &desired,
                true,
            );
            match first {
                // Status 3: handle 1 still open from an earlier session; close it and retry once.
                Err(error) if is_busy(&error) => {
                    let _ = self.bragi.close_handle(FRAME_HANDLE);
                    self.bragi.write_resource(
                        FRAME_HANDLE,
                        RES_DIRECT_LIGHTING,
                        declared,
                        &desired,
                        true,
                    )
                }
                other => other,
            }
        };
        match result {
            Ok(()) => {
                self.handle_open = true;
                self.written = Some(desired);
            }
            Err(error) => {
                if self.handle_open {
                    let _ = self.bragi.close_handle(FRAME_HANDLE);
                }
                self.handle_open = false;
                self.written = None;
                if !self.check_offline(&error, now) {
                    self.note_error("Writing the per-key frame", &error);
                    self.frame_retry_at = Some(now + RETRY);
                }
            }
        }
    }

    /// Raw IPC is property-only. Resource handles belong solely to atomic operations.
    pub fn xfer(&mut self, payload: &[u8], now: Instant) -> Result<Vec<u8>> {
        bragi::check_allowed(self.bragi.model, payload)?;
        if self.restoration_pending || self.descriptor_stale {
            bail!("Mode uncertain; restore/reacquire before requests");
        }
        if !matches!(payload[0], bragi::CMD_GET | CMD_SET | bragi::CMD_PING) {
            bail!("Raw resource I/O is forbidden; use an atomic resource operation");
        }
        let result = (|| {
            if payload[0] == CMD_SET {
                let prop = payload[1];
                if prop == PROP_MODE {
                    bail!("The live-mode companion owns this keyboard's mode");
                }
                let mut bytes = [0u8; 4];
                for (a, b) in bytes.iter_mut().zip(&payload[3..]) {
                    *a = *b;
                }
                let value = u32::from_le_bytes(bytes);
                // Authoritative compare, device-reported cap and budget at the hardware boundary.
                if prop == bragi::PROP_POLLING
                    && value > self.bragi.get_u32(bragi::PROP_MAX_POLLING)?
                {
                    bail!("Polling rate exceeds the keyboard's reported maximum");
                }
                let current = if prop == bragi::PROP_SLEEP_MS {
                    self.sleep_ms
                } else {
                    Some(self.bragi.get_u32(prop)?)
                };
                if current == Some(value) {
                    return Ok(vec![0, CMD_SET, 0]);
                }
                (self.budget)(&self.device_id)?;
            }
            let reply = self.bragi.xfer(payload)?;
            if reply.status() != Status::Ok {
                return Err(StatusError {
                    cmd: payload[0],
                    arg: payload.get(1).copied().unwrap_or(0),
                    status: reply.status(),
                }
                .into());
            }
            if payload[0] == CMD_SET && payload[1] == bragi::PROP_SLEEP_MS {
                self.sleep_ms = Some(u32::from_le_bytes(
                    payload[3..7].try_into().expect("validated sleep SET width"),
                ));
            }
            Ok(reply.raw)
        })();
        if let Err(error) = &result {
            self.check_offline(error, now);
        }
        result
    }

    /// Execute one whole transaction under the worker's deadline; no handles escape this call.
    pub fn rpc(
        &mut self,
        operation: &Operation,
        deadline: Instant,
        now: Instant,
    ) -> Result<Vec<u8>> {
        bragi::check_deadline(deadline)?;
        if self.restoration_pending || self.descriptor_stale {
            bail!("Mode uncertain; restore/reacquire before requests");
        }
        let old = self.bragi.deadline();
        self.bragi.set_deadline(Some(deadline));
        let result = (|| match operation {
            Operation::Xfer { payload } => self.xfer(payload, now),
            Operation::ReadResource { resource } => {
                if *resource != bragi::RES_K65_EFFECT || self.bragi.model != Model::K65PlusWireless
                {
                    bail!("Resource is not readable");
                }
                self.prepare_resource()?;
                self.bragi.read_resource(*resource)
            }
            Operation::WriteResource {
                resource,
                declared_len,
                bytes,
            } => {
                // Clients cannot commandeer transient lighting or address forbidden resources.
                if *resource != bragi::RES_K65_EFFECT || self.bragi.model != Model::K65PlusWireless
                {
                    bail!("Resource is not an allowed persistent effect");
                }
                let crate::effects::Decoded::Known(effect) = crate::effects::decode(bytes) else {
                    bail!("Unknown effect descriptor");
                };
                let (expected_len, expected_bytes) = crate::effects::encode_k65_write(&effect);
                if *declared_len != expected_len || *bytes != expected_bytes {
                    bail!("Invalid effect framing");
                }
                self.prepare_resource()?;
                let current = self.bragi.read_resource(*resource)?;
                if crate::effects::decode(&current) == crate::effects::decode(bytes) {
                    return Ok(Vec::new());
                }
                bragi::check_deadline(deadline)?;
                (self.budget)(&self.device_id)?;
                self.bragi
                    .write_resource(1, *resource, *declared_len, bytes, false)?;
                Ok(Vec::new())
            }
        })();
        self.bragi.set_deadline(old);
        if let Err(error) = &result {
            self.check_offline(error, now);
            self.note_error("Atomic request", error);
        } else {
            self.last_error = None;
        }
        // Resource read/write closes handle 1. Invalidate frame regardless of outcome.
        if !matches!(operation, Operation::Xfer { .. }) {
            self.frame_retry_at = Some(now);
        }
        result
    }
    fn prepare_resource(&mut self) -> Result<()> {
        // Never discard bookkeeping on uncertain/rejected CLOSE; Bragi prevents rebinding.
        if self.handle_open {
            self.written = None;
            self.bragi.close_handle(FRAME_HANDLE)?;
            self.handle_open = false;
        }
        self.bragi.cleanup_handles()
    }

    /// Dial/remap brightness step through the owned transport.
    pub fn brightness_step(&mut self, delta_percent: i32, now: Instant) -> Result<()> {
        let result = adjust_brightness(
            &mut self.bragi,
            &self.device_id,
            delta_percent,
            &mut self.budget,
        );
        if let Err(error) = &result {
            self.check_offline(error, now);
        }
        result.map(|_| ())
    }

    /// Restore hardware mode; only verified completion clears recovery responsibility.
    pub fn release(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        self.restoration_pending = self.entered;
        let old_deadline = self.bragi.deadline();
        self.bragi
            .set_deadline(Some(Instant::now() + Duration::from_secs(3)));
        let closed = self.bragi.cleanup_handles();
        self.handle_open = false;
        self.written = None;
        let restored = if self.entered {
            self.bragi
                .set(PROP_MODE, u32::from(MODE_HARDWARE), 1)
                .and_then(|()| {
                    let mode = self.bragi.mode()?;
                    if mode != MODE_HARDWARE {
                        bail!("Keyboard stayed in mode {mode} — run Rescue");
                    }
                    Ok(())
                })
        } else {
            Ok(())
        };
        let result = match (restored, closed) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Err(e), Err(c)) => Err(e.context(format!("also resource cleanup failed: {c:#}"))),
        };
        self.bragi.set_deadline(old_deadline);
        match &result {
            Ok(()) => {
                self.released = true;
                self.entered = false;
                self.restoration_pending = false;
                self.last_error = None;
            }
            Err(e) => {
                self.restoration_pending = true;
                self.note_error("Hardware-mode restoration failed — run Rescue", e);
            }
        }
        result
    }
}

fn is_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<StatusError>())
        .any(|status| status.status == Status::Failed && status.cmd == CMD_OPEN)
}

impl<T: Transport> Drop for Owner<T> {
    fn drop(&mut self) {
        if !self.released
            && let Err(error) = self.release()
        {
            eprintln!(
                "{}: could not restore hardware mode: {error:#} — run `hyprdeck-corsair rescue`",
                self.device_id
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bragi::tests::FakeKeyboard;
    use crate::bragi::{CMD_PING, CMD_WRITE, CMD_WRITE_MORE, Reply};
    use std::sync::{Arc, Mutex};

    /// Transport over a shared fake keyboard so tests can inspect it after the owner is gone.
    #[derive(Clone)]
    pub(crate) struct SharedFake(pub Arc<Mutex<FakeKeyboard>>);

    impl SharedFake {
        pub fn new() -> Self {
            Self(Arc::new(Mutex::new(FakeKeyboard::new())))
        }
        pub fn with<R>(&self, f: impl FnOnce(&mut FakeKeyboard) -> R) -> R {
            f(&mut self.0.lock().unwrap())
        }
    }

    impl Transport for SharedFake {
        fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
            self.0.lock().unwrap().xfer(payload)
        }
        fn sleep(&mut self, duration: Duration) {
            self.0.lock().unwrap().sleep(duration);
        }
    }

    pub(crate) fn counting_budget() -> (Budget, Arc<Mutex<u32>>) {
        let count = Arc::new(Mutex::new(0));
        let inner = count.clone();
        (
            Box::new(move |_| {
                let mut count = inner.lock().unwrap();
                *count += 1;
                Ok(*count)
            }),
            count,
        )
    }

    fn owner(fake: &SharedFake, link: LinkKind, now: Instant) -> Owner<SharedFake> {
        let (budget, _) = counting_budget();
        Owner::acquire(
            fake.clone(),
            Model::K65PlusWireless,
            link,
            "k65plus-test",
            budget,
            now,
        )
        .unwrap()
    }

    #[test]
    fn acquire_enters_software_mode_and_release_restores_it() {
        let fake = SharedFake::new();
        let now = Instant::now();
        let mut owner = owner(&fake, LinkKind::Receiver, now);
        assert!(owner.is_online());
        assert_eq!(fake.with(|k| k.mode), MODE_SOFTWARE);
        owner.release().unwrap();
        assert_eq!(fake.with(|k| k.mode), MODE_HARDWARE);
        let log = fake.with(|k| k.log.clone());
        let n = log.len();
        assert_eq!(log[n - 2], vec![CMD_SET, PROP_MODE, 0, 1]);
        assert_eq!(log[n - 1], vec![bragi::CMD_GET, PROP_MODE]);
        owner.release().unwrap();
        assert_eq!(fake.with(|k| k.log.len()), n, "release is idempotent");
    }

    #[test]
    fn drop_releases_mode_and_frame_handle() {
        let fake = SharedFake::new();
        let now = Instant::now();
        {
            let mut owner = owner(&fake, LinkKind::Usb, now);
            owner.set_frame(Some(vec![0x12, 0, 1, 2, 3]), now);
            assert_eq!(
                fake.with(|k| k.open.get(&1).copied()),
                Some(RES_DIRECT_LIGHTING)
            );
        }
        assert_eq!(fake.with(|k| k.mode), MODE_HARDWARE);
        assert!(
            fake.with(|k| k.open.is_empty()),
            "handle 1 closed before mode 1"
        );
    }

    #[test]
    fn keepalive_follows_the_link_interval() {
        let fake = SharedFake::new();
        let start = Instant::now();
        let mut owner = owner(&fake, LinkKind::Receiver, start);
        assert_eq!(owner.next_deadline(), start + Duration::from_secs(5));
        owner.tick(start + Duration::from_secs(4));
        assert!(!fake.with(|k| k.cmds().contains(&CMD_PING)));
        owner.tick(start + Duration::from_secs(5));
        assert_eq!(
            fake.with(|k| k.cmds().iter().filter(|c| **c == CMD_PING).count()),
            1
        );
        assert_eq!(owner.next_deadline(), start + Duration::from_secs(10));
        assert_eq!(keepalive_interval(LinkKind::Usb), Duration::from_secs(15));
    }

    #[test]
    fn frames_are_written_once_and_only_on_change() {
        let fake = SharedFake::new();
        let now = Instant::now();
        let mut owner = owner(&fake, LinkKind::Usb, now);
        let frame: Vec<u8> = [0x12, 0x00].into_iter().chain(vec![9; 369]).collect();
        owner.set_frame(Some(frame.clone()), now);
        owner.set_frame(Some(frame.clone()), now);
        let (opens, writes) = fake.with(|k| {
            let cmds = k.cmds();
            (
                cmds.iter().filter(|c| **c == CMD_OPEN).count(),
                cmds.iter().filter(|c| **c == CMD_WRITE).count(),
            )
        });
        assert_eq!((opens, writes), (1, 1));
        let mut expected = (frame.len() as u32).to_le_bytes().to_vec();
        expected.extend_from_slice(&frame);
        assert_eq!(
            fake.with(|k| k.written[&RES_DIRECT_LIGHTING].clone()),
            expected
        );
        let mut changed = frame.clone();
        changed[5] = 1;
        owner.set_frame(Some(changed), now);
        let (opens, writes) = fake.with(|k| {
            let cmds = k.cmds();
            (
                cmds.iter().filter(|c| **c == CMD_OPEN).count(),
                cmds.iter().filter(|c| **c == CMD_WRITE).count(),
            )
        });
        assert_eq!((opens, writes), (1, 2), "handle stays open between frames");
        assert!(fake.with(|k| k.cmds().contains(&CMD_WRITE_MORE)));
        owner.set_frame(None, now);
        assert!(fake.with(|k| k.open.is_empty()));
    }

    #[test]
    fn offline_pauses_then_reapplies_mode_and_frame_on_wake() {
        let fake = SharedFake::new();
        let start = Instant::now();
        let mut owner = owner(&fake, LinkKind::Usb, start);
        owner.set_frame(Some(vec![0x12, 0, 5, 5, 5]), start);
        // Keyboard falls asleep and comes back in hardware mode with no open handles.
        fake.with(|k| k.offline = true);
        let t1 = start + Duration::from_secs(15);
        owner.tick(t1);
        assert!(!owner.is_online());
        assert_eq!(owner.next_deadline(), t1 + RETRY);
        let before = fake.with(|k| k.log.len());
        assert!(
            !owner.tick(t1 + Duration::from_secs(1)),
            "no retry before 2 s"
        );
        assert_eq!(fake.with(|k| k.log.len()), before);
        assert!(!owner.tick(t1 + RETRY), "still offline");
        fake.with(|k| {
            k.offline = false;
            k.mode = MODE_HARDWARE;
            k.open.clear();
            k.written.clear();
        });
        assert!(owner.tick(t1 + RETRY * 2), "woke");
        assert!(owner.is_online());
        assert_eq!(fake.with(|k| k.mode), MODE_SOFTWARE);
        assert_eq!(
            fake.with(|k| k.written.get(&RES_DIRECT_LIGHTING).cloned()),
            Some([5u32.to_le_bytes().to_vec(), vec![0x12, 0, 5, 5, 5]].concat())
        );
    }

    #[test]
    fn acquire_while_asleep_waits_for_the_keyboard() {
        let fake = SharedFake::new();
        fake.with(|k| k.offline = true);
        let start = Instant::now();
        let mut owner = owner(&fake, LinkKind::Receiver, start);
        assert!(!owner.is_online());
        fake.with(|k| k.offline = false);
        assert!(owner.tick(start + RETRY));
        assert_eq!(fake.with(|k| k.mode), MODE_SOFTWARE);
        drop(owner);
        assert_eq!(fake.with(|k| k.mode), MODE_HARDWARE);
    }

    #[test]
    fn xfer_is_allowlisted_and_lends_handle_one() {
        let fake = SharedFake::new();
        let now = Instant::now();
        let mut owner = owner(&fake, LinkKind::Usb, now);
        owner.set_frame(Some(vec![0x12, 0, 1, 1, 1]), now);
        let reply = owner
            .xfer(&[bragi::CMD_GET, bragi::PROP_BRIGHTNESS], now)
            .unwrap();
        assert_eq!(reply[1], bragi::CMD_GET);
        assert!(owner.xfer(&[bragi::CMD_OPEN, 1, 0x02, 0x00], now).is_err());
        assert!(owner.xfer(&[0x0B], now).is_err());
        assert!(owner.xfer(&[CMD_SET, PROP_MODE, 0, 1], now).is_err());
        assert_eq!(fake.with(|k| k.mode), MODE_SOFTWARE);
        // Atomic resource RPC never lends handles and invalidates the lighting cache.
        assert!(owner.xfer(&[CMD_OPEN, 1, 0x60, 0x6d], now).is_err());
        let data = owner
            .rpc(
                &Operation::ReadResource {
                    resource: bragi::RES_K65_EFFECT,
                },
                now + Duration::from_secs(2),
                now,
            )
            .unwrap();
        assert!(!data.is_empty());
        assert!(!owner.frame_applied());
        owner.tick(now);
        assert_eq!(
            fake.with(|k| k.open.get(&1).copied()),
            Some(RES_DIRECT_LIGHTING)
        );
    }

    #[test]
    fn brightness_steps_compare_skip_and_count_budget() {
        let fake = SharedFake::new();
        fake.with(|k| {
            k.mode = MODE_SOFTWARE;
        });
        let (mut budget, count) = counting_budget();
        let mut bragi = Bragi::new(fake.clone(), Model::K65PlusWireless);
        assert_eq!(
            adjust_brightness(&mut bragi, "k", 10, &mut budget).unwrap(),
            Some(800)
        );
        assert_eq!(fake.with(|k| k.props[&PROP_BRIGHTNESS]), 800);
        fake.with(|k| k.props.insert(PROP_BRIGHTNESS, 1000));
        assert_eq!(
            adjust_brightness(&mut bragi, "k", 10, &mut budget).unwrap(),
            None
        );
        assert_eq!(*count.lock().unwrap(), 1, "skipped write is not counted");
        assert_eq!(
            adjust_brightness(&mut bragi, "k", -200, &mut budget).unwrap(),
            Some(0)
        );
        let mut refuse: Budget = Box::new(|_| bail!("Daily limit"));
        assert!(adjust_brightness(&mut bragi, "k", 10, &mut refuse).is_err());
        assert_eq!(fake.with(|k| k.props[&PROP_BRIGHTNESS]), 0);
    }

    #[test]
    fn failed_entry_is_not_owned() {
        let fake = SharedFake::new();
        fake.with(|k| k.mode = 5);
        let (budget, _) = counting_budget();
        assert!(
            Owner::acquire(
                fake.clone(),
                Model::K65PlusWireless,
                LinkKind::Usb,
                "k",
                budget,
                Instant::now()
            )
            .is_err()
        );
        assert_eq!(fake.with(|k| k.cmds()), vec![bragi::CMD_GET]);
    }
}

#[cfg(test)]
mod atomic_recovery_regressions {
    use super::tests::{SharedFake, counting_budget};
    use super::*;
    fn acquire(fake: &SharedFake) -> (Owner<SharedFake>, std::sync::Arc<std::sync::Mutex<u32>>) {
        let (budget, count) = counting_budget();
        (
            Owner::acquire(
                fake.clone(),
                Model::K65PlusWireless,
                LinkKind::Usb,
                "k65-test",
                budget,
                Instant::now(),
            )
            .unwrap(),
            count,
        )
    }
    #[test]
    fn raw_and_atomic_persistence_share_authoritative_compare_cap_and_budget() {
        let fake = SharedFake::new();
        let (mut owner, count) = acquire(&fake);
        let now = Instant::now();
        let deadline = now + Duration::from_secs(2);
        owner
            .rpc(
                &Operation::Xfer {
                    payload: vec![CMD_SET, PROP_BRIGHTNESS, 0, 0xbc, 2],
                },
                deadline,
                now,
            )
            .unwrap();
        assert_eq!(*count.lock().unwrap(), 0);
        owner
            .rpc(
                &Operation::Xfer {
                    payload: vec![CMD_SET, PROP_BRIGHTNESS, 0, 0xf4, 1],
                },
                deadline,
                now,
            )
            .unwrap();
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(
            owner
                .rpc(
                    &Operation::Xfer {
                        payload: vec![CMD_SET, bragi::PROP_POLLING, 0, 7]
                    },
                    deadline,
                    now
                )
                .is_err()
        );
        assert_eq!(*count.lock().unwrap(), 1);
        let effect = crate::effects::default_effect(crate::effects::EffectKind::Static);
        let (declared_len, bytes) = crate::effects::encode_k65_write(&effect);
        let operation = Operation::WriteResource {
            resource: bragi::RES_K65_EFFECT,
            declared_len,
            bytes,
        };
        owner.rpc(&operation, deadline, now).unwrap();
        owner.rpc(&operation, deadline, now).unwrap();
        assert_eq!(*count.lock().unwrap(), 2);
        assert!(fake.with(|k| k.open.is_empty()));
    }
    #[test]
    fn sleep_write_only_cache_is_authoritative_without_get() {
        let fake = SharedFake::new();
        let (mut owner, count) = acquire(&fake);
        let now = Instant::now();
        let mut payload = vec![CMD_SET, bragi::PROP_SLEEP_MS, 0];
        payload.extend_from_slice(&60_000u32.to_le_bytes());
        for _ in 0..2 {
            owner
                .rpc(
                    &Operation::Xfer {
                        payload: payload.clone(),
                    },
                    now + Duration::from_secs(1),
                    now,
                )
                .unwrap();
        }
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(!fake.with(|k| {
            k.log
                .iter()
                .any(|p| p == &[bragi::CMD_GET, bragi::PROP_SLEEP_MS])
        }));
    }
    #[test]
    fn expired_and_forbidden_operations_never_send_or_consume_budget() {
        let fake = SharedFake::new();
        let (mut owner, count) = acquire(&fake);
        let now = Instant::now();
        let before = fake.with(|k| k.log.len());
        assert!(
            owner
                .rpc(
                    &Operation::Xfer {
                        payload: vec![CMD_SET, PROP_BRIGHTNESS, 0, 0, 0]
                    },
                    now,
                    now
                )
                .is_err()
        );
        for resource in [2, bragi::RES_DIRECT_LIGHTING, 0xffff] {
            assert!(
                owner
                    .rpc(
                        &Operation::ReadResource { resource },
                        now + Duration::from_secs(1),
                        now
                    )
                    .is_err()
            );
        }
        assert_eq!(fake.with(|k| k.log.len()), before);
        assert_eq!(*count.lock().unwrap(), 0);
    }
    #[test]
    fn failed_restoration_retains_mode_uncertainty_and_recovers() {
        let fake = SharedFake::new();
        let (mut owner, _) = acquire(&fake);
        fake.with(|k| k.stuck = true);
        assert!(owner.release().is_err());
        assert!(owner.restoration_pending());
        assert_eq!(owner.mode_status(), 0);
        assert!(!owner.frame_applied());
        assert!(!owner.tick(Instant::now() + Duration::from_secs(30)));
        fake.with(|k| k.stuck = false);
        owner.release().unwrap();
        assert_eq!(owner.mode_status(), MODE_HARDWARE);
    }
    #[test]
    fn ping_success_detects_mode_drift_and_reapplies_frame() {
        let fake = SharedFake::new();
        let (mut owner, _) = acquire(&fake);
        let now = Instant::now();
        owner.set_frame(Some(vec![0x12, 0, 1, 2, 3]), now);
        fake.with(|k| {
            k.mode = MODE_HARDWARE;
            k.open.clear();
        });
        owner.tick(now + keepalive_interval(LinkKind::Usb));
        assert_eq!(fake.with(|k| k.mode), MODE_SOFTWARE);
        assert!(owner.frame_applied());
    }
}
