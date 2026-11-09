//! `--input-stream <id>`: key-tester NDJSON frames `{"status","pressed":[viz key ids]|null}`.
//! Sources in order: the companion's `keys` subscription (when it owns or grabs the keyboard),
//! read-only non-exclusive evdev, else one idle `pressed: null` frame. Key data is never logged.

use super::labels;
use crate::devices::{self, Keyboard, PhysicalLayout};
use crate::ipc;
use crate::layouts;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

pub const IDLE_STATUS: &str = "Install the input rule to see live key presses";
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const KEY_BYTES: usize = 0x300 / 8;

#[derive(Serialize)]
struct Frame<'a> {
    status: &'a str,
    pressed: Option<Vec<String>>,
}

/// Write one frame; Ok(false) when the reader went away (clean exit).
fn emit(out: &mut dyn Write, status: &str, pressed: Option<&BTreeSet<String>>) -> Result<bool> {
    let frame = Frame {
        status,
        pressed: pressed.map(|set| set.iter().cloned().collect()),
    };
    let mut line = serde_json::to_vec(&frame)?;
    line.push(b'\n');
    match out.write_all(&line).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(error).context("Writing key frame"),
    }
}

/// Key frames (layout key ids) from the companion.
pub type KeyFrames = Box<dyn Iterator<Item = Result<Vec<String>>>>;

/// Sources the stream can use; the real ones talk to the daemon and /dev/input.
pub trait Sources {
    /// The daemon's key frames (layout key ids) when it owns or grabs `keyboard`.
    fn daemon_keys(&self, keyboard: &Keyboard) -> Result<Option<KeyFrames>>;
    /// Read-only, non-blocking evdev fds for the keyboard's input nodes (errors per node).
    fn open_evdev(&self, keyboard: &Keyboard) -> Vec<io::Result<OwnedFd>>;
    /// Block until the stream's reader goes away (idle mode).
    fn wait_for_reader_exit(&self);
    /// fd whose POLLHUP/POLLERR means the reader went away (stdout), if any.
    fn hup_fd(&self) -> Option<RawFd>;
    /// Production periodically reselects sources; finite test streams keep their old contract.
    fn reselect(&self) -> bool {
        false
    }
    fn rediscover(&self, _id: &str) -> Option<Keyboard> {
        None
    }
}

struct System;

