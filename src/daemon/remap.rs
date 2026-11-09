//! Remaps and macros on the OS input layer: the keyboard's evdev node(s) are grabbed and every
//! event is forwarded to the companion's virtual keyboard, with remapped keys transformed.
//! LED state written to the virtual keyboard (Caps/Num Lock) is forwarded back to the physical
//! keyboard. Never logs key contents.

use super::actions::{self, MacroQueue, Op, Step};
use super::hub::KeyHub;
use super::output::{self, EV_KEY, EV_LED, EV_SYN, Ev, SYN_DROPPED, SYN_REPORT, SharedSink};
use super::util::{self, POLL_GONE, Waker};
use crate::config::{Action, DeviceLive, Macro};
use crate::layouts::{self, Layout};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long to wait for keys held at grab time (e.g. Enter that started the service) to be released.
pub const RELEASE_WAIT: Duration = Duration::from_secs(3);

/// One physical evdev node.
pub trait InputNode: Send {
    fn fd(&self) -> RawFd;
    /// Read all pending events (non-blocking) into `out`.
    fn read(&mut self, out: &mut Vec<Ev>) -> io::Result<()>;
    fn keys(&self) -> Vec<u16>;
    fn has_leds(&self) -> bool;
    /// Authoritative EVIOCGKEY snapshot; unsupported/failed queries must fail open.
    fn key_state(&self) -> io::Result<BTreeSet<u16>> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn ungrab(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn grab(&mut self) -> io::Result<()>;
    fn send_led(&mut self, code: u16, value: i32) -> io::Result<()>;
}

/// Real evdev node; ungrabbed when dropped.
pub struct EvdevNode {
    device: evdev::raw_stream::RawDevice,
    grabbed: bool,
}

impl EvdevNode {
    /// Open a node read-write (needed for grab and LEDs), non-blocking.
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::PermissionDenied => anyhow::anyhow!(
                    "No access to {} — install the opt-in input rule (71-hyprdeck-corsair-input.rules)",
                    path.display()
                ),
                _ => anyhow::Error::new(error).context(format!("Opening {}", path.display())),
            })?;
        let device = evdev::raw_stream::RawDevice::from_fd(OwnedFd::from(file))
            .with_context(|| format!("Reading capabilities of {}", path.display()))?;
        Ok(Self {
            device,
            grabbed: false,
        })
    }
    /// Nodes that also report pointer or absolute axes are never grabbed (they could not be
    /// forwarded faithfully).
    pub fn forwardable(&self) -> bool {
        let events = self.device.supported_events();
        !events.contains(evdev::EventType::RELATIVE) && !events.contains(evdev::EventType::ABSOLUTE)
    }
}

impl InputNode for EvdevNode {
    fn fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }
    fn read(&mut self, out: &mut Vec<Ev>) -> io::Result<()> {
        match self.device.fetch_events() {
            Ok(events) => {
                out.extend(events.map(|event| Ev {
                    kind: event.event_type().0,
                    code: event.code(),
                    value: event.value(),
                }));
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn keys(&self) -> Vec<u16> {
        self.device
            .supported_keys()
            .map(|keys| keys.iter().map(|key| key.0).collect())
            .unwrap_or_default()
    }
    fn has_leds(&self) -> bool {
        self.device.supported_leds().is_some()
    }
    fn key_state(&self) -> io::Result<BTreeSet<u16>> {
        self.device
            .get_key_state()
            .map(|state| state.iter().map(|key| key.0).collect())
    }
    fn ungrab(&mut self) -> io::Result<()> {
        if self.grabbed {
            self.device.ungrab()?;
            self.grabbed = false;
        }
        Ok(())
    }
    fn grab(&mut self) -> io::Result<()> {
        self.device.grab()?;
        self.grabbed = true;
        Ok(())
    }
    fn send_led(&mut self, code: u16, value: i32) -> io::Result<()> {
        self.device
            .send_events(&[evdev::InputEvent::new(EV_LED, code, value)])
    }
}

impl Drop for EvdevNode {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.device.ungrab();
        }
    }
}

