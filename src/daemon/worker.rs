//! Per-device worker thread: computes what the live config needs, owns software mode (via
//! [`Owner`]), reads IF2 events, emulates the dial, keeps per-key frames current and runs the
//! remap forwarder. Hardware access goes through [`Platform`] so tests use fakes only.

use super::actions::{self, DialEvent, MacroRunner, Op};
use super::events::{self, DIAL_PRESS_INDEX, KeyTracker, REPORT_LEN, Report};
use super::frame;
use super::hub::KeyHub;
use super::output::{self, EventSink, KEY_LEFTCTRL, SharedSink};
use super::owner::{self, Budget, Owner};
use super::remap::{self, InputNode, Notice, RemapConfig, RemapHandle, RemapSetup, Side};
use super::util::{self, POLL_GONE, Waker};
use crate::bragi::{self, Bragi, PROP_LAYOUT, Transport};
use crate::config::{DeviceLive, DialMode};
use crate::devices::{Keyboard, Model, PhysicalLayout};
use crate::ipc::DeviceStatus;
use crate::layouts::{self, Layout};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the daemon waits for the hidraw lock (a plugin request may hold it briefly).
pub const LOCK_WAIT: Duration = Duration::from_millis(750);
/// Retry period for virtual devices and grabs that failed (permissions rarely change quickly).
const INPUT_RETRY: Duration = Duration::from_secs(10);
/// Retry period for taking ownership when the keyboard was busy.
const ACQUIRE_RETRY: Duration = Duration::from_secs(2);

/// Hardware access used by a worker; [`RealPlatform`] in production, fakes in tests.
pub trait Platform: Send + Sync + 'static {
    type Transport: Transport + Send + 'static;
    fn open_transport(&self, keyboard: &Keyboard) -> Result<Self::Transport>;
    /// The IF2 event node (non-blocking reads of 64-byte reports), if the keyboard has one.
    fn open_events(&self, keyboard: &Keyboard) -> Result<Option<File>>;
    fn create_keyboard(&self, model: Model, keys: &[u16]) -> Result<Box<dyn EventSink>>;
    fn create_wheel(&self, model: Model) -> Result<Box<dyn EventSink>>;
    /// Open (not grab) the evdev nodes carrying any of `codes`.
    fn open_nodes(
        &self,
        keyboard: &Keyboard,
        codes: &BTreeSet<u16>,
    ) -> Result<Vec<Box<dyn InputNode>>>;
    fn budget(&self) -> Budget;
    fn spawn(&self, argv: &[String]) -> Result<()>;
}

pub struct RealPlatform;

/// HIDIOCGRAWINFO = _IOR('H', 0x03, struct hidraw_devinfo)
const HIDIOCGRAWINFO: libc::c_ulong = 0x8008_4803;

#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}

impl Platform for RealPlatform {
    type Transport = bragi::Hidraw;
    fn open_transport(&self, keyboard: &Keyboard) -> Result<bragi::Hidraw> {
        bragi::Hidraw::open_keyboard(keyboard, LOCK_WAIT)
    }
    fn open_events(&self, keyboard: &Keyboard) -> Result<Option<File>> {
        let Some(node) = &keyboard.event_node else {
            return Ok(None);
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(node)
            .with_context(|| format!("Opening the event node {}", node.display()))?;
        let mut info = HidrawDevinfo::default();
        // SAFETY: HidrawDevinfo has the kernel hidraw_devinfo layout; `file` is a live fd.
        if unsafe { libc::ioctl(file.as_raw_fd(), HIDIOCGRAWINFO, &mut info) } < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("Reading HID identity of {}", node.display()));
        }
        if (info.vendor as u16, info.product as u16) != (keyboard.vid, keyboard.pid) {
            bail!(
                "{} is not this keyboard's event node — replug it",
                node.display()
            );
        }
        Ok(Some(file))
    }
    fn create_keyboard(&self, model: Model, keys: &[u16]) -> Result<Box<dyn EventSink>> {
        output::create_keyboard(model, keys)
    }
    fn create_wheel(&self, model: Model) -> Result<Box<dyn EventSink>> {
        output::create_wheel(model)
    }
    fn open_nodes(
        &self,
        keyboard: &Keyboard,
        codes: &BTreeSet<u16>,
    ) -> Result<Vec<Box<dyn InputNode>>> {
        remap::open_nodes(&keyboard.evdev_nodes, codes)
    }
    fn budget(&self) -> Budget {
        owner::real_budget()
    }
    fn spawn(&self, argv: &[String]) -> Result<()> {
        actions::spawn_detached(argv)
    }
}

pub enum Command {
    Rpc {
        operation: crate::ipc::Operation,
        deadline: Instant,
        reply: SyncSender<Result<Vec<u8>>>,
    },
    Reload(DeviceLive),
    Side {
        side: Side,
        generation: u64,
    },
    RemapStopped(u64),
    RemapApplied(u64),
    Shutdown,
}

/// Sender side of a worker: command channel + waker.
#[derive(Clone)]
pub struct Mailbox {
    tx: Sender<Command>,
    waker: Arc<Waker>,
    shutdown: Arc<AtomicBool>,
}

impl Mailbox {
    pub fn new() -> Result<(Self, Receiver<Command>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = Arc::new(Waker::new().context("Creating a worker waker")?);
        Ok((
            Self {
                tx,
                waker,
                shutdown: Arc::new(AtomicBool::new(false)),
            },
            rx,
        ))
    }
    pub fn send(&self, command: Command) -> bool {
        if matches!(command, Command::Shutdown) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
        let sent = self.tx.send(command).is_ok();
        self.waker.wake();
        sent
    }
}

/// Which live features apply to a keyboard on its current link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Needs {
    pub dial_capable: bool,
    pub software_mode: bool,
    pub lighting: bool,
    pub grab: bool,
}

pub fn needs(keyboard: &Keyboard, live: &DeviceLive) -> Needs {
    let caps = keyboard.model.capabilities(keyboard.link);
    // Dial emulation is hardware-verified on the K65 PLUS only (DESIGN §7).
    let dial_capable = caps.dial && keyboard.model == Model::K65PlusWireless;
    let lighting = caps.live_lighting && live.lighting.enabled;
    Needs {
        dial_capable,
        software_mode: live.needs_software_mode(dial_capable, caps.live_lighting),
        lighting,
        grab: live.needs_grab(),
    }
}

pub struct Worker<P: Platform> {
    platform: Arc<P>,
    keyboard: Keyboard,
    live: DeviceLive,
    rx: Receiver<Command>,
    mailbox: Mailbox,
    status: Arc<Mutex<DeviceStatus>>,
    hub: Arc<KeyHub>,
    layout: &'static Layout,
    owner: Option<Owner<P::Transport>>,
    events: Option<File>,
    tracker: KeyTracker,
    sink: Option<SharedSink>,
    runner: Option<MacroRunner>,
    wheel: Option<Box<dyn EventSink>>,
    remap: Option<RemapHandle>,
    remap_dirty: bool,
    remap_codes: BTreeSet<u16>,
    remap_transition: bool,
    remap_epoch: u64,
    frame_dirty: bool,
    acquire_at: Instant,
    sink_at: Instant,
    wheel_at: Instant,
    remap_at: Instant,
    errors: BTreeMap<&'static str, String>,
    generation: u64,
    layout_known: bool,
    layout_at: Instant,
    dial_owner: output::OutputOwner,
}

pub struct WorkerSetup<P: Platform> {
    pub platform: Arc<P>,
    pub keyboard: Keyboard,
    pub live: DeviceLive,
    pub rx: Receiver<Command>,
    pub mailbox: Mailbox,
    pub status: Arc<Mutex<DeviceStatus>>,
    pub hub: Arc<KeyHub>,
}