impl Sources for System {
    fn daemon_keys(&self, keyboard: &Keyboard) -> Result<Option<KeyFrames>> {
        let Ok(Some(devices)) = ipc::hello() else {
            return Ok(None);
        };
        let handled = devices.iter().any(|status| {
            status.id == keyboard.id
                && ((status.owned && status.mode == crate::bragi::MODE_SOFTWARE) || status.grabbed)
        });
        if !handled {
            return Ok(None);
        }
        Ok(
            ipc::subscribe_keys_timed(&keyboard.id, std::time::Duration::from_millis(500))?.map(
                |frames| {
                    Box::new(frames.map(|frame| frame.map(|frame| frame.pressed))) as KeyFrames
                },
            ),
        )
    }
    fn open_evdev(&self, keyboard: &Keyboard) -> Vec<io::Result<OwnedFd>> {
        keyboard
            .evdev_nodes
            .iter()
            .map(|node| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(node)
                    .map(OwnedFd::from)
            })
            .collect()
    }
    fn wait_for_reader_exit(&self) {
        {
            let mut fd = libc::pollfd {
                fd: libc::STDOUT_FILENO,
                events: 0,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let rc = unsafe { libc::poll(&mut fd, 1, 500) };
            if rc > 0 && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return;
            }
            if rc < 0 && io::Error::last_os_error().kind() != ErrorKind::Interrupted {
                // Not pollable (e.g. a regular file): idle until killed.
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
    fn hup_fd(&self) -> Option<RawFd> {
        Some(libc::STDOUT_FILENO)
    }
    fn reselect(&self) -> bool {
        true
    }
    fn rediscover(&self, id: &str) -> Option<Keyboard> {
        devices::find(id).ok()
    }
}

pub fn run(device_id: &str) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    loop {
        if let Some(keyboard) = System.rediscover(device_id) {
            return stream(&keyboard, &mut out, &System);
        }
        if !emit(&mut out, "Keyboard input unavailable — reconnecting", None)?
            || reader_gone(System.hup_fd())
        {
            return Ok(());
        }
        System.wait_for_reader_exit();
    }
}

/// evdev keycode -> visualization ids (both physical layouts: unknown ids are ignored by the host).
fn evdev_map(keyboard: &Keyboard) -> BTreeMap<u16, Vec<String>> {
    let mut map: BTreeMap<u16, Vec<String>> = BTreeMap::new();
    for physical in [PhysicalLayout::Ansi, PhysicalLayout::Iso] {
        for key in layouts::layout(keyboard.model, physical).keys {
            if let Some(code) = key.evdev {
                let ids = map.entry(code).or_default();
                let id = labels::viz_id(key.id);
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    map
}

pub fn stream(keyboard: &Keyboard, out: &mut dyn Write, sources: &dyn Sources) -> Result<()> {
    let mut current = keyboard.clone();
    loop {
        let result = stream_once(&current, out, sources);
        if !sources.reselect() || reader_gone(sources.hup_fd()) {
            return result;
        }
        if result.is_ok() {
            return Ok(());
        }
        if !emit(out, "Key source unavailable — reconnecting", None)? {
            return Ok(());
        }
        loop {
            sources.wait_for_reader_exit();
            if reader_gone(sources.hup_fd()) {
                return Ok(());
            }
            if let Some(found) = sources.rediscover(&current.id) {
                current = found;
                break;
            }
        }
    }
}

fn reader_gone(fd: Option<RawFd>) -> bool {
    let Some(fd) = fd else {
        return false;
    };
    let mut poll = libc::pollfd {
        fd,
        events: 0,
        revents: 0,
    };
    // SAFETY: one live pollfd, no memory beyond the struct is accessed.
    unsafe {
        libc::poll(&mut poll, 1, 0) > 0
            && poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }
}

fn stream_once(keyboard: &Keyboard, out: &mut dyn Write, sources: &dyn Sources) -> Result<()> {
    if let Some(frames) = sources.daemon_keys(keyboard)? {
        let status = "Live keys · companion";
        if !emit(out, status, Some(&BTreeSet::new()))? {
            return Ok(());
        }
        for frame in frames {
            let frame = match frame {
                Err(error)
                    if sources.reselect()
                        && error.chain().any(|cause| {
                            cause.downcast_ref::<io::Error>().is_some_and(|error| {
                                matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                            })
                        }) =>
                {
                    if reader_gone(sources.hup_fd()) {
                        return Ok(());
                    }
                    if sources.rediscover(&keyboard.id).as_ref() != Some(keyboard) {
                        bail!("Key source changed");
                    }
                    continue;
                }
                other => other?,
            };
            let pressed: BTreeSet<String> = frame
                .iter()
                .take(512)
                .map(|id| labels::viz_id(id))
                .collect();
            if !emit(out, status, Some(&pressed))? {
                return Ok(());
            }
        }
        bail!("The live-mode companion stopped the key stream");
    }
    let opened = sources.open_evdev(keyboard);
    let denied = opened
        .iter()
        .any(|result| matches!(result, Err(error) if error.kind() == ErrorKind::PermissionDenied));
    let fds: Vec<OwnedFd> = opened.into_iter().filter_map(Result::ok).collect();
    if fds.is_empty() {
        let status = if keyboard.evdev_nodes.is_empty() {
            "No key input device found for this keyboard"
        } else if denied {
            IDLE_STATUS
        } else {
            "Key input is not available right now"
        };
        if emit(out, status, None)? {
            sources.wait_for_reader_exit();
        }
        if sources.reselect() {
            bail!("No input source; retrying");
        }
        return Ok(());
    }
    evdev_loop(&fds, &evdev_map(keyboard), out, sources, keyboard)
}

/// EVIOCGKEY: currently pressed keycodes (empty when the fd is not an evdev node).
fn key_state(fd: RawFd) -> BTreeSet<u16> {
    let mut bits = [0u8; KEY_BYTES];
    let request = (2u64 << 30) | ((KEY_BYTES as u64) << 16) | (u64::from(b'E') << 8) | 0x18;
    // SAFETY: the buffer is KEY_BYTES long, matching the size encoded in the request.
    let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, bits.as_mut_ptr()) };
    if rc < 0 {
        return BTreeSet::new();
    }
    (0..KEY_BYTES * 8)
        .filter(|bit| bits[bit / 8] & (1 << (bit % 8)) != 0)
        .map(|bit| bit as u16)
        .collect()
}

fn pressed_ids(states: &[BTreeSet<u16>], map: &BTreeMap<u16, Vec<String>>) -> BTreeSet<String> {
    states
        .iter()
        .flatten()
        .filter_map(|code| map.get(code))
        .flatten()
        .take(512)
        .cloned()
        .collect()
}

fn evdev_loop(
    fds: &[OwnedFd],
    map: &BTreeMap<u16, Vec<String>>,
    out: &mut dyn Write,
    sources: &dyn Sources,
    keyboard: &Keyboard,
) -> Result<()> {
    let hup_fd = sources.hup_fd();
    const STATUS: &str = "Live keys · read-only input";
    let mut states: Vec<BTreeSet<u16>> = fds.iter().map(|fd| key_state(fd.as_raw_fd())).collect();
    let mut dropped = vec![false; fds.len()];
    let mut last = pressed_ids(&states, map);
    if !emit(out, STATUS, Some(&last))? {
        return Ok(());
    }
    let event_size = std::mem::size_of::<libc::input_event>();
    let mut events = vec![
        // SAFETY: input_event is plain old data; all-zero is valid.
        unsafe { std::mem::zeroed::<libc::input_event>() };
        64
    ];
    let mut next_check = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if sources.reselect() && std::time::Instant::now() >= next_check {
            if sources.rediscover(&keyboard.id).as_ref() != Some(keyboard)
                || sources.daemon_keys(keyboard)?.is_some()
            {
                bail!("Key source changed — reconnecting");
            }
            next_check = std::time::Instant::now() + std::time::Duration::from_millis(500);
        }
        let mut polls: Vec<libc::pollfd> = fds
            .iter()
            .map(|fd| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if let Some(fd) = hup_fd {
            polls.push(libc::pollfd {
                fd,
                events: 0,
                revents: 0,
            });
        }
        // SAFETY: `polls` is a valid array of pollfd.
        let rc = unsafe {
            libc::poll(
                polls.as_mut_ptr(),
                polls.len() as libc::nfds_t,
                if sources.reselect() { 500 } else { -1 },
            )
        };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("Waiting for key input");
        }
        if hup_fd.is_some()
            && polls[fds.len()].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
        {
            return Ok(());
        }
        for (index, poll) in polls.iter().take(fds.len()).enumerate() {
            if poll.revents == 0 {
                continue;
            }
            if poll.revents & libc::POLLIN == 0 {
                bail!("Keyboard input device disconnected");
            }
            // SAFETY: the buffer holds `events.len()` input_event records.
            let read = unsafe {
                libc::read(
                    poll.fd,
                    events.as_mut_ptr().cast(),
                    events.len() * event_size,
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
                    continue;
                }
                return Err(error).context("Reading key input");
            }
            if read == 0 {
                bail!("Keyboard input device disconnected");
            }
            let count = read as usize / event_size;
            for event in &events[..count] {
                match (event.type_, event.code) {
                    (EV_SYN, SYN_DROPPED) => dropped[index] = true,
                    (EV_SYN, SYN_REPORT) => {
                        if dropped[index] {
                            states[index] = key_state(poll.fd);
                            dropped[index] = false;
                        }
                        let now = pressed_ids(&states, map);
                        if now != last {
                            if !emit(out, STATUS, Some(&now))? {
                                return Ok(());
                            }
                            last = now;
                        }
                    }
                    (EV_KEY, code) if !dropped[index] => match event.value {
                        0 => {
                            states[index].remove(&code);
                        }
                        1 => {
                            states[index].insert(code);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod source_reselection_regressions {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::os::fd::FromRawFd;
    struct Switching {
        keyboard: Keyboard,
        reader: RefCell<Option<OwnedFd>>,
        calls: Cell<usize>,
    }
    impl Sources for Switching {
        fn daemon_keys(&self, _: &Keyboard) -> Result<Option<KeyFrames>> {
            let calls = self.calls.get();
            self.calls.set(calls + 1);
            if calls == 0 {
                return Ok(None);
            }
            Ok(Some(Box::new(vec![Ok(vec!["a".into()])].into_iter())))
        }
        fn open_evdev(&self, _: &Keyboard) -> Vec<io::Result<OwnedFd>> {
            self.reader
                .borrow_mut()
                .take()
                .into_iter()
                .map(Ok)
                .collect()
        }
        fn wait_for_reader_exit(&self) {}
        fn hup_fd(&self) -> Option<RawFd> {
            None
        }
        fn reselect(&self) -> bool {
            true
        }
        fn rediscover(&self, _: &str) -> Option<Keyboard> {
            Some(self.keyboard.clone())
        }
    }
    struct StopAfter {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for StopAfter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.iter().filter(|byte| **byte == b'\n').count() >= self.limit {
                return Err(ErrorKind::BrokenPipe.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn silent_evdev_switches_to_companion_and_clears_previous_source() {
        let mut fds = [0; 2];
        // SAFETY: fresh owned pipe descriptors, used only as idle mock input.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) },
            0
        );
        let (reader, _writer) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let keyboard = Keyboard {
            id: "k65-test".into(),
            model: crate::devices::Model::K65PlusWireless,
            link: crate::devices::LinkKind::Usb,
            name: "K65",
            vid: 0x1b1c,
            pid: 0x2b10,
            serial: "mock".into(),
            usb_path: "/nonexistent".into(),
            cmd_node: "/nonexistent".into(),
            event_node: None,
            evdev_nodes: vec![],
            frame_len: 64,
            target: 8,
        };
        let sources = Switching {
            keyboard: keyboard.clone(),
            reader: RefCell::new(Some(reader)),
            calls: Cell::new(0),
        };
        let mut out = StopAfter {
            bytes: Vec::new(),
            limit: 4,
        };
        let start = std::time::Instant::now();
        stream(&keyboard, &mut out, &sources).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        let frames: Vec<serde_json::Value> = String::from_utf8(out.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(frames[0]["pressed"], serde_json::json!([]));
        assert!(frames[1]["pressed"].is_null());
        assert_eq!(frames[3]["pressed"], serde_json::json!(["a"]));
    }
}