/// Open the keyboard's evdev nodes that carry at least one remapped key (not grabbed yet).
pub fn open_nodes(
    paths: &[std::path::PathBuf],
    codes: &BTreeSet<u16>,
) -> Result<Vec<Box<dyn InputNode>>> {
    let mut nodes: Vec<Box<dyn InputNode>> = Vec::new();
    let mut last_error = None;
    for path in paths {
        match EvdevNode::open(path) {
            Ok(node) => {
                let carries = node.keys().iter().any(|code| codes.contains(code));
                if carries && node.forwardable() {
                    nodes.push(Box::new(node));
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    if nodes.is_empty() {
        match last_error {
            Some(error) => return Err(error),
            None => bail!("None of the keyboard's input nodes carries the remapped keys"),
        }
    }
    // A partly accessible capability set is not an active mapping set.
    if let Some(error) = last_error {
        return Err(error.context("Not all input nodes are accessible"));
    }
    let covered: BTreeSet<u16> = nodes.iter().flat_map(|node| node.keys()).collect();
    if !codes.is_subset(&covered) {
        bail!("Some remapped keys have no accessible forwardable input node");
    }
    Ok(nodes)
}

/// Wait (bounded) until no key is held on any node, then grab all; on any failure nothing stays grabbed.
pub fn grab_all(nodes: &mut [Box<dyn InputNode>], stop: &AtomicBool) -> Result<()> {
    grab_until(nodes, stop, Instant::now() + RELEASE_WAIT)
}
fn grab_until(
    nodes: &mut [Box<dyn InputNode>],
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<()> {
    let result = (|| {
        loop {
            if stop.load(Ordering::SeqCst) {
                bail!("Grab cancelled");
            }
            let held = nodes
                .iter()
                .map(|node| node.key_state())
                .collect::<io::Result<Vec<_>>>()?;
            if held.iter().all(BTreeSet::is_empty) {
                break;
            }
            if Instant::now() >= deadline {
                bail!("Keys are still held; refusing grab and retrying later");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut discard = Vec::new();
        for node in nodes.iter_mut() {
            if stop.load(Ordering::SeqCst) {
                bail!("Grab cancelled");
            }
            // Everything queued before EVIOCGRAB already reached the desktop.
            discard.clear();
            node.read(&mut discard)?;
            if !node.key_state()?.is_empty() {
                bail!("A key raced with grab acquisition");
            }
            node.grab().context("Grabbing the keyboard (EVIOCGRAB)")?;
            // A press straddling the grab boundary cannot safely be transferred to another device.
            // Refuse the acquisition rather than duplicate a pre-grab press or steal its release.
            discard.clear();
            node.read(&mut discard)?;
            if !node.key_state()?.is_empty() || discard.iter().any(|event| event.kind == EV_KEY) {
                bail!("Input raced with grab acquisition; retrying at an idle boundary");
            }
        }
        if stop.load(Ordering::SeqCst) {
            bail!("Grab cancelled");
        }
        Ok(())
    })();
    if result.is_err() {
        for node in nodes.iter_mut() {
            let _ = node.ungrab();
        }
    }
    result
}

/// Remap table in evdev codes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemapConfig {
    pub map: HashMap<u16, Action>,
    pub macros: BTreeMap<String, Macro>,
}

impl RemapConfig {
    pub fn from_live(live: &DeviceLive, layout: &Layout) -> Self {
        let map = live
            .remaps
            .iter()
            .filter_map(|(id, action)| {
                let code = layouts::key_by_id(layout, id)?.evdev?;
                Some((code, action.clone()))
            })
            .collect();
        Self {
            map,
            macros: live.macros.clone(),
        }
    }
    pub fn source_codes(&self) -> BTreeSet<u16> {
        self.map.keys().copied().collect()
    }
}

/// Side effects the forwarder cannot perform inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    Queue(Vec<Step>),
    Run(Vec<String>),
    Brightness(i32),
}

/// Event transformation state (pure; the thread feeds it).
#[derive(Default)]
pub struct Forwarder {
    pub config: RemapConfig,
    /// Source code -> output codes held for it.
    active: HashMap<u16, Vec<u16>>,
    batch: Vec<Ev>,
    /// Physical keys currently held (evdev codes), for the key tester.
    pub pressed: BTreeSet<u16>,
    /// Output keys currently down on the virtual keyboard.
    down: BTreeSet<u16>,
    refs: HashMap<u16, usize>,
    dropped: bool,
    resync: bool,
}

impl Forwarder {
    pub fn new(config: RemapConfig) -> Self {
        Self {
            config,
            batch: Vec::with_capacity(64),
            ..Self::default()
        }
    }

    fn send(
        &mut self,
        events: &[Ev],
        emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>,
    ) -> io::Result<()> {
        // Failed downs are potentially down; failed ups must remain in cleanup state.
        for event in events
            .iter()
            .filter(|event| event.kind == EV_KEY && event.value == 1)
        {
            self.down.insert(event.code);
        }
        emit(events)?;
        for event in events
            .iter()
            .filter(|event| event.kind == EV_KEY && event.value == 0)
        {
            self.down.remove(&event.code);
        }
        Ok(())
    }
    fn press_codes(&mut self, codes: &[u16]) -> Vec<Ev> {
        codes
            .iter()
            .filter_map(|code| {
                let count = self.refs.entry(*code).or_default();
                *count += 1;
                (*count == 1).then(|| Ev::key(*code, 1))
            })
            .collect()
    }
    fn release_codes(&mut self, codes: &[u16]) -> Vec<Ev> {
        codes
            .iter()
            .rev()
            .filter_map(|code| {
                let count = self.refs.get_mut(code)?;
                *count = count.saturating_sub(1);
                (*count == 0).then(|| Ev::key(*code, 0))
            })
            .collect()
    }
    fn captured_codes(&self, code: u16) -> Vec<u16> {
        match self.config.map.get(&code) {
            None => vec![code],
            Some(Action::Key { combo }) => actions::combo_codes(combo).unwrap_or_default(),
            Some(Action::Media { key }) => vec![actions::media_code(*key)],
            _ => Vec::new(),
        }
    }
    /// Reconcile a post-SYN_DROPPED authoritative snapshot without replaying macros/commands.
    fn reconcile_state(
        &mut self,
        held: BTreeSet<u16>,
        emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>,
    ) -> io::Result<()> {
        // A dropped report can discard unflushed downs or ups. Rebuild desired ownership
        // from the authoritative held set, preserving each already-captured press disposition.
        let mut active = HashMap::new();
        let mut refs = HashMap::new();
        for code in &held {
            let codes = self
                .active
                .get(code)
                .cloned()
                .unwrap_or_else(|| self.captured_codes(*code));
            for output in &codes {
                *refs.entry(*output).or_insert(0usize) += 1;
            }
            active.insert(*code, codes);
        }
        let desired: BTreeSet<u16> = refs.keys().copied().collect();
        let events: Vec<Ev> = self
            .down
            .difference(&desired)
            .map(|code| Ev::key(*code, 0))
            .chain(desired.difference(&self.down).map(|code| Ev::key(*code, 1)))
            .collect();
        self.send(&events, emit)?;
        self.active = active;
        self.refs = refs;
        self.pressed = held;
        self.resync = false;
        Ok(())
    }

    fn flush(&mut self, emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let result = self.send(&batch, emit);
        self.batch = batch;
        self.batch.clear();
        result
    }

    /// Handle one physical event. Returns whether the physical pressed set changed.
    pub fn handle(
        &mut self,
        event: Ev,
        emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>,
        side: &mut Vec<Side>,
    ) -> io::Result<bool> {
        if self.dropped {
            if event.kind == EV_SYN && event.code == SYN_REPORT {
                self.dropped = false;
                self.resync = true;
            }
            return Ok(false);
        }
        if self.resync {
            return Ok(false);
        }
        match (event.kind, event.code) {
            (EV_SYN, SYN_REPORT) => {
                self.flush(emit)?;
                Ok(false)
            }
            (EV_SYN, SYN_DROPPED) => {
                self.batch.clear();
                self.dropped = true;
                Ok(false)
            }
            (EV_KEY, code) => {
                let changed = match event.value {
                    1 => self.pressed.insert(code),
                    0 => self.pressed.remove(&code),
                    _ => false,
                };
                match event.value {
                    0 => {
                        if let Some(codes) = self.active.remove(&code) {
                            let events = self.release_codes(&codes);
                            if self.config.map.contains_key(&code) {
                                self.flush(emit)?;
                                if !events.is_empty() {
                                    self.send(&events, emit)?;
                                }
                            } else {
                                self.batch.extend(events);
                            }
                        }
                    }
                    2 => {
                        if let Some(last) = self
                            .active
                            .get(&code)
                            .and_then(|codes| codes.last())
                            .copied()
                        {
                            if self.config.map.contains_key(&code) {
                                self.flush(emit)?;
                                self.send(&[Ev::key(last, 2)], emit)?;
                            } else {
                                self.batch.push(Ev::key(last, 2));
                            }
                        }
                    }
                    1 if changed => {
                        let codes = self.captured_codes(code);
                        // Capture even pass-through and suppressed action dispositions before writes.
                        self.active.insert(code, codes.clone());
                        let events = self.press_codes(&codes);
                        if let Some(action) = self.config.map.get(&code).cloned() {
                            self.flush(emit)?;
                            if !events.is_empty() {
                                self.send(&events, emit)?;
                            }
                            if !matches!(action, Action::Key { .. } | Action::Media { .. }) {
                                self.remapped(code, 1, &action, emit, side)?;
                            }
                        } else {
                            self.batch.extend(events);
                        }
                    }
                    _ => {}
                }
                Ok(changed)
            }
            // Scan codes are dropped (the virtual keyboard has its own); LED echoes and anything
            // else are not forwarded either.
            (output::EV_MSC, _) => Ok(false),
            _ => Ok(false),
        }
    }

    fn remapped(
        &mut self,
        code: u16,
        value: i32,
        action: &Action,
        emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>,
        side: &mut Vec<Side>,
    ) -> io::Result<()> {
        let _ = code;
        if value != 1 {
            return Ok(());
        }
        match actions::action_ops(action, &self.config.macros) {
            Ok(ops) => {
                for op in ops {
                    match op {
                        Op::Queue(steps) => side.push(Side::Queue(steps)),
                        Op::Run(argv) => side.push(Side::Run(argv)),
                        Op::Brightness(delta) => side.push(Side::Brightness(delta)),
                        Op::Tap(codes) => {
                            for batch in actions::tap_batches(&codes) {
                                let events = if batch.first().is_some_and(|event| event.value == 1)
                                {
                                    self.press_codes(&codes)
                                } else {
                                    self.release_codes(&codes)
                                };
                                if !events.is_empty() {
                                    self.send(&events, emit)?;
                                }
                            }
                        }
                        Op::Wheel { .. } => {}
                    }
                }
            }
            Err(error) => eprintln!("remap action failed: {error:#}"),
        }
        Ok(())
    }

    /// Release every key this forwarder holds down on the virtual keyboard.
    pub fn release_all(&mut self, emit: &mut dyn FnMut(&[Ev]) -> io::Result<()>) {
        self.batch.clear();
        self.active.clear();
        self.refs.clear();
        let codes: Vec<u16> = self.down.iter().copied().collect();
        if !codes.is_empty() && emit(&actions::release(&codes)).is_ok() {
            self.down.clear();
        }
    }
}

/// Callback for side effects the worker performs (brightness, commands) and for "stopped".
pub type Notify = Box<dyn Fn(Notice) + Send>;

#[derive(Debug)]
pub enum Notice {
    Side { side: Side, generation: u64 },
    Stopped,
    Applied(u64),
}

/// Running forwarder thread; dropping it ungrabs the keyboard.
pub struct RemapHandle {
    stop: Arc<AtomicBool>,
    waker: Arc<Waker>,
    thread: Option<JoinHandle<()>>,
    config: Arc<Mutex<(RemapConfig, MacroQueue, u64)>>,
    adopted: Arc<AtomicU64>,
    held: Arc<AtomicBool>,
}

pub struct RemapSetup {
    pub nodes: Vec<Box<dyn InputNode>>,
    pub sink: SharedSink,
    pub config: RemapConfig,
    pub generation: u64,
    pub macros: MacroQueue,
    pub notify: Notify,
    pub hub: Arc<KeyHub>,
    pub layout: &'static Layout,
}

impl RemapHandle {
    /// Start forwarding from already grabbed nodes.
    pub fn start(setup: RemapSetup) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let waker = Arc::new(Waker::new().context("Creating the remap waker")?);
        let config = Arc::new(Mutex::new((
            setup.config.clone(),
            setup.macros.clone(),
            setup.generation,
        )));
        let adopted = Arc::new(AtomicU64::new(0));
        let held = Arc::new(AtomicBool::new(false));
        let thread = {
            let adopted = adopted.clone();
            let held = held.clone();
            let (stop, waker, config) = (stop.clone(), waker.clone(), config.clone());
            std::thread::Builder::new()
                .name("remap".into())
                .spawn(move || forward(setup, &stop, &waker, &config, &adopted, &held))
                .context("Starting the remap thread")?
        };
        Ok(Self {
            stop,
            waker,
            thread: Some(thread),
            config,
            adopted,
            held,
        })
    }
    pub fn update(&self, config: RemapConfig, macros: MacroQueue, generation: u64) {
        *self.config.lock().unwrap_or_else(|p| p.into_inner()) = (config, macros, generation);
        self.waker.wake();
    }
    pub fn keys_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }
    pub fn applied_generation(&self) -> u64 {
        self.adopted.load(Ordering::Acquire)
    }
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for RemapHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.waker.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn forward(
    setup: RemapSetup,
    stop: &AtomicBool,
    waker: &Waker,
    shared: &Mutex<(RemapConfig, MacroQueue, u64)>,
    adopted: &AtomicU64,
    held: &AtomicBool,
) {
    let RemapSetup {
        mut nodes,
        sink,
        config,
        mut generation,
        mut macros,
        notify,
        hub,
        layout,
    } = setup;
    let mut forwarders: Vec<Forwarder> = nodes
        .iter()
        .map(|_| Forwarder::new(config.clone()))
        .collect();
    let owners: Vec<_> = nodes.iter().map(|_| output::new_owner()).collect();
    let feedback = sink
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .feedback_fd();
    let mut fds: Vec<libc::pollfd> = nodes
        .iter()
        .map(|node| util::pollfd(node.fd(), libc::POLLIN))
        .collect();
    let feedback_slot = fds.len();
    fds.push(util::pollfd(feedback.unwrap_or(-1), libc::POLLIN));
    let waker_slot = fds.len();
    fds.push(util::pollfd(waker.fd(), libc::POLLIN));
    let mut events: Vec<Ev> = Vec::with_capacity(128);
    let mut leds: Vec<Ev> = Vec::with_capacity(16);
    let mut side: Vec<Side> = Vec::new();

    adopted.store(generation, Ordering::Release);
    notify(Notice::Applied(generation));
    'outer: loop {
        if let Err(error) = util::poll(&mut fds, None) {
            eprintln!("remap poll failed: {error}");
            break;
        }
        if fds[waker_slot].revents != 0 {
            waker.drain();
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let updated = shared
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            for forwarder in &mut forwarders {
                forwarder.config = updated.0.clone();
            }
            macros = updated.1;
            generation = updated.2;
            adopted.store(generation, Ordering::Release);
            notify(Notice::Applied(generation));
        }
        let mut changed = false;
        for index in 0..nodes.len() {
            let revents = fds[index].revents;
            let forwarder = &mut forwarders[index];
            let mut emit = |events: &[Ev]| output::emit_owned(&sink, owners[index], events);
            if revents & libc::POLLIN != 0 {
                events.clear();
                if let Err(error) = nodes[index].read(&mut events) {
                    eprintln!("keyboard input node went away: {error}");
                    break 'outer;
                }
                for event in &events {
                    match forwarder.handle(*event, &mut emit, &mut side) {
                        Ok(flag) => {
                            changed |= flag;
                            if forwarder.resync {
                                match nodes[index]
                                    .key_state()
                                    .and_then(|held| forwarder.reconcile_state(held, &mut emit))
                                {
                                    Ok(()) => changed = true,
                                    Err(error) => {
                                        eprintln!("Input resynchronization failed: {error}");
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("virtual keyboard write failed: {error}");
                            break 'outer;
                        }
                    }
                }
            } else if revents & POLL_GONE != 0 {
                eprintln!("keyboard input node went away");
                break 'outer;
            }
        }
        held.store(
            forwarders
                .iter()
                .any(|forwarder| !forwarder.pressed.is_empty()),
            Ordering::Release,
        );
        for item in side.drain(..) {
            match item {
                Side::Queue(steps) => actions::submit(&macros, steps),
                other => notify(Notice::Side {
                    side: other,
                    generation,
                }),
            }
        }
        if fds[feedback_slot].revents & libc::POLLIN != 0 {
            leds.clear();
            let read = sink
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .read_feedback(&mut leds);
            if read.is_ok() {
                for led in leds.iter().filter(|event| event.kind == EV_LED) {
                    for node in nodes.iter_mut().filter(|node| node.has_leds()) {
                        let _ = node.send_led(led.code, led.value);
                    }
                }
            }
        }
        if changed {
            let pressed = forwarders
                .iter()
                .flat_map(|forwarder| forwarder.pressed.iter().copied())
                .collect();
            hub.publish_evdev(super::events::ids_for_codes(layout, &pressed));
        }
    }
    for (forwarder, owner) in forwarders.iter_mut().zip(owners) {
        forwarder.release_all(&mut |events| output::emit_owned(&sink, owner, events));
        let _ = output::release_owned(&sink, owner);
    }
    for node in &mut nodes {
        let _ = node.ungrab();
    }
    held.store(false, Ordering::Release);
    macros.cancel();
    hub.clear_evdev();
    drop(nodes);
    if !stop.load(Ordering::SeqCst) {
        notify(Notice::Stopped);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{MediaKey, parse_macro};
    use crate::daemon::output::tests::Recorder;
    use crate::devices::{Model, PhysicalLayout};
    use std::io::Write;
    use std::os::fd::FromRawFd;

    fn run(forwarder: &mut Forwarder, events: &[Ev]) -> (Vec<Vec<Ev>>, Vec<Side>) {
        let mut out = Vec::new();
        let mut side = Vec::new();
        for event in events {
            forwarder
                .handle(
                    *event,
                    &mut |batch: &[Ev]| {
                        out.push(batch.to_vec());
                        Ok(())
                    },
                    &mut side,
                )
                .unwrap();
        }
        (out, side)
    }

    const SYN: Ev = Ev {
        kind: EV_SYN,
        code: SYN_REPORT,
        value: 0,
    };
    const MSC: Ev = Ev {
        kind: output::EV_MSC,
        code: 4,
        value: 0x70039,
    };

    fn config() -> RemapConfig {
        let layout = layouts::layout(Model::K65PlusWireless, PhysicalLayout::Ansi);
        let mut live = DeviceLive::default();
        live.macros.insert(
            "hi".into(),
            Macro {
                source: "text:hi".into(),
                steps: parse_macro("text:hi").unwrap(),
            },
        );
        live.remaps.insert(
            "capslock".into(),
            Action::Key {
                combo: "ctrl".into(),
            },
        );
        live.remaps.insert("f1".into(), Action::Disabled);
        live.remaps
            .insert("f2".into(), Action::Macro { name: "hi".into() });
        live.remaps.insert(
            "f3".into(),
            Action::Media {
                key: MediaKey::Next,
            },
        );
        live.remaps.insert(
            "f4".into(),
            Action::Command {
                argv: vec!["true".into()],
            },
        );
        live.remaps
            .insert("f5".into(), Action::Brightness { delta: 10 });
        live.remaps.insert(
            "f6".into(),
            Action::Key {
                combo: "ctrl+shift+t".into(),
            },
        );
        RemapConfig::from_live(&live, layout)
    }

    #[test]
    fn unmapped_keys_pass_through_in_one_report() {
        let mut forwarder = Forwarder::new(config());
        let (out, side) = run(
            &mut forwarder,
            &[
                MSC,
                Ev::key(30, 1),
                SYN,
                Ev::key(30, 2),
                SYN,
                Ev::key(30, 0),
                SYN,
            ],
        );
        assert_eq!(
            out,
            vec![
                vec![Ev::key(30, 1)],
                vec![Ev::key(30, 2)],
                vec![Ev::key(30, 0)]
            ]
        );
        assert!(side.is_empty());
    }

    #[test]
    fn remapped_keys_are_transformed() {
        let mut forwarder = Forwarder::new(config());
        // Caps Lock -> Ctrl, held with repeat.
        let (out, _) = run(
            &mut forwarder,
            &[
                Ev::key(58, 1),
                SYN,
                Ev::key(58, 2),
                SYN,
                Ev::key(58, 0),
                SYN,
            ],
        );
        assert_eq!(
            out,
            vec![
                vec![Ev::key(29, 1)],
                vec![Ev::key(29, 2)],
                vec![Ev::key(29, 0)]
            ]
        );
        // F1 disabled.
        let (out, _) = run(&mut forwarder, &[Ev::key(59, 1), SYN, Ev::key(59, 0), SYN]);
        assert!(out.is_empty());
        // F6 -> ctrl+shift+t combo held while the key is.
        let (out, _) = run(&mut forwarder, &[Ev::key(64, 1), SYN, Ev::key(64, 0), SYN]);
        assert_eq!(
            out,
            vec![
                vec![Ev::key(29, 1), Ev::key(42, 1), Ev::key(20, 1)],
                vec![Ev::key(20, 0), Ev::key(42, 0), Ev::key(29, 0)]
            ]
        );
        // F3 -> next track.
        let (out, _) = run(&mut forwarder, &[Ev::key(61, 1), SYN, Ev::key(61, 0), SYN]);
        assert_eq!(
            out,
            vec![
                vec![Ev::key(output::KEY_NEXTSONG, 1)],
                vec![Ev::key(output::KEY_NEXTSONG, 0)]
            ]
        );
    }

    #[test]
    fn macros_commands_and_brightness_become_side_effects_on_press_only() {
        let mut forwarder = Forwarder::new(config());
        let (out, side) = run(
            &mut forwarder,
            &[
                Ev::key(60, 1),
                SYN,
                Ev::key(60, 2),
                SYN,
                Ev::key(60, 0),
                SYN,
                Ev::key(62, 1),
                SYN,
                Ev::key(62, 0),
                SYN,
                Ev::key(63, 1),
                SYN,
                Ev::key(63, 0),
                SYN,
            ],
        );
        assert!(out.is_empty());
        assert_eq!(side.len(), 3);
        assert!(matches!(&side[0], Side::Queue(steps) if !steps.is_empty()));
        assert_eq!(side[1], Side::Run(vec!["true".into()]));
        assert_eq!(side[2], Side::Brightness(10));
    }

    #[test]
    fn mixed_reports_keep_order_and_release_all_on_stop() {
        let mut forwarder = Forwarder::new(config());
        let (out, _) = run(
            &mut forwarder,
            &[Ev::key(30, 1), Ev::key(58, 1), Ev::key(31, 1), SYN],
        );
        assert_eq!(
            out,
            vec![
                vec![Ev::key(30, 1)],
                vec![Ev::key(29, 1)],
                vec![Ev::key(31, 1)]
            ]
        );
        assert_eq!(forwarder.pressed, [30, 31, 58].into());
        let mut released = Vec::new();
        forwarder.release_all(&mut |batch: &[Ev]| {
            released.push(batch.to_vec());
            Ok(())
        });
        assert_eq!(
            released,
            vec![vec![Ev::key(31, 0), Ev::key(30, 0), Ev::key(29, 0)]]
        );
        // A config change while a remapped key is held still releases what it pressed.
        let mut forwarder = Forwarder::new(config());
        run(&mut forwarder, &[Ev::key(58, 1), SYN]);
        forwarder.config.map.insert(58, Action::Disabled);
        let (out, _) = run(&mut forwarder, &[Ev::key(58, 0), SYN]);
        assert_eq!(out, vec![vec![Ev::key(29, 0)]]);
    }

    /// Fake node fed through a pipe of raw `Ev` triples; records grabs and LED writes.
    pub(crate) struct PipeNode {
        read: std::fs::File,
        pub leds: Arc<Mutex<Vec<(u16, i32)>>>,
        pub grabbed: Arc<AtomicBool>,
        pub fail_grab: bool,
    }

    impl PipeNode {
        pub fn new() -> (Self, std::fs::File) {
            let mut fds = [0; 2];
            // SAFETY: valid out-array for pipe2.
            assert_eq!(
                unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
                0
            );
            // SAFETY: fresh descriptors we own.
            let (read, write) = unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            };
            (
                Self {
                    read,
                    leds: Arc::default(),
                    grabbed: Arc::default(),
                    fail_grab: false,
                },
                write,
            )
        }
    }

    pub(crate) fn send(pipe: &mut std::fs::File, events: &[Ev]) {
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend_from_slice(&event.kind.to_le_bytes());
            bytes.extend_from_slice(&event.code.to_le_bytes());
            bytes.extend_from_slice(&event.value.to_le_bytes());
        }
        pipe.write_all(&bytes).unwrap();
    }

    impl InputNode for PipeNode {
        fn fd(&self) -> RawFd {
            self.read.as_raw_fd()
        }
        fn read(&mut self, out: &mut Vec<Ev>) -> io::Result<()> {
            use std::io::Read;
            let mut buffer = [0u8; 8 * 32];
            loop {
                match self.read.read(&mut buffer) {
                    Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                    Ok(n) => out.extend(buffer[..n].as_chunks::<8>().0.iter().map(|c| Ev {
                        kind: u16::from_le_bytes([c[0], c[1]]),
                        code: u16::from_le_bytes([c[2], c[3]]),
                        value: i32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                    })),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        }
        fn keys(&self) -> Vec<u16> {
            output::keyboard_keys([])
        }
        fn has_leds(&self) -> bool {
            true
        }
        fn key_state(&self) -> io::Result<BTreeSet<u16>> {
            Ok(BTreeSet::new())
        }
        fn ungrab(&mut self) -> io::Result<()> {
            self.grabbed.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn grab(&mut self) -> io::Result<()> {
            if self.fail_grab {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.grabbed.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn send_led(&mut self, code: u16, value: i32) -> io::Result<()> {
            self.leds.lock().unwrap().push((code, value));
            Ok(())
        }
    }

    #[test]
    fn forwarding_thread_transforms_publishes_and_releases_on_stop() {
        let layout = layouts::layout(Model::K65PlusWireless, PhysicalLayout::Ansi);
        let (node, mut pipe) = PipeNode::new();
        let (recorder, batches) = Recorder::new();
        let sink = output::share(Box::new(recorder));
        let runner = actions::MacroRunner::start(sink.clone()).unwrap();
        let hub = Arc::new(KeyHub::default());
        let keys = hub.subscribe();
        let notices = Arc::new(Mutex::new(Vec::new()));
        let seen = notices.clone();
        let handle = RemapHandle::start(RemapSetup {
            nodes: vec![Box::new(node)],
            sink,
            config: config(),
            generation: 1,
            macros: runner.queue(),
            notify: Box::new(move |notice| seen.lock().unwrap().push(format!("{notice:?}"))),
            hub: hub.clone(),
            layout,
        })
        .unwrap();
        assert_eq!(
            keys.recv_timeout(Duration::from_secs(1)).unwrap(),
            Vec::<String>::new()
        );
        send(&mut pipe, &[Ev::key(58, 1), SYN]);
        assert_eq!(
            keys.recv_timeout(Duration::from_secs(2)).unwrap(),
            vec!["capslock".to_string()]
        );
        send(&mut pipe, &[Ev::key(62, 1), SYN]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !notices
            .lock()
            .unwrap()
            .iter()
            .any(|notice| notice.contains("Run"))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            notices
                .lock()
                .unwrap()
                .iter()
                .any(|notice| notice.contains("Run"))
        );
        assert!(handle.is_running());
        drop(handle);
        let batches = batches.lock().unwrap();
        assert_eq!(batches.first().unwrap(), &vec![Ev::key(29, 1)]);
        assert_eq!(
            batches.last().unwrap(),
            &vec![Ev::key(29, 0)],
            "held keys released on stop"
        );
        assert_eq!(
            notices.lock().unwrap().len(),
            2,
            "a requested stop is not reported"
        );
    }

    struct HeldNode {
        pipe: PipeNode,
        held: bool,
        authoritative: bool,
    }
    impl InputNode for HeldNode {
        fn fd(&self) -> RawFd {
            self.pipe.fd()
        }
        fn read(&mut self, out: &mut Vec<Ev>) -> io::Result<()> {
            self.pipe.read(out)
        }
        fn keys(&self) -> Vec<u16> {
            self.pipe.keys()
        }
        fn has_leds(&self) -> bool {
            false
        }
        fn key_state(&self) -> io::Result<BTreeSet<u16>> {
            if !self.authoritative {
                return Err(io::ErrorKind::Unsupported.into());
            }
            Ok(if self.held {
                BTreeSet::from([30])
            } else {
                BTreeSet::new()
            })
        }
        fn grab(&mut self) -> io::Result<()> {
            self.pipe.grab()
        }
        fn ungrab(&mut self) -> io::Result<()> {
            self.pipe.ungrab()
        }
        fn send_led(&mut self, _: u16, _: i32) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn held_key_timeout_unknown_state_and_shutdown_refuse_grab() {
        for (held, authoritative, cancelled) in [
            (true, true, false),
            (false, false, false),
            (false, true, true),
        ] {
            let (pipe, _write) = PipeNode::new();
            let grabbed = pipe.grabbed.clone();
            let mut nodes: Vec<Box<dyn InputNode>> = vec![Box::new(HeldNode {
                pipe,
                held,
                authoritative,
            })];
            assert!(grab_until(&mut nodes, &AtomicBool::new(cancelled), Instant::now()).is_err());
            assert!(!grabbed.load(Ordering::SeqCst));
        }
    }
    #[test]
    fn delivered_pregrab_events_are_drained_without_forwarding() {
        let (pipe, mut write) = PipeNode::new();
        let grabbed = pipe.grabbed.clone();
        send(&mut write, &[Ev::key(30, 1), SYN, Ev::key(30, 0), SYN]);
        let mut nodes: Vec<Box<dyn InputNode>> = vec![Box::new(pipe)];
        grab_all(&mut nodes, &AtomicBool::new(false)).unwrap();
        assert!(grabbed.load(Ordering::SeqCst));
        let mut pending = Vec::new();
        nodes[0].read(&mut pending).unwrap();
        assert!(pending.is_empty());
        nodes[0].ungrab().unwrap();
    }
    #[test]
    fn syn_dropped_skips_intermediate_actions_and_reconciles_authoritative_state() {
        let mut f = Forwarder::new(config());
        let mut batches = Vec::new();
        let mut side = Vec::new();
        let mut emit = |batch: &[Ev]| {
            batches.extend_from_slice(batch);
            Ok(())
        };
        f.handle(Ev::key(58, 1), &mut emit, &mut side).unwrap();
        f.handle(
            Ev {
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            &mut emit,
            &mut side,
        )
        .unwrap();
        f.handle(Ev::key(62, 1), &mut emit, &mut side).unwrap();
        f.handle(SYN, &mut emit, &mut side).unwrap();
        assert!(side.is_empty());
        assert!(f.resync);
        f.reconcile_state(BTreeSet::from([62]), &mut emit).unwrap();
        assert!(side.is_empty());
        assert_eq!(f.pressed, BTreeSet::from([62]));
        assert!(!f.down.contains(&29));
        assert!(batches.contains(&Ev::key(29, 0)));
    }
    #[test]
    fn failed_grab_leaves_nothing_grabbed() {
        let (first, _pipe1) = PipeNode::new();
        let (mut second, _pipe2) = PipeNode::new();
        second.fail_grab = true;
        let grabbed = first.grabbed.clone();
        let mut nodes: Vec<Box<dyn InputNode>> = vec![Box::new(first), Box::new(second)];
        assert!(grab_all(&mut nodes, &AtomicBool::new(false)).is_err());
        assert!(
            !grabbed.load(Ordering::SeqCst),
            "partial acquisition rolled back immediately"
        );
        drop(nodes); // dropping releases (EvdevNode ungrabs on drop)
    }
}

#[cfg(test)]
mod safety_regressions {
    use super::*;
    fn event(f: &mut Forwarder, code: u16, value: i32) -> Vec<Ev> {
        let mut out = Vec::new();
        let mut side = Vec::new();
        let mut emit = |batch: &[Ev]| {
            out.extend_from_slice(batch);
            Ok(())
        };
        f.handle(Ev::key(code, value), &mut emit, &mut side)
            .unwrap();
        f.handle(
            Ev {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut emit,
            &mut side,
        )
        .unwrap();
        out
    }
    #[test]
    fn removal_keeps_original_press_disposition() {
        let mut f = Forwarder::new(RemapConfig::default());
        f.config.map.insert(
            58,
            Action::Key {
                combo: "ctrl".into(),
            },
        );
        assert_eq!(event(&mut f, 58, 1), vec![Ev::key(29, 1)]);
        f.config.map.clear();
        assert_eq!(event(&mut f, 58, 2), vec![Ev::key(29, 2)]);
        assert_eq!(event(&mut f, 58, 0), vec![Ev::key(29, 0)]);
    }
    #[test]
    fn addition_keeps_original_pass_through_disposition() {
        let mut f = Forwarder::new(RemapConfig::default());
        assert_eq!(event(&mut f, 30, 1), vec![Ev::key(30, 1)]);
        f.config.map.insert(30, Action::Disabled);
        assert_eq!(event(&mut f, 30, 2), vec![Ev::key(30, 2)]);
        assert_eq!(event(&mut f, 30, 0), vec![Ev::key(30, 0)]);
    }
    #[test]
    fn overlapping_physical_and_remapped_keys_keep_modifier_held() {
        let mut f = Forwarder::new(RemapConfig::default());
        f.config.map.insert(
            58,
            Action::Key {
                combo: "ctrl".into(),
            },
        );
        event(&mut f, 29, 1);
        event(&mut f, 58, 1);
        assert!(event(&mut f, 58, 0).is_empty());
        assert_eq!(event(&mut f, 29, 0), vec![Ev::key(29, 0)]);
    }
    #[test]
    fn failed_release_is_retried_at_cleanup() {
        let mut f = Forwarder::new(RemapConfig::default());
        f.config.map.insert(
            58,
            Action::Key {
                combo: "ctrl".into(),
            },
        );
        event(&mut f, 58, 1);
        let mut side = Vec::new();
        assert!(
            f.handle(
                Ev::key(58, 0),
                &mut |_| Err(io::ErrorKind::BrokenPipe.into()),
                &mut side
            )
            .is_err()
        );
        let mut out = Vec::new();
        f.release_all(&mut |batch| {
            out.extend_from_slice(batch);
            Ok(())
        });
        assert_eq!(out, vec![Ev::key(29, 0)]);
    }
}

#[cfg(test)]
mod dropped_pending_regressions {
    use super::*;
    #[test]
    fn dropped_report_discards_unflushed_up_but_snapshot_still_releases_actual_down() {
        let mut f = Forwarder::default();
        let mut out = Vec::new();
        let mut side = Vec::new();
        let mut emit = |batch: &[Ev]| {
            out.extend_from_slice(batch);
            Ok(())
        };
        let report = Ev {
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };
        f.handle(Ev::key(30, 1), &mut emit, &mut side).unwrap();
        f.handle(report, &mut emit, &mut side).unwrap();
        f.handle(Ev::key(30, 0), &mut emit, &mut side).unwrap();
        f.handle(
            Ev {
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            &mut emit,
            &mut side,
        )
        .unwrap();
        f.handle(report, &mut emit, &mut side).unwrap();
        f.reconcile_state(BTreeSet::new(), &mut emit).unwrap();
        assert_eq!(out, vec![Ev::key(30, 1), Ev::key(30, 0)]);
        assert!(f.down.is_empty());
    }
}