pub fn idle_status(id: &str) -> DeviceStatus {
    DeviceStatus {
        id: id.to_string(),
        owned: false,
        mode: bragi::MODE_HARDWARE,
        grabbed: false,
        features: Vec::new(),
        error: None,
        generation: None,
        frame_applied: None,
        remaps_active: None,
        config_revision: None,
    }
}

impl<P: Platform> Worker<P> {
    pub fn new(setup: WorkerSetup<P>) -> Self {
        let now = Instant::now();
        Self {
            layout: layouts::layout(setup.keyboard.model, PhysicalLayout::Ansi),
            platform: setup.platform,
            keyboard: setup.keyboard,
            live: setup.live,
            rx: setup.rx,
            mailbox: setup.mailbox,
            status: setup.status,
            hub: setup.hub,
            owner: None,
            events: None,
            tracker: KeyTracker::default(),
            sink: None,
            runner: None,
            wheel: None,
            remap: None,
            remap_dirty: false,
            remap_codes: BTreeSet::new(),
            remap_transition: false,
            remap_epoch: 0,
            frame_dirty: true,
            acquire_at: now,
            sink_at: now,
            wheel_at: now,
            remap_at: now,
            errors: BTreeMap::new(),
            generation: 1,
            layout_known: false,
            layout_at: now,
            dial_owner: output::new_owner(),
        }
    }

    fn id(&self) -> &str {
        &self.keyboard.id
    }

    pub fn run(mut self) {
        loop {
            let now = Instant::now();
            self.reconcile(now);
            self.publish_status();
            let timeout = util::timeout_until(self.next_deadline(), Instant::now());
            let mut fds = [
                util::pollfd(self.mailbox.waker.fd(), libc::POLLIN),
                util::pollfd(
                    self.events.as_ref().map_or(-1, |file| file.as_raw_fd()),
                    libc::POLLIN,
                ),
            ];
            if let Err(error) = util::poll(&mut fds, timeout) {
                eprintln!("{}: poll failed: {error}", self.id());
                std::thread::sleep(Duration::from_millis(100));
            }
            if fds[1].revents & libc::POLLIN != 0 {
                self.read_events();
            } else if fds[1].revents & POLL_GONE != 0 {
                self.close_events();
            }
            if fds[0].revents != 0 {
                self.mailbox.waker.drain();
            }
            loop {
                match self.rx.try_recv() {
                    Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                        self.shutdown();
                        return;
                    }
                    Ok(command) => self.command(command),
                    Err(TryRecvError::Empty) => break,
                }
            }
            if let Some(owner) = &mut self.owner
                && owner.tick(Instant::now())
            {
                // Woke from sleep: key state on the keyboard is fresh.
                self.tracker.clear();
                self.hub.close();
                self.hub.set_if2(self.events.is_some());
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Rpc {
                operation,
                deadline,
                reply,
            } => {
                let result = match &mut self.owner {
                    Some(owner) => owner.rpc(&operation, deadline, Instant::now()),
                    None => Err(anyhow::anyhow!("The companion does not own this keyboard")),
                };
                let _ = reply.try_send(result);
            }
            Command::Reload(live) => {
                if live != self.live {
                    eprintln!("{}: live settings reloaded", self.id());
                    self.runner = None;
                    self.generation += 1;
                    self.live = live;
                    self.remap_dirty = true;
                    self.frame_dirty = true;
                    // A config change is a reason to try failed inputs again right away.
                    let now = Instant::now();
                    self.sink_at = now;
                    self.wheel_at = now;
                    self.remap_at = now;
                    self.acquire_at = now;
                    self.layout_at = now;
                }
            }
            Command::Side { .. } if self.mailbox.shutdown.load(Ordering::SeqCst) => {}
            Command::Side { .. }
                if self.remap.as_ref().is_some_and(|remap| !remap.is_running()) => {}
            Command::Side { generation, .. } if generation != self.generation => {}
            Command::Side {
                side: Side::Brightness(delta),
                ..
            } => self.brightness(delta),
            Command::Side {
                side: Side::Run(argv),
                ..
            } => self.run_command(&argv),
            Command::Side {
                side: Side::Queue(steps),
                ..
            } => {
                if let Some(runner) = &self.runner {
                    actions::submit(&runner.queue(), steps);
                }
            }
            Command::RemapApplied(generation) => {
                if generation == self.generation && !self.remap_transition {
                    self.remap_dirty = false;
                }
            }
            Command::RemapStopped(epoch) if epoch != self.remap_epoch => {}
            Command::RemapStopped(_) => {
                self.generation += 1;
                self.runner = None;
                if !self.hub.if2() {
                    self.hub.close();
                }
                if self.remap.take().is_some() {
                    self.errors.insert(
                        "remap",
                        "Key remapping stopped (keyboard input went away)".into(),
                    );
                    self.remap_at = Instant::now() + ACQUIRE_RETRY;
                }
            }
            Command::Shutdown => {}
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let wants = needs(&self.keyboard, &self.live);
        let mut deadline: Option<Instant> = self
            .owner
            .as_ref()
            .filter(|owner| !owner.restoration_pending())
            .map(Owner::next_deadline);
        let mut consider = |at: Instant| {
            deadline = Some(deadline.map_or(at, |current| current.min(at)));
        };
        if wants.software_mode && self.owner.is_none() {
            consider(self.acquire_at);
        }
        if (wants.grab || (wants.software_mode && wants.dial_capable)) && self.sink.is_none() {
            consider(self.sink_at);
        }
        if wants.grab
            && self.remap.is_none()
            && self.sink.is_some()
            && self.runner.is_some()
            && self.layout_known
        {
            consider(self.remap_at);
        }
        if self.remap_transition {
            consider(self.remap_at);
        }
        if wants.grab && !self.layout_known {
            consider(self.layout_at);
        }
        if self.errors.contains_key("restore")
            || self.owner.as_ref().is_some_and(Owner::restoration_pending)
            || (wants.software_mode && wants.dial_capable && self.events.is_none())
        {
            consider(self.acquire_at);
        }
        if self.wants_wheel() && self.wheel.is_none() {
            consider(self.wheel_at);
        }
        deadline
    }

    fn wants_wheel(&self) -> bool {
        self.owner.is_some()
            && needs(&self.keyboard, &self.live).dial_capable
            && matches!(self.live.dial.mode, DialMode::Scroll | DialMode::Zoom)
    }

    fn set_error(&mut self, key: &'static str, error: Option<String>) {
        match error {
            Some(text) => {
                if self.errors.get(key) != Some(&text) {
                    eprintln!("{}: {text}", self.keyboard.id);
                }
                self.errors.insert(key, text);
            }
            None => {
                self.errors.remove(key);
            }
        }
    }

    fn reconcile(&mut self, now: Instant) {
        let wants = needs(&self.keyboard, &self.live);
        let wants_sink =
            wants.grab || self.remap.is_some() || (wants.software_mode && wants.dial_capable);

        // Failed output must ungrab before destroying the shared device, even with Arc clones.
        if self
            .sink
            .as_ref()
            .is_some_and(|sink| sink.lock().unwrap_or_else(|p| p.into_inner()).failed())
        {
            self.remap = None;
            self.runner = None;
            if let Some(sink) = self.sink.take() {
                sink.lock().unwrap_or_else(|p| p.into_inner()).destroy();
            }
            self.close_events();
            self.release_owner();
            self.sink_at = now + INPUT_RETRY;
            self.set_error(
                "input",
                Some("Virtual input failed; keyboard ungrabbed; retry pending".into()),
            );
        }
        if self.owner.as_ref().is_some_and(Owner::terminal_disconnect) {
            self.close_events();
            self.remap = None;
            self.runner = None;
            // The descriptor is terminal. Drop performs its independent cleanup attempt;
            // reacquisition verifies current hardware mode instead of claiming it restored.
            self.owner = None;
            self.errors.insert(
                "restore",
                "Terminal disconnect; mode unknown until reacquired".into(),
            );
            self.acquire_at = now + ACQUIRE_RETRY;
        }
        // Recovery remains scheduled even after configuration disables every live feature.
        if self.owner.is_none()
            && self.errors.contains_key("restore")
            && !wants.software_mode
            && now >= self.acquire_at
        {
            let restored = self
                .platform
                .open_transport(&self.keyboard)
                .and_then(|transport| {
                    crate::rescue::rescue_with(&mut Bragi::new(transport, self.keyboard.model))
                });
            match restored {
                Ok(_) => self.set_error("restore", None),
                Err(error) => {
                    self.set_error("restore", Some(format!("Rescue required: {error:#}")))
                }
            }
            self.acquire_at = now + ACQUIRE_RETRY;
        }
        if self.runner.as_ref().is_some_and(MacroRunner::is_cancelled) {
            self.runner = None;
            self.remap_dirty = true;
        }
        // Remap-only operation reads layout in hardware mode, without an ownership dip.
        if wants.grab && !self.layout_known && now >= self.layout_at {
            let physical = self
                .platform
                .open_transport(&self.keyboard)
                .and_then(|transport| {
                    let mut bragi = Bragi::new(transport, self.keyboard.model);
                    let code = bragi.get_u32(PROP_LAYOUT)?;
                    PhysicalLayout::from_code(code).context("Unknown physical layout")
                });
            if let Ok(physical) = physical {
                self.layout = layouts::layout(self.keyboard.model, physical);
                self.layout_known = true;
                self.remap_dirty = true;
            }
            self.layout_at = now + ACQUIRE_RETRY;
        }
        // 1. Virtual keyboard (dial emulation and remap output) + macro runner.
        if !wants_sink && self.sink.is_some() {
            self.remap = None;
            self.runner = None;
            self.sink = None;
        }
        if wants_sink && self.sink.is_none() && now >= self.sink_at {
            match self
                .platform
                .create_keyboard(self.keyboard.model, &output::keyboard_keys([]))
            {
                Ok(sink) => {
                    let sink = output::share(sink);
                    sink.lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .set_waker(self.mailbox.waker.clone());
                    match MacroRunner::start(sink.clone()) {
                        Ok(runner) => self.runner = Some(runner),
                        Err(error) => eprintln!("{}: {error:#}", self.keyboard.id),
                    }
                    self.sink = Some(sink);
                    self.set_error("input", None);
                }
                Err(error) => {
                    self.set_error("input", Some(format!("{error:#}")));
                    self.sink_at = now + INPUT_RETRY;
                }
            }
        }

        if wants_sink
            && self.runner.is_none()
            && let Some(sink) = &self.sink
        {
            match MacroRunner::start(sink.clone()) {
                Ok(runner) => self.runner = Some(runner),
                Err(error) => self.set_error(
                    "input",
                    Some(format!("Macro runner unavailable: {error:#}")),
                ),
            }
        }

        // Dial ownership requires a working event reader before changing mode.
        if wants.software_mode
            && wants.dial_capable
            && self.events.is_none()
            && now >= self.acquire_at
        {
            match self.platform.open_events(&self.keyboard) {
                Ok(Some(events)) => {
                    self.events = Some(events);
                    self.set_error("events", None);
                }
                Ok(None) => {
                    self.set_error(
                        "events",
                        Some("Dial event interface missing; staying in hardware mode".into()),
                    );
                    self.acquire_at = now + ACQUIRE_RETRY;
                }
                Err(error) => {
                    self.set_error(
                        "events",
                        Some(format!("Dial event reader unavailable: {error:#}")),
                    );
                    self.acquire_at = now + ACQUIRE_RETRY;
                }
            }
        }
        // 2. Software mode. Dial-only needs dial emulation; without it the dial would go dead.
        let dial_ok = !wants.dial_capable || (self.sink.is_some() && self.events.is_some());
        let soft = wants.software_mode && dial_ok;
        if wants.software_mode && wants.dial_capable && !dial_ok {
            if now >= self.acquire_at {
                self.acquire_at = now + ACQUIRE_RETRY;
            }
            let why = if wants.lighting {
                "Per-key lighting stays off until the dial event reader and input output are available"
            } else {
                "Dial modes need the opt-in input rule (uinput access); staying in hardware mode"
            };
            self.set_error("dial", Some(why.into()));
        } else {
            self.set_error("dial", None);
        }
        if self.owner.as_ref().is_some_and(Owner::restoration_pending) {
            if now >= self.acquire_at {
                self.release_owner();
                self.acquire_at = now + ACQUIRE_RETRY;
            }
        } else if !soft && self.owner.is_some() {
            self.release_owner();
            self.acquire_at = now + ACQUIRE_RETRY;
        }
        if soft && self.owner.is_none() && now >= self.acquire_at {
            self.acquire(now);
        }
        if let Some(owner) = &mut self.owner
            && self.frame_dirty
        {
            self.frame_dirty = false;
            let frame = wants.lighting.then(|| {
                let caps = self.keyboard.model.capabilities(self.keyboard.link);
                frame::compose(
                    self.keyboard.model,
                    self.layout,
                    &self.live.lighting,
                    caps.led_count,
                )
            });
            owner.set_frame(frame, now);
        }

        // 3. Virtual wheel for Scroll/Zoom.
        let wants_wheel = self.wants_wheel();
        if !wants_wheel {
            self.wheel = None;
            self.set_error("wheel", None);
        } else if self.wheel.is_none() && now >= self.wheel_at {
            match self.platform.create_wheel(self.keyboard.model) {
                Ok(wheel) => {
                    self.wheel = Some(wheel);
                    self.set_error("wheel", None);
                }
                Err(error) => {
                    self.set_error("wheel", Some(format!("{error:#}")));
                    self.wheel_at = now + INPUT_RETRY;
                }
            }
        }

        // 4. Remaps. Never abandon the captured disposition of a held physical key.
        self.remap_transition = false;
        let config = RemapConfig::from_live(&self.live, self.layout);
        if !wants.grab {
            if self.remap.as_ref().is_some_and(RemapHandle::keys_held) {
                if let (Some(remap), Some(runner)) = (&self.remap, &self.runner) {
                    remap.update(config, runner.queue(), self.generation);
                }
                self.remap_transition = true;
                self.remap_at = now + Duration::from_millis(20);
            } else if self.remap.take().is_some() {
                self.runner = None;
                if !wants.software_mode {
                    self.sink = None;
                }
                eprintln!("{}: key remapping stopped", self.keyboard.id);
            }
            self.set_error("remap", None);
        } else if self.remap.is_some() && self.remap_dirty {
            if config.source_codes().is_subset(&self.remap_codes) {
                if let (Some(remap), Some(runner)) = (&self.remap, &self.runner) {
                    remap.update(config, runner.queue(), self.generation);
                }
            } else if self.remap.as_ref().is_some_and(RemapHandle::keys_held) {
                self.remap_transition = true;
                self.remap_at = now + Duration::from_millis(20);
            } else {
                // Additional source capabilities: stop old producers only at an idle boundary.
                self.remap = None;
                self.runner = None;
                if let Some(sink) = &self.sink {
                    match MacroRunner::start(sink.clone()) {
                        Ok(runner) => self.runner = Some(runner),
                        Err(error) => self.set_error("input", Some(format!("{error:#}"))),
                    }
                }
                self.remap_at = now;
            }
        }

        if wants.grab
            && self.remap.is_none()
            && self.sink.is_some()
            && self.runner.is_some()
            && self.layout_known
            && now >= self.remap_at
        {
            match self.start_remap() {
                Ok(handle) => {
                    eprintln!("{}: key remapping active", self.keyboard.id);
                    self.remap = Some(handle);
                    self.set_error("remap", None);
                }
                Err(error) => {
                    self.set_error("remap", Some(format!("Key remapping is off: {error:#}")));
                    self.remap_at = now + INPUT_RETRY;
                }
            }
        }
        self.remap_dirty = self.remap_transition;
    }

    fn acquire(&mut self, now: Instant) {
        let result = self
            .platform
            .open_transport(&self.keyboard)
            .and_then(|transport| {
                Owner::acquire(
                    transport,
                    self.keyboard.model,
                    self.keyboard.link,
                    &self.keyboard.id,
                    self.platform.budget(),
                    now,
                )
            });
        let mut owner = match result {
            Ok(owner) => owner,
            Err(error) => {
                self.set_error("live", Some(format!("Live mode not active: {error:#}")));
                self.acquire_at = now + ACQUIRE_RETRY;
                return;
            }
        };
        self.set_error("live", None);
        if owner.restoration_pending() {
            self.set_error(
                "restore",
                Some("Rescue required: live-mode acquisition restoration is uncertain".into()),
            );
            self.acquire_at = now + ACQUIRE_RETRY;
        } else {
            self.set_error("restore", None);
        }
        eprintln!(
            "{}: live mode on{}",
            self.keyboard.id,
            if owner.is_online() {
                ""
            } else {
                " (keyboard asleep; waiting)"
            }
        );
        if owner.is_online()
            && let Ok(reply) = owner.xfer(&[bragi::CMD_GET, PROP_LAYOUT], now)
            && reply.get(2) == Some(&0)
        {
            let code = reply.get(3).copied().unwrap_or(0);
            if let Some(physical) = PhysicalLayout::from_code(code.into()) {
                self.layout = layouts::layout(self.keyboard.model, physical);
                self.remap_dirty = true;
                self.layout_known = true;
            }
        }
        if self.events.is_none() {
            match self.platform.open_events(&self.keyboard) {
                Ok(events) => {
                    self.events = events;
                    self.set_error("events", None);
                }
                Err(error) => self.set_error("events", Some(format!("{error:#}"))),
            }
        }
        self.tracker.clear();
        self.hub
            .set_if2(self.events.is_some() && owner.is_online() && !owner.restoration_pending());
        self.frame_dirty = true;
        self.owner = Some(owner);
    }

    fn close_events(&mut self) {
        self.events = None;
        self.tracker.clear();
        self.hub.set_if2(false);
        if self.remap.is_none() {
            self.hub.close();
        }
        if let Some(runner) = &self.runner {
            runner.cancel();
        }
        if let Some(sink) = &self.sink {
            let _ = output::release_owned(sink, self.dial_owner);
        }
        self.acquire_at = Instant::now() + ACQUIRE_RETRY;
    }

    fn release_owner(&mut self) {
        self.close_events();
        self.wheel = None;
        let released = self.owner.as_mut().map(Owner::release);
        match released {
            Some(Ok(())) => {
                self.owner = None;
                self.set_error("restore", None);
            }
            Some(Err(error)) => {
                self.set_error("restore", Some(format!("Rescue required: {error:#}")))
            }
            None => {}
        }
        self.frame_dirty = true;
    }

    fn start_remap(&mut self) -> Result<RemapHandle> {
        let config = RemapConfig::from_live(&self.live, self.layout);
        let codes = config.source_codes();
        if codes.is_empty() {
            bail!("the remapped keys are not on this keyboard's layout");
        }
        let sink = self.sink.clone().context("no virtual keyboard")?;
        let mut nodes = self.platform.open_nodes(&self.keyboard, &codes)?;
        // The virtual keyboard must be able to emit every key the grabbed nodes can produce.
        let mut needed: BTreeSet<u16> = nodes.iter().flat_map(|node| node.keys()).collect();
        let have: BTreeSet<u16> = sink
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .iter()
            .copied()
            .collect();
        if !needed.is_subset(&have) {
            self.runner = None;
            needed.extend(have);
            let keys: Vec<u16> = needed.into_iter().collect();
            let new_device = self.platform.create_keyboard(self.keyboard.model, &keys)?;
            {
                let mut guard = sink.lock().unwrap_or_else(|p| p.into_inner());
                guard.destroy();
                guard.replace(new_device);
            }
            self.runner = Some(MacroRunner::start(sink.clone())?);
        }
        remap::grab_all(&mut nodes, &self.mailbox.shutdown)?;
        self.remap_codes = codes;
        let generation = self.generation;
        self.remap_epoch += 1;
        let epoch = self.remap_epoch;
        let mailbox = self.mailbox.clone();
        RemapHandle::start(RemapSetup {
            nodes,
            sink,
            config,
            generation,
            macros: self
                .runner
                .as_ref()
                .context("macro runner not running")?
                .queue(),
            notify: Box::new(move |notice| {
                mailbox.send(match notice {
                    Notice::Side { side, generation } => Command::Side { side, generation },
                    Notice::Stopped => Command::RemapStopped(epoch),
                    Notice::Applied(generation) => Command::RemapApplied(generation),
                });
            }),
            hub: self.hub.clone(),
            layout: self.layout,
        })
    }

    fn read_events(&mut self) {
        let mut buffer = [0u8; REPORT_LEN];
        loop {
            let Some(file) = self.events.as_mut() else {
                return;
            };
            let read = match file.read(&mut buffer) {
                Ok(0) => {
                    self.close_events();
                    return;
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    eprintln!("{}: event node read failed: {error}", self.keyboard.id);
                    self.close_events();
                    return;
                }
            };
            self.handle_report(&buffer[..read]);
        }
    }

    pub(crate) fn handle_report(&mut self, report: &[u8]) {
        let dial_capable = needs(&self.keyboard, &self.live).dial_capable;
        match events::parse(report) {
            Report::Keys(keys) => {
                let change = self.tracker.update(keys);
                if dial_capable && change.pressed.contains(&DIAL_PRESS_INDEX) {
                    self.dial(DialEvent::Press);
                }
                self.hub
                    .publish(events::ids_for_indices(self.layout, self.tracker.held()));
            }
            Report::Dial(delta) if dial_capable => self.dial(DialEvent::Rotate(delta)),
            Report::Dial(_) | Report::Other => {}
        }
    }

    fn dial(&mut self, event: DialEvent) {
        let ops = match actions::dial_ops(&self.live.dial, &self.live.macros, event) {
            Ok(ops) => ops,
            Err(error) => {
                eprintln!("{}: dial action failed: {error:#}", self.keyboard.id);
                return;
            }
        };
        for op in ops {
            self.execute(op);
        }
    }

    fn emit_key(&self, events: &[output::Ev]) {
        if let Some(sink) = &self.sink
            && let Err(error) = output::emit_owned(sink, self.dial_owner, events)
        {
            eprintln!(
                "{}: virtual keyboard write failed: {error}",
                self.keyboard.id
            );
        }
    }

    fn execute(&mut self, op: Op) {
        match op {
            Op::Tap(codes) => {
                for batch in actions::tap_batches(&codes) {
                    self.emit_key(&batch);
                }
            }
            Op::Wheel { ctrl, detents } => {
                if self.wheel.is_none() {
                    return;
                }
                if ctrl {
                    self.emit_key(&[output::Ev::key(KEY_LEFTCTRL, 1)]);
                }
                if let Some(wheel) = &mut self.wheel
                    && let Err(error) = wheel.emit(&actions::wheel_batch(detents))
                {
                    eprintln!("{}: virtual wheel write failed: {error}", self.keyboard.id);
                }
                if ctrl {
                    self.emit_key(&[output::Ev::key(KEY_LEFTCTRL, 0)]);
                }
            }
            Op::Brightness(delta) => self.brightness(delta),
            Op::Run(argv) => self.run_command(&argv),
            Op::Queue(steps) => {
                if let Some(runner) = &self.runner {
                    actions::submit(&runner.queue(), steps);
                }
            }
        }
    }

    fn run_command(&self, argv: &[String]) {
        if let Err(error) = self.platform.spawn(argv) {
            eprintln!("{}: command failed: {error:#}", self.keyboard.id);
        }
    }

    fn brightness(&mut self, delta: i32) {
        let now = Instant::now();
        let result = match &mut self.owner {
            Some(owner) => owner.brightness_step(delta, now),
            // Not owned (remap-only): a short software-mode dip, like the plugin does.
            None => self
                .platform
                .open_transport(&self.keyboard)
                .and_then(|transport| {
                    let mut bragi = Bragi::new(transport, self.keyboard.model);
                    let mut budget = self.platform.budget();
                    let id = self.keyboard.id.clone();
                    bragi
                        .with_software_mode(|bragi| {
                            owner::adjust_brightness(bragi, &id, delta, &mut budget)
                        })
                        .map(|_| ())
                }),
        };
        if let Err(error) = result {
            eprintln!("{}: brightness step failed: {error:#}", self.keyboard.id);
        }
    }

    fn publish_status(&self) {
        let wants = needs(&self.keyboard, &self.live);
        let owned = self.owner.is_some();
        let mut features = Vec::new();
        if owned
            && wants.dial_capable
            && self.sink.is_some()
            && self.events.is_some()
            && self
                .owner
                .as_ref()
                .is_some_and(|owner| owner.mode_status() == bragi::MODE_SOFTWARE)
        {
            features.push("dial".to_string());
        }
        if owned
            && wants.lighting
            && !self.frame_dirty
            && self.owner.as_ref().is_some_and(Owner::frame_applied)
        {
            features.push("lighting".to_string());
        }
        let grabbed = self.remap.as_ref().is_some_and(RemapHandle::is_running);
        if grabbed
            && !self.remap_transition
            && !self.live.remaps.is_empty()
            && self
                .remap
                .as_ref()
                .is_some_and(|remap| remap.applied_generation() == self.generation)
        {
            features.push("remap".to_string());
        }
        let mut errors: Vec<String> = self.errors.values().cloned().collect();
        if let Some(error) = self.owner.as_ref().and_then(|owner| owner.last_error()) {
            errors.push(error.to_string());
        }
        let status = DeviceStatus {
            id: self.keyboard.id.clone(),
            owned,
            mode: self.owner.as_ref().map_or_else(
                || {
                    if self.errors.contains_key("restore") {
                        0
                    } else {
                        bragi::MODE_HARDWARE
                    }
                },
                Owner::mode_status,
            ),
            grabbed,
            features,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
            generation: Some(self.generation),
            frame_applied: Some(
                !self.frame_dirty && self.owner.as_ref().is_some_and(Owner::frame_applied),
            ),
            remaps_active: Some(
                !self.remap_dirty
                    && !self.remap_transition
                    && !self.live.remaps.is_empty()
                    && grabbed
                    && self
                        .remap
                        .as_ref()
                        .is_some_and(|remap| remap.applied_generation() == self.generation),
            ),
            config_revision: Some(crate::config::device_revision(&self.live)),
        };
        *self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = status;
    }

    /// Ungrab first (keys must never go dead), then hardware mode, then the virtual devices.
    fn shutdown(&mut self) {
        self.remap = None;
        self.runner = None;
        self.release_owner();
        self.wheel = None;
        if let Some(sink) = self.sink.take() {
            sink.lock().unwrap_or_else(|p| p.into_inner()).destroy();
        }
        self.hub.close();
        self.publish_status();
    }
}

impl<P: Platform> Drop for Worker<P> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bragi::{MODE_HARDWARE, MODE_SOFTWARE, RES_DIRECT_LIGHTING};
    use crate::config::{Action, MediaKey};
    use crate::daemon::output::tests::Recorder;
    use crate::daemon::output::{Ev, KEY_MUTE, KEY_VOLUMEUP};
    use crate::daemon::owner::tests::{SharedFake, counting_budget};
    use crate::daemon::remap::tests::{PipeNode, send};
    use crate::devices::LinkKind;
    use crate::effects::Rgb;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;

    type Batches = Arc<Mutex<Vec<Vec<Ev>>>>;

    /// Fake platform: shared fake keyboard, pipe-backed IF2 node, recording sinks and commands.
    pub(crate) struct FakePlatform {
        pub keyboard: SharedFake,
        pub events_tx: Mutex<Option<File>>,
        pub sinks: Mutex<Vec<Batches>>,
        pub wheels: Mutex<Vec<Batches>>,
        pub commands: Mutex<Vec<Vec<String>>>,
        pub nodes: Mutex<Vec<PipeNode>>,
        pub no_uinput: bool,
        pub no_events: bool,
        pub disconnected: Arc<AtomicBool>,
        pub busy: Mutex<bool>,
    }

    impl FakePlatform {
        pub fn new() -> Self {
            let keyboard = SharedFake::new();
            keyboard.with(|k| {
                k.props.insert(PROP_LAYOUT, 1);
            });
            Self {
                keyboard,
                events_tx: Mutex::new(None),
                sinks: Mutex::default(),
                wheels: Mutex::default(),
                commands: Mutex::default(),
                nodes: Mutex::default(),
                no_uinput: false,
                no_events: false,
                disconnected: Arc::new(AtomicBool::new(false)),
                busy: Mutex::new(false),
            }
        }
        pub fn last_sink(&self) -> Batches {
            self.sinks.lock().unwrap().last().unwrap().clone()
        }
        /// Write one IF2 report into the fake event node.
        pub fn report(&self, bytes: &[u8]) {
            let mut report = bytes.to_vec();
            report.resize(REPORT_LEN, 0);
            self.events_tx
                .lock()
                .unwrap()
                .as_mut()
                .expect("event node open")
                .write_all(&report)
                .unwrap();
        }
    }

    pub(crate) struct Disconnectable {
        fake: SharedFake,
        disconnected: Arc<AtomicBool>,
    }
    impl Transport for Disconnectable {
        fn xfer(&mut self, payload: &[u8]) -> Result<bragi::Reply> {
            if self.disconnected.load(Ordering::SeqCst) {
                return Err(bragi::Disconnected.into());
            }
            self.fake.xfer(payload)
        }
        fn sleep(&mut self, duration: Duration) {
            self.fake.sleep(duration);
        }
    }

    impl Platform for FakePlatform {
        type Transport = Disconnectable;

        fn open_transport(&self, _: &Keyboard) -> Result<Disconnectable> {
            if *self.busy.lock().unwrap() {
                bail!("Keyboard busy");
            }
            Ok(Disconnectable {
                fake: self.keyboard.clone(),
                disconnected: self.disconnected.clone(),
            })
        }
        fn open_events(&self, _: &Keyboard) -> Result<Option<File>> {
            if self.no_events {
                return Ok(None);
            }
            let mut fds = [0; 2];
            // SAFETY: valid out-array for pipe2.
            assert_eq!(
                unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
                0
            );
            // SAFETY: fresh descriptors we own.
            let (read, write) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
            *self.events_tx.lock().unwrap() = Some(write);
            Ok(Some(read))
        }
        fn create_keyboard(&self, _: Model, _: &[u16]) -> Result<Box<dyn EventSink>> {
            if self.no_uinput {
                bail!("No access to /dev/uinput");
            }
            let (recorder, batches) = Recorder::new();
            self.sinks.lock().unwrap().push(batches);
            Ok(Box::new(recorder))
        }
        fn create_wheel(&self, _: Model) -> Result<Box<dyn EventSink>> {
            if self.no_uinput {
                bail!("No access to /dev/uinput");
            }
            let (recorder, batches) = Recorder::new();
            self.wheels.lock().unwrap().push(batches);
            Ok(Box::new(recorder))
        }
        fn open_nodes(&self, _: &Keyboard, _: &BTreeSet<u16>) -> Result<Vec<Box<dyn InputNode>>> {
            let nodes: Vec<Box<dyn InputNode>> = self
                .nodes
                .lock()
                .unwrap()
                .drain(..)
                .map(|node| Box::new(node) as Box<dyn InputNode>)
                .collect();
            if nodes.is_empty() {
                bail!("no input nodes");
            }
            Ok(nodes)
        }
        fn budget(&self) -> Budget {
            counting_budget().0
        }
        fn spawn(&self, argv: &[String]) -> Result<()> {
            self.commands.lock().unwrap().push(argv.to_vec());
            Ok(())
        }
    }

    pub(crate) fn keyboard(link: LinkKind) -> Keyboard {
        Keyboard {
            id: "k65plus-test".into(),
            model: Model::K65PlusWireless,
            link,
            name: "Corsair K65 PLUS Wireless",
            vid: 0x1b1c,
            pid: if link == LinkKind::Receiver {
                0x2b07
            } else {
                0x2b10
            },
            serial: "test".into(),
            usb_path: PathBuf::from("/nonexistent"),
            cmd_node: PathBuf::from("/nonexistent/hidraw"),
            event_node: Some(PathBuf::from("/nonexistent/hidraw-events")),
            evdev_nodes: Vec::new(),
            frame_len: 64,
            target: if link == LinkKind::Receiver {
                0x09
            } else {
                0x08
            },
        }
    }

    fn worker(
        platform: &Arc<FakePlatform>,
        link: LinkKind,
        live: DeviceLive,
    ) -> Worker<FakePlatform> {
        let (mailbox, rx) = Mailbox::new().unwrap();
        Worker::new(WorkerSetup {
            platform: platform.clone(),
            keyboard: keyboard(link),
            live,
            rx,
            mailbox,
            status: Arc::new(Mutex::new(idle_status("k65plus-test"))),
            hub: Arc::new(KeyHub::default()),
        })
    }

    fn live(mode: DialMode) -> DeviceLive {
        let mut live = DeviceLive::default();
        live.dial.mode = mode;
        live
    }

    #[test]
    fn shutdown_flag_precedes_queued_command_and_expired_rpc_does_not_write() {
        let (mailbox, rx) = Mailbox::new().unwrap();
        assert!(mailbox.send(Command::Shutdown));
        assert!(mailbox.shutdown.load(Ordering::SeqCst));
        assert!(matches!(rx.try_recv().unwrap(), Command::Shutdown));
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Usb, live(DialMode::Media));
        worker.reconcile(Instant::now());
        let before = platform.keyboard.with(|k| k.log.len());
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        worker.command(Command::Rpc {
            operation: crate::ipc::Operation::Xfer {
                payload: vec![bragi::CMD_SET, bragi::PROP_BRIGHTNESS, 0, 0, 0],
            },
            deadline: Instant::now(),
            reply,
        });
        assert!(rx.recv().unwrap().is_err());
        assert_eq!(platform.keyboard.with(|k| k.log.len()), before);
    }
    #[test]
    fn restore_failure_survives_disabled_config_and_status_never_claims_hardware() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Usb, live(DialMode::Media));
        worker.reconcile(Instant::now());
        platform.keyboard.with(|k| k.stuck = true);
        worker.command(Command::Reload(live(DialMode::Volume)));
        worker.reconcile(Instant::now());
        worker.publish_status();
        assert!(worker.owner.is_some());
        assert_eq!(worker.status.lock().unwrap().mode, 0);
        platform.keyboard.with(|k| k.stuck = false);
        worker.reconcile(Instant::now() + ACQUIRE_RETRY);
        worker.publish_status();
        assert!(worker.owner.is_none());
        assert_eq!(worker.status.lock().unwrap().mode, MODE_HARDWARE);
    }
    #[test]
    fn first_if2_subscriber_gets_already_held_key_and_reader_loss_restores() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Usb, live(DialMode::Media));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x02, 0x10]);
        assert_eq!(
            worker.hub.subscribe().try_recv().unwrap(),
            vec!["a".to_string()]
        );
        worker.close_events();
        worker.reconcile(Instant::now());
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
        worker.reconcile(Instant::now() + ACQUIRE_RETRY);
        assert!(worker.owner.is_some() && worker.events.is_some());
    }
    #[test]
    fn remap_only_iso_layout_is_read_without_mode_writes() {
        let platform = Arc::new(FakePlatform::new());
        platform.keyboard.with(|k| {
            k.props.insert(PROP_LAYOUT, 2);
        });
        let mut config = DeviceLive::default();
        config.remaps.insert("capslock".into(), Action::Disabled);
        let mut worker = worker(&platform, LinkKind::Usb, config);
        worker.reconcile(Instant::now());
        assert!(worker.layout_known);
        assert_eq!(
            worker.layout,
            layouts::layout(Model::K65PlusWireless, PhysicalLayout::Iso)
        );
        assert!(
            !platform
                .keyboard
                .with(|k| k.log.iter().any(|p| p[0] == bragi::CMD_SET))
        );
    }

    #[test]
    fn missing_if2_refuses_dial_ownership_even_with_lighting_enabled() {
        let mut fake = FakePlatform::new();
        fake.no_events = true;
        let platform = Arc::new(fake);
        let mut config = live(DialMode::Media);
        config.lighting.enabled = true;
        let mut worker = worker(&platform, LinkKind::Usb, config);
        worker.reconcile(Instant::now());
        worker.publish_status();
        assert!(worker.owner.is_none());
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
        assert!(
            !worker
                .status
                .lock()
                .unwrap()
                .features
                .iter()
                .any(|feature| feature == "dial")
        );
    }
    #[test]
    fn terminal_disconnect_releases_stale_owner_and_reacquires_verified_transport() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Usb, live(DialMode::Media));
        worker.reconcile(Instant::now());
        platform.disconnected.store(true, Ordering::SeqCst);
        worker
            .owner
            .as_mut()
            .unwrap()
            .tick(Instant::now() + Duration::from_secs(20));
        assert!(worker.owner.as_ref().unwrap().terminal_disconnect());
        worker.reconcile(Instant::now());
        worker.publish_status();
        assert!(worker.owner.is_none());
        assert_eq!(worker.status.lock().unwrap().mode, 0);
        platform.disconnected.store(false, Ordering::SeqCst);
        worker.reconcile(Instant::now() + ACQUIRE_RETRY);
        worker.publish_status();
        assert!(worker.owner.is_some());
        assert_eq!(worker.status.lock().unwrap().mode, MODE_SOFTWARE);
    }
    #[test]
    fn removing_macro_binding_cancels_running_and_queued_generations() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Usb, live(DialMode::Media));
        worker.reconcile(Instant::now());
        let queue = worker.runner.as_ref().unwrap().queue();
        let batches = platform.last_sink();
        actions::submit(
            &queue,
            vec![
                actions::Step::Emit(vec![Ev::key(29, 1)]),
                actions::Step::Sleep(Duration::from_secs(3)),
            ],
        );
        let until = Instant::now() + Duration::from_secs(1);
        while batches.lock().unwrap().is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        actions::submit(&queue, vec![actions::Step::Emit(vec![Ev::key(30, 1)])]);
        worker.command(Command::Reload(live(DialMode::Volume)));
        actions::submit(&queue, vec![actions::Step::Emit(vec![Ev::key(31, 1)])]);
        assert!(
            batches
                .lock()
                .unwrap()
                .iter()
                .any(|batch| batch.contains(&Ev::key(29, 0)))
        );
        assert!(
            !batches
                .lock()
                .unwrap()
                .iter()
                .any(|batch| batch.contains(&Ev::key(30, 1)) || batch.contains(&Ev::key(31, 1)))
        );
        let old_generation = worker.generation - 1;
        worker.command(Command::Side {
            side: Side::Run(vec!["ignored".into()]),
            generation: old_generation,
        });
        assert!(platform.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn removing_last_remap_keeps_held_press_until_its_real_release() {
        let platform = Arc::new(FakePlatform::new());
        let (node, mut pipe) = PipeNode::new();
        let grabbed = node.grabbed.clone();
        platform.nodes.lock().unwrap().push(node);
        let mut config = DeviceLive::default();
        config.remaps.insert(
            "capslock".into(),
            Action::Key {
                combo: "ctrl".into(),
            },
        );
        let mut worker = worker(&platform, LinkKind::Usb, config);
        worker.reconcile(Instant::now());
        let batches = platform.last_sink();
        send(
            &mut pipe,
            &[
                Ev::key(58, 1),
                Ev {
                    kind: 0,
                    code: 0,
                    value: 0,
                },
            ],
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while !worker.remap.as_ref().unwrap().keys_held() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        worker.command(Command::Reload(DeviceLive::default()));
        worker.reconcile(Instant::now());
        assert!(grabbed.load(Ordering::SeqCst));
        assert!(worker.remap_transition);
        assert!(
            !batches
                .lock()
                .unwrap()
                .iter()
                .any(|batch| batch.contains(&Ev::key(29, 0)))
        );
        send(
            &mut pipe,
            &[
                Ev::key(58, 0),
                Ev {
                    kind: 0,
                    code: 0,
                    value: 0,
                },
            ],
        );
        while worker.remap.as_ref().unwrap().keys_held() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        worker.reconcile(Instant::now());
        assert!(!grabbed.load(Ordering::SeqCst));
        assert!(worker.remap.is_none());
        assert!(
            batches
                .lock()
                .unwrap()
                .iter()
                .any(|batch| batch.contains(&Ev::key(29, 0)))
        );
    }
    #[test]
    fn added_remap_source_reopens_required_nodes_at_idle_boundary() {
        let platform = Arc::new(FakePlatform::new());
        let (first, _p1) = PipeNode::new();
        platform.nodes.lock().unwrap().push(first);
        let mut config = DeviceLive::default();
        config.remaps.insert("capslock".into(), Action::Disabled);
        let mut worker = worker(&platform, LinkKind::Usb, config.clone());
        worker.reconcile(Instant::now());
        assert!(worker.remap.is_some());
        let (second, _p2) = PipeNode::new();
        let grabbed = second.grabbed.clone();
        platform.nodes.lock().unwrap().push(second);
        config.remaps.insert("a".into(), Action::Disabled);
        worker.command(Command::Reload(config));
        worker.reconcile(Instant::now());
        assert!(platform.nodes.lock().unwrap().is_empty());
        assert!(grabbed.load(Ordering::SeqCst));
        assert!(worker.remap_codes.contains(&30));
    }

    #[test]
    fn adopted_new_revision_resets_old_frame_acknowledgement_before_application() {
        let platform = Arc::new(FakePlatform::new());
        let mut initial = live(DialMode::Media);
        initial.lighting.enabled = true;
        let mut worker = worker(&platform, LinkKind::Usb, initial.clone());
        worker.reconcile(Instant::now());
        worker.publish_status();
        assert_eq!(worker.status.lock().unwrap().frame_applied, Some(true));
        let old = worker.status.lock().unwrap().config_revision.clone();
        initial.lighting.base = Some(Rgb(1, 2, 3));
        worker.command(Command::Reload(initial.clone()));
        worker.publish_status();
        let status = worker.status.lock().unwrap().clone();
        assert_ne!(status.config_revision, old);
        assert_eq!(
            status.config_revision,
            Some(crate::config::device_revision(&initial))
        );
        assert_eq!(status.frame_applied, Some(false));
        worker.reconcile(Instant::now());
        worker.publish_status();
        assert_eq!(worker.status.lock().unwrap().frame_applied, Some(true));
    }
    #[test]
    fn feature_needs_follow_model_and_link() {
        let receiver = keyboard(LinkKind::Receiver);
        let mut config = live(DialMode::Volume);
        config.lighting.enabled = true;
        let wants = needs(&receiver, &config);
        assert!(!wants.software_mode, "no live lighting over the receiver");
        assert!(wants.dial_capable && !wants.lighting && !wants.grab);
        let wired = keyboard(LinkKind::Usb);
        assert!(needs(&wired, &config).software_mode && needs(&wired, &config).lighting);
        let mut k100 = keyboard(LinkKind::Usb);
        k100.model = Model::K100Rgb;
        let wants = needs(&k100, &live(DialMode::Scroll));
        assert!(!wants.dial_capable && !wants.software_mode);
    }

    #[test]
    fn dial_mode_owns_software_mode_and_emulates_the_dial() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Media));
        worker.reconcile(Instant::now());
        assert!(worker.owner.is_some());
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_SOFTWARE);
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0x01, 0, 0, 0]);
        let sink = platform.last_sink();
        assert_eq!(
            sink.lock().unwrap().clone(),
            vec![
                vec![Ev::key(output::KEY_NEXTSONG, 1)],
                vec![Ev::key(output::KEY_NEXTSONG, 0)]
            ]
        );
        // Dial press is bit 137 of the key bitmap (byte 17, bit 1); releasing does nothing.
        let mut press = vec![0x01, 0x02];
        press.extend_from_slice(&[0; 17]);
        press.push(0x02);
        worker.handle_report(&press);
        worker.handle_report(&[0x01, 0x02]);
        assert_eq!(sink.lock().unwrap().len(), 4);
        assert_eq!(
            sink.lock().unwrap()[2],
            vec![Ev::key(output::KEY_PLAYPAUSE, 1)]
        );
        worker.publish_status();
        let status = worker.status.lock().unwrap().clone();
        assert!(status.owned && status.mode == 2 && status.features == vec!["dial"]);
        // Back to Volume: onboard dial again, hardware mode restored.
        worker.command(Command::Reload(live(DialMode::Volume)));
        worker.reconcile(Instant::now());
        assert!(worker.owner.is_none() && worker.sink.is_none());
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
    }

    #[test]
    fn volume_and_workspace_and_scroll_modes() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Workspace));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(
            platform.commands.lock().unwrap().clone(),
            vec![
                vec!["hyprctl", "dispatch", "workspace", "e-1"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            ]
        );
        worker.command(Command::Reload(live(DialMode::Scroll)));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0x02, 0, 0, 0]);
        let wheel = platform.wheels.lock().unwrap()[0].clone();
        assert_eq!(
            wheel.lock().unwrap().clone(),
            vec![actions::wheel_batch(-2)]
        );
        worker.command(Command::Reload(live(DialMode::Zoom)));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0x01, 0, 0, 0]);
        let keys = platform.last_sink().lock().unwrap().clone();
        assert_eq!(
            keys,
            vec![
                vec![Ev::key(KEY_LEFTCTRL, 1)],
                vec![Ev::key(KEY_LEFTCTRL, 0)]
            ]
        );
        assert_eq!(
            wheel.lock().unwrap().last().unwrap(),
            &actions::wheel_batch(1)
        );
        // Custom mode with a media press action.
        let mut custom = live(DialMode::Custom);
        custom.dial.press = Some(Action::Media {
            key: MediaKey::Mute,
        });
        custom.dial.clockwise = Some(Action::Media {
            key: MediaKey::VolumeUp,
        });
        worker.command(Command::Reload(custom));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0x01, 0, 0, 0]);
        let keys = platform.last_sink().lock().unwrap().clone();
        assert_eq!(keys[keys.len() - 2], vec![Ev::key(KEY_VOLUMEUP, 1)]);
        let mut press = vec![0x01, 0x02];
        press.extend_from_slice(&[0; 17]);
        press.push(0x02);
        worker.handle_report(&press);
        let keys = platform.last_sink().lock().unwrap().clone();
        assert_eq!(keys[keys.len() - 2], vec![Ev::key(KEY_MUTE, 1)]);
    }

    #[test]
    fn brightness_dial_writes_through_the_owned_transport() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Brightness));
        worker.reconcile(Instant::now());
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0x01, 0, 0, 0]);
        assert_eq!(
            platform.keyboard.with(|k| k.props[&bragi::PROP_BRIGHTNESS]),
            800
        );
        worker.handle_report(&[0x01, 0x05, 0x60, 0x00, 0xfe, 0xff, 0xff, 0xff]);
        assert_eq!(
            platform.keyboard.with(|k| k.props[&bragi::PROP_BRIGHTNESS]),
            600
        );
    }

    #[test]
    fn dial_mode_without_uinput_stays_in_hardware_mode() {
        let mut platform = FakePlatform::new();
        platform.no_uinput = true;
        let platform = Arc::new(platform);
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Scroll));
        worker.reconcile(Instant::now());
        assert!(worker.owner.is_none());
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
        assert!(
            platform.keyboard.with(|k| k.log.is_empty()),
            "never touched the keyboard"
        );
        worker.publish_status();
        let status = worker.status.lock().unwrap().clone();
        assert!(!status.owned);
        assert!(status.error.unwrap().contains("input rule"));
    }

    #[test]
    fn live_lighting_writes_frames_on_change_only() {
        let platform = Arc::new(FakePlatform::new());
        let mut config = live(DialMode::Volume);
        config.lighting.enabled = true;
        config.lighting.base = Some(Rgb(1, 2, 3));
        let mut worker = worker(&platform, LinkKind::Usb, config.clone());
        worker.reconcile(Instant::now());
        let written = platform
            .keyboard
            .with(|k| k.written[&RES_DIRECT_LIGHTING].clone());
        assert_eq!(written.len(), 4 + 2 + 123 * 3);
        assert_eq!(
            &written[4..12],
            &[0x12, 0x00, 1, 2, 3, 0, 0, 0],
            "LED 0 follows space, LED 1 dark"
        );
        let writes = || {
            platform
                .keyboard
                .with(|k| k.cmds().iter().filter(|c| **c == bragi::CMD_WRITE).count())
        };
        assert_eq!(writes(), 1);
        worker.command(Command::Reload(config.clone()));
        worker.reconcile(Instant::now());
        assert_eq!(writes(), 1, "identical config: no write");
        config.lighting.keys.insert("a".into(), Rgb(9, 9, 9));
        worker.command(Command::Reload(config));
        worker.reconcile(Instant::now());
        assert_eq!(writes(), 2);
        worker.publish_status();
        let features = worker.status.lock().unwrap().features.clone();
        assert_eq!(features, vec!["dial", "lighting"]);
    }

    #[test]
    fn busy_keyboard_is_retried_later() {
        let platform = Arc::new(FakePlatform::new());
        *platform.busy.lock().unwrap() = true;
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Media));
        let now = Instant::now();
        worker.reconcile(now);
        assert!(worker.owner.is_none());
        assert_eq!(worker.next_deadline(), Some(now + ACQUIRE_RETRY));
        *platform.busy.lock().unwrap() = false;
        worker.reconcile(now + ACQUIRE_RETRY);
        assert!(worker.owner.is_some());
    }

    #[test]
    fn xfer_only_when_owned() {
        let platform = Arc::new(FakePlatform::new());
        let mut worker = worker(&platform, LinkKind::Receiver, live(DialMode::Volume));
        worker.reconcile(Instant::now());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        worker.command(Command::Rpc {
            deadline: Instant::now() + Duration::from_secs(2),
            operation: crate::ipc::Operation::Xfer {
                payload: vec![bragi::CMD_GET, bragi::PROP_BATTERY],
            },
            reply: tx,
        });
        assert!(rx.recv().unwrap().is_err());
        worker.command(Command::Reload(live(DialMode::Media)));
        worker.reconcile(Instant::now());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        worker.command(Command::Rpc {
            deadline: Instant::now() + Duration::from_secs(2),
            operation: crate::ipc::Operation::Xfer {
                payload: vec![bragi::CMD_GET, bragi::PROP_BRIGHTNESS],
            },
            reply: tx,
        });
        let reply = rx.recv().unwrap().unwrap();
        assert_eq!(&reply[..5], &[1, bragi::CMD_GET, 0, 0xbc, 0x02]);
    }

    #[test]
    fn worker_thread_shutdown_restores_hardware_mode_and_ungrabs() {
        let platform = Arc::new(FakePlatform::new());
        let (node, mut pipe) = PipeNode::new();
        let grabbed = node.grabbed.clone();
        platform.nodes.lock().unwrap().push(node);
        let mut config = live(DialMode::Media);
        config.remaps.insert(
            "capslock".into(),
            Action::Key {
                combo: "esc".into(),
            },
        );
        let (mailbox, rx) = Mailbox::new().unwrap();
        let status = Arc::new(Mutex::new(idle_status("k65plus-test")));
        let worker = Worker::new(WorkerSetup {
            platform: platform.clone(),
            keyboard: keyboard(LinkKind::Receiver),
            live: config,
            rx,
            mailbox: mailbox.clone(),
            status: status.clone(),
            hub: Arc::new(KeyHub::default()),
        });
        let thread = std::thread::spawn(move || worker.run());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !(status.lock().unwrap().grabbed && status.lock().unwrap().owned)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(grabbed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_SOFTWARE);
        // Remapped key: Caps Lock -> Esc on the virtual keyboard.
        send(
            &mut pipe,
            &[
                Ev::key(58, 1),
                Ev {
                    kind: 0,
                    code: 0,
                    value: 0,
                },
            ],
        );
        let sink = platform.last_sink();
        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sink.lock().unwrap()[0], vec![Ev::key(1, 1)]);
        let started = Instant::now();
        assert!(mailbox.send(Command::Shutdown));
        thread.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(platform.keyboard.with(|k| k.mode), MODE_HARDWARE);
        assert_eq!(
            sink.lock().unwrap().last().unwrap(),
            &vec![Ev::key(1, 0)],
            "held remapped key released before ungrab"
        );
        let status = status.lock().unwrap().clone();
        assert!(!status.owned && !status.grabbed);
    }
}
