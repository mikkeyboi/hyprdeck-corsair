//! Turning configured [`Action`]s, macros, text and dial modes into input event plans, the macro
//! runner thread and detached command spawning. Never logs key contents.

use super::output::{
    self, Ev, KEY_0, KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_MUTE, KEY_NEXTSONG, KEY_PLAYPAUSE,
    KEY_PREVIOUSSONG, KEY_STOPCD, KEY_VOLUMEDOWN, KEY_VOLUMEUP, SharedSink,
};
use crate::config::{Action, DialConfig, DialMode, Macro, MacroStep, MediaKey};
use crate::layouts::keynames;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Pause between key batches of typed text and macros (some clients drop faster input).
pub const KEY_GAP: Duration = Duration::from_millis(3);
/// Most dial detents acted on from one report.
const MAX_DETENTS: i32 = 10;
/// Brightness step per dial detent, percent.
pub const DIAL_BRIGHTNESS_STEP: i32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Emit(Vec<Ev>),
    Sleep(Duration),
}

/// One thing to do in response to a trigger (dial detent/press or remapped key press).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Short tap batches, emitted inline on the virtual keyboard.
    Tap(Vec<u16>),
    /// Wheel detents on the virtual dial pointer (+ = up), optionally with Ctrl held.
    Wheel { ctrl: bool, detents: i32 },
    /// Keyboard backlight step in percent.
    Brightness(i32),
    /// Spawn argv detached.
    Run(Vec<String>),
    /// Longer sequences (text, macros) for the macro runner thread.
    Queue(Vec<Step>),
}

pub fn media_code(key: MediaKey) -> u16 {
    match key {
        MediaKey::PlayPause => KEY_PLAYPAUSE,
        MediaKey::Next => KEY_NEXTSONG,
        MediaKey::Previous => KEY_PREVIOUSSONG,
        MediaKey::Stop => KEY_STOPCD,
        MediaKey::Mute => KEY_MUTE,
        MediaKey::VolumeUp => KEY_VOLUMEUP,
        MediaKey::VolumeDown => KEY_VOLUMEDOWN,
    }
}

/// Keycodes of a combo, modifiers first.
pub fn combo_codes(combo: &str) -> Result<Vec<u16>> {
    keynames::parse_combo(combo).map_err(|error| anyhow::anyhow!("{error}"))
}

pub fn press(codes: &[u16]) -> Vec<Ev> {
    codes.iter().map(|code| Ev::key(*code, 1)).collect()
}

pub fn release(codes: &[u16]) -> Vec<Ev> {
    codes.iter().rev().map(|code| Ev::key(*code, 0)).collect()
}

/// Press all keys (one report), then release them in reverse order (one report).
pub fn tap_batches(codes: &[u16]) -> [Vec<Ev>; 2] {
    [press(codes), release(codes)]
}

/// US-layout keycode and Shift for a character.
pub fn us_char(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    let plain = |code| Some((code, false));
    let shifted = |code| Some((code, true));
    match c {
        'a'..='z' => plain(LETTERS[(c as u8 - b'a') as usize]),
        'A'..='Z' => shifted(LETTERS[(c as u8 - b'A') as usize]),
        '1'..='9' => plain(2 + (c as u8 - b'1') as u16),
        '0' => plain(KEY_0),
        ' ' => plain(57),
        '\n' => plain(28),
        '\t' => plain(15),
        '-' => plain(12),
        '_' => shifted(12),
        '=' => plain(13),
        '+' => shifted(13),
        '[' => plain(26),
        '{' => shifted(26),
        ']' => plain(27),
        '}' => shifted(27),
        ';' => plain(39),
        ':' => shifted(39),
        '\'' => plain(40),
        '"' => shifted(40),
        '`' => plain(41),
        '~' => shifted(41),
        '\\' => plain(43),
        '|' => shifted(43),
        ',' => plain(51),
        '<' => shifted(51),
        '.' => plain(52),
        '>' => shifted(52),
        '/' => plain(53),
        '?' => shifted(53),
        other => {
            let index = SHIFTED_DIGITS.find(other)?;
            shifted(if index == 0 { KEY_0 } else { 1 + index as u16 })
        }
    }
}

fn push_tap(steps: &mut Vec<Step>, codes: &[u16]) {
    for batch in tap_batches(codes) {
        steps.push(Step::Emit(batch));
        steps.push(Step::Sleep(KEY_GAP));
    }
}

/// Type `text` on a US layout.
pub fn text_steps(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::with_capacity(text.len() * 4);
    for c in text.chars() {
        let (code, shift) =
            us_char(c).context("Text contains a character the US layout cannot type")?;
        if shift {
            push_tap(&mut steps, &[KEY_LEFTSHIFT, code]);
        } else {
            push_tap(&mut steps, &[code]);
        }
    }
    Ok(steps)
}

fn key_code(name: &str) -> Result<u16> {
    keynames::code(name).with_context(|| format!("Unknown key {name:?}"))
}

/// Expand macro steps into emit/sleep steps.
pub fn macro_steps(steps: &[MacroStep]) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    for step in steps {
        match step {
            MacroStep::Tap { combo } => push_tap(&mut out, &combo_codes(combo)?),
            MacroStep::Down { key } => {
                out.push(Step::Emit(vec![Ev::key(key_code(key)?, 1)]));
                out.push(Step::Sleep(KEY_GAP));
            }
            MacroStep::Up { key } => {
                out.push(Step::Emit(vec![Ev::key(key_code(key)?, 0)]));
                out.push(Step::Sleep(KEY_GAP));
            }
            MacroStep::Text { text } => out.extend(text_steps(text)?),
            MacroStep::Delay { ms } => out.push(Step::Sleep(Duration::from_millis((*ms).into()))),
            MacroStep::Media { key } => push_tap(&mut out, &[media_code(*key)]),
        }
    }
    Ok(out)
}

/// Ops for a press-style trigger of `action` (remapped key press, dial press, custom detent).
pub fn action_ops(action: &Action, macros: &BTreeMap<String, Macro>) -> Result<Vec<Op>> {
    Ok(match action {
        Action::Key { combo } => vec![Op::Tap(combo_codes(combo)?)],
        Action::Text { text } => vec![Op::Queue(text_steps(text)?)],
        Action::Macro { name } => {
            let item = macros
                .get(name)
                .with_context(|| format!("Macro {name:?} does not exist"))?;
            vec![Op::Queue(macro_steps(&item.steps)?)]
        }
        Action::Media { key } => vec![Op::Tap(vec![media_code(*key)])],
        Action::Command { argv } => {
            if argv.is_empty() {
                bail!("Empty command");
            }
            vec![Op::Run(argv.clone())]
        }
        Action::Disabled => Vec::new(),
        Action::Brightness { delta } => vec![Op::Brightness(*delta)],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialEvent {
    Rotate(i32),
    Press,
}

fn workspace(step: &str) -> Op {
    Op::Run(vec![
        "hyprctl".into(),
        "dispatch".into(),
        "workspace".into(),
        step.into(),
    ])
}

/// What the dial does for `event` in the configured mode. Clockwise means "more": volume up,
/// scroll down the page, zoom in, brighter, next track, next workspace.
pub fn dial_ops(
    dial: &DialConfig,
    macros: &BTreeMap<String, Macro>,
    event: DialEvent,
) -> Result<Vec<Op>> {
    let (detents, clockwise) = match event {
        DialEvent::Rotate(0) => return Ok(Vec::new()),
        DialEvent::Rotate(n) => (n.clamp(-MAX_DETENTS, MAX_DETENTS).abs(), n > 0),
        DialEvent::Press => (0, false),
    };
    let repeat = |op: Op| vec![op; detents as usize];
    let pick = |cw: Op, ccw: Op| repeat(if clockwise { cw } else { ccw });
    let sign = if clockwise { 1 } else { -1 };
    let press = event == DialEvent::Press;
    Ok(match dial.mode {
        DialMode::Volume if press => vec![Op::Tap(vec![KEY_MUTE])],
        DialMode::Volume => pick(Op::Tap(vec![KEY_VOLUMEUP]), Op::Tap(vec![KEY_VOLUMEDOWN])),
        DialMode::Scroll if press => Vec::new(),
        DialMode::Scroll => vec![Op::Wheel {
            ctrl: false,
            detents: -sign * detents,
        }],
        DialMode::Zoom if press => vec![Op::Tap(vec![KEY_LEFTCTRL, KEY_0])],
        DialMode::Zoom => vec![Op::Wheel {
            ctrl: true,
            detents: sign * detents,
        }],
        DialMode::Brightness if press => Vec::new(),
        DialMode::Brightness => vec![Op::Brightness(sign * detents * DIAL_BRIGHTNESS_STEP)],
        DialMode::Media if press => vec![Op::Tap(vec![KEY_PLAYPAUSE])],
        DialMode::Media => pick(Op::Tap(vec![KEY_NEXTSONG]), Op::Tap(vec![KEY_PREVIOUSSONG])),
        DialMode::Workspace if press => Vec::new(),
        DialMode::Workspace => pick(workspace("e+1"), workspace("e-1")),
        DialMode::Custom => {
            let action = if press {
                &dial.press
            } else if clockwise {
                &dial.clockwise
            } else {
                &dial.counter_clockwise
            };
            let Some(action) = action else {
                return Ok(Vec::new());
            };
            let ops = action_ops(action, macros)?;
            let times = if press { 1 } else { detents as usize };
            // Brightness steps from several detents are combined into one write.
            if let [Op::Brightness(delta)] = ops.as_slice() {
                return Ok(vec![Op::Brightness(delta * times as i32)]);
            }
            std::iter::repeat_n(ops, times).flatten().collect()
        }
    })
}

/// Wheel batches for `detents` (hi-res units of 120 per detent), optionally wrapped in Ctrl.
pub fn wheel_batch(detents: i32) -> Vec<Ev> {
    vec![
        Ev::rel(output::REL_WHEEL, detents),
        Ev::rel(output::REL_WHEEL_HI_RES, detents * 120),
    ]
}

/// Spawn `argv` detached: no shell, own session, stdio to /dev/null; reaped by a thread.
pub fn spawn_detached(argv: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (program, args) = argv.split_first().context("Empty command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and is the only call made between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Starting {program:?}"))?;
    std::thread::Builder::new()
        .name("reap".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("Starting the reaper thread")?;
    Ok(())
}

/// Queue scope is a configuration generation/source lifetime. Old cloned queues become inert
/// on reload or input-source loss, including queued work that has not started yet.
#[derive(Clone)]
pub struct MacroQueue {
    tx: SyncSender<MacroJob>,
    cancelled: Arc<AtomicBool>,
    generation: u64,
}
struct MacroJob {
    steps: Vec<Step>,
    generation: u64,
    owner: output::OutputOwner,
}
impl MacroQueue {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}
pub fn submit(queue: &MacroQueue, steps: Vec<Step>) {
    if queue.cancelled.load(Ordering::SeqCst) {
        return;
    }
    let job = MacroJob {
        steps,
        generation: queue.generation,
        owner: output::new_owner(),
    };
    match queue.tx.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => eprintln!("macro queue full; dropped one macro"),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Runs macro/text jobs on its own thread so key forwarding never waits.
pub struct MacroRunner {
    queue: MacroQueue,
    thread: Option<JoinHandle<()>>,
}
impl MacroRunner {
    pub fn start(sink: SharedSink) -> Result<Self> {
        let (tx, jobs) = std::sync::mpsc::sync_channel::<MacroJob>(8);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let generation = output::new_owner();
        let thread = std::thread::Builder::new()
            .name("macros".into())
            .spawn(move || run_jobs(&jobs, &sink, &flag, generation))
            .context("Starting the macro thread")?;
        Ok(Self {
            queue: MacroQueue {
                tx,
                cancelled,
                generation,
            },
            thread: Some(thread),
        })
    }
    pub fn queue(&self) -> MacroQueue {
        self.queue.clone()
    }
    pub fn is_cancelled(&self) -> bool {
        self.queue.cancelled.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.queue.cancel();
    }
}
impl Drop for MacroRunner {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn run_jobs(jobs: &Receiver<MacroJob>, sink: &SharedSink, stop: &AtomicBool, generation: u64) {
    while !stop.load(Ordering::SeqCst) {
        match jobs.recv_timeout(Duration::from_millis(20)) {
            Ok(job) if job.generation == generation => {
                run_owned_steps(&job.steps, sink, stop, job.owner)
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Execute one job. Central ownership retains uncertain writes and retries release on failure.
#[cfg(test)]
pub fn run_steps(steps: &[Step], sink: &SharedSink, stop: &AtomicBool) {
    run_owned_steps(steps, sink, stop, output::new_owner());
}
fn run_owned_steps(
    steps: &[Step],
    sink: &SharedSink,
    stop: &AtomicBool,
    owner: output::OutputOwner,
) {
    for step in steps {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match step {
            Step::Emit(events) => {
                if let Err(error) = output::emit_owned(sink, owner, events) {
                    eprintln!("macro output failed: {error}");
                    break;
                }
            }
            Step::Sleep(duration) => {
                let mut left = *duration;
                while !left.is_zero() && !stop.load(Ordering::SeqCst) {
                    let chunk = left.min(Duration::from_millis(10));
                    std::thread::sleep(chunk);
                    left -= chunk;
                }
            }
        }
    }
    let _ = output::release_owned(sink, owner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::output::tests::Recorder;

    fn emitted(steps: &[Step]) -> Vec<Vec<Ev>> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Emit(events) => Some(events.clone()),
                Step::Sleep(_) => None,
            })
            .collect()
    }

    #[test]
    fn combos_press_modifiers_first_and_release_in_reverse() {
        let codes = combo_codes("ctrl+shift+t").unwrap();
        assert_eq!(codes, vec![29, 42, 20]);
        let [down, up] = tap_batches(&codes);
        assert_eq!(down, vec![Ev::key(29, 1), Ev::key(42, 1), Ev::key(20, 1)]);
        assert_eq!(up, vec![Ev::key(20, 0), Ev::key(42, 0), Ev::key(29, 0)]);
        assert!(combo_codes("ctrl+nope").is_err());
    }

    #[test]
    fn us_layout_text() {
        assert_eq!(us_char('a'), Some((30, false)));
        assert_eq!(us_char('Z'), Some((44, true)));
        assert_eq!(us_char('1'), Some((2, false)));
        assert_eq!(us_char('0'), Some((11, false)));
        assert_eq!(us_char('!'), Some((2, true)));
        assert_eq!(us_char(')'), Some((11, true)));
        assert_eq!(us_char('('), Some((10, true)));
        assert_eq!(us_char('@'), Some((3, true)));
        assert_eq!(us_char('?'), Some((53, true)));
        assert_eq!(us_char('\n'), Some((28, false)));
        assert_eq!(us_char('é'), None);
        let batches = emitted(&text_steps("Hi!").unwrap());
        assert_eq!(
            batches,
            vec![
                vec![Ev::key(42, 1), Ev::key(35, 1)],
                vec![Ev::key(35, 0), Ev::key(42, 0)],
                vec![Ev::key(23, 1)],
                vec![Ev::key(23, 0)],
                vec![Ev::key(42, 1), Ev::key(2, 1)],
                vec![Ev::key(2, 0), Ev::key(42, 0)],
            ]
        );
        assert!(text_steps("naïve").is_err());
    }

    #[test]
    fn macros_expand_steps() {
        let steps = crate::config::parse_macro(
            "ctrl+c, delay:20, down:shift, text:a, up:shift, media:next",
        )
        .unwrap();
        let plan = macro_steps(&steps).unwrap();
        assert!(plan.contains(&Step::Sleep(Duration::from_millis(20))));
        assert_eq!(
            emitted(&plan),
            vec![
                vec![Ev::key(29, 1), Ev::key(46, 1)],
                vec![Ev::key(46, 0), Ev::key(29, 0)],
                vec![Ev::key(42, 1)],
                vec![Ev::key(30, 1)],
                vec![Ev::key(30, 0)],
                vec![Ev::key(42, 0)],
                vec![Ev::key(KEY_NEXTSONG, 1)],
                vec![Ev::key(KEY_NEXTSONG, 0)],
            ]
        );
    }

    #[test]
    fn action_mapping() {
        let mut macros = BTreeMap::new();
        macros.insert(
            "m".to_string(),
            Macro {
                source: "a".into(),
                steps: crate::config::parse_macro("a").unwrap(),
            },
        );
        assert_eq!(
            action_ops(
                &Action::Key {
                    combo: "ctrl+shift+t".into()
                },
                &macros
            )
            .unwrap(),
            vec![Op::Tap(vec![29, 42, 20])]
        );
        assert_eq!(
            action_ops(
                &Action::Media {
                    key: MediaKey::PlayPause
                },
                &macros
            )
            .unwrap(),
            vec![Op::Tap(vec![KEY_PLAYPAUSE])]
        );
        assert!(action_ops(&Action::Disabled, &macros).unwrap().is_empty());
        assert_eq!(
            action_ops(
                &Action::Command {
                    argv: vec!["true".into()]
                },
                &macros
            )
            .unwrap(),
            vec![Op::Run(vec!["true".into()])]
        );
        assert_eq!(
            action_ops(&Action::Brightness { delta: -5 }, &macros).unwrap(),
            vec![Op::Brightness(-5)]
        );
        let Op::Queue(steps) =
            &action_ops(&Action::Macro { name: "m".into() }, &macros).unwrap()[0]
        else {
            panic!("macro must be queued")
        };
        assert_eq!(
            emitted(steps),
            vec![vec![Ev::key(30, 1)], vec![Ev::key(30, 0)]]
        );
        assert!(matches!(
            action_ops(&Action::Text { text: "x".into() }, &macros).unwrap()[0],
            Op::Queue(_)
        ));
        assert!(
            action_ops(
                &Action::Macro {
                    name: "gone".into()
                },
                &macros
            )
            .is_err()
        );
    }

    fn dial(mode: DialMode) -> DialConfig {
        DialConfig {
            mode,
            ..DialConfig::default()
        }
    }

    #[test]
    fn dial_modes() {
        let macros = BTreeMap::new();
        let ops = |mode, event| dial_ops(&dial(mode), &macros, event).unwrap();
        use DialEvent::{Press, Rotate};
        assert_eq!(
            ops(DialMode::Volume, Rotate(2)),
            vec![Op::Tap(vec![KEY_VOLUMEUP]), Op::Tap(vec![KEY_VOLUMEUP])]
        );
        assert_eq!(
            ops(DialMode::Volume, Rotate(-1)),
            vec![Op::Tap(vec![KEY_VOLUMEDOWN])]
        );
        assert_eq!(ops(DialMode::Volume, Press), vec![Op::Tap(vec![KEY_MUTE])]);
        assert_eq!(
            ops(DialMode::Scroll, Rotate(1)),
            vec![Op::Wheel {
                ctrl: false,
                detents: -1
            }]
        );
        assert_eq!(
            ops(DialMode::Zoom, Rotate(-2)),
            vec![Op::Wheel {
                ctrl: true,
                detents: -2
            }]
        );
        assert_eq!(
            ops(DialMode::Zoom, Press),
            vec![Op::Tap(vec![KEY_LEFTCTRL, KEY_0])]
        );
        assert_eq!(
            ops(DialMode::Brightness, Rotate(3)),
            vec![Op::Brightness(30)]
        );
        assert_eq!(
            ops(DialMode::Brightness, Rotate(-1)),
            vec![Op::Brightness(-10)]
        );
        assert!(ops(DialMode::Brightness, Press).is_empty());
        assert_eq!(
            ops(DialMode::Media, Rotate(-1)),
            vec![Op::Tap(vec![KEY_PREVIOUSSONG])]
        );
        assert_eq!(
            ops(DialMode::Media, Press),
            vec![Op::Tap(vec![KEY_PLAYPAUSE])]
        );
        assert_eq!(
            ops(DialMode::Workspace, Rotate(1)),
            vec![Op::Run(vec![
                "hyprctl".into(),
                "dispatch".into(),
                "workspace".into(),
                "e+1".into()
            ])]
        );
        assert_eq!(ops(DialMode::Workspace, Rotate(-1))[0], workspace("e-1"));
        assert!(ops(DialMode::Volume, Rotate(0)).is_empty());
        assert_eq!(
            ops(DialMode::Volume, Rotate(1000)).len(),
            10,
            "detents capped"
        );
        let custom = DialConfig {
            mode: DialMode::Custom,
            clockwise: Some(Action::Key {
                combo: "alt+tab".into(),
            }),
            counter_clockwise: Some(Action::Brightness { delta: -5 }),
            press: None,
        };
        assert_eq!(
            dial_ops(&custom, &macros, Rotate(2)).unwrap(),
            vec![Op::Tap(vec![56, 15]), Op::Tap(vec![56, 15])]
        );
        assert_eq!(
            dial_ops(&custom, &macros, Rotate(-3)).unwrap(),
            vec![Op::Brightness(-15)]
        );
        assert!(dial_ops(&custom, &macros, Press).unwrap().is_empty());
    }

    #[test]
    fn runner_releases_keys_left_down() {
        let (recorder, batches) = Recorder::new();
        let sink = output::share(Box::new(recorder));
        let stop = AtomicBool::new(false);
        let steps = macro_steps(&crate::config::parse_macro("down:shift, a").unwrap()).unwrap();
        run_steps(&steps, &sink, &stop);
        let batches = batches.lock().unwrap();
        assert_eq!(batches.last().unwrap(), &vec![Ev::key(42, 0)]);
    }

    #[test]
    fn runner_thread_runs_queued_jobs_and_stops() {
        let (recorder, batches) = Recorder::new();
        let runner = MacroRunner::start(output::share(Box::new(recorder))).unwrap();
        submit(&runner.queue(), text_steps("ab").unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while batches.lock().unwrap().len() < 4 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(runner);
        assert_eq!(batches.lock().unwrap().len(), 4);
    }

    #[test]
    fn detached_commands_spawn_without_a_shell() {
        spawn_detached(&["true".to_string()]).unwrap();
        assert!(spawn_detached(&["/nonexistent/hyprdeck-test-binary".to_string()]).is_err());
        assert!(spawn_detached(&[]).is_err());
    }
}

#[cfg(test)]
mod queue_capacity_regressions {
    use super::*;
    use crate::daemon::output::tests::Recorder;
    #[test]
    fn full_macro_queue_never_blocks_normal_input_forwarding() {
        let (recorder, batches) = Recorder::new();
        let sink = output::share(Box::new(recorder));
        let runner = MacroRunner::start(sink.clone()).unwrap();
        let queue = runner.queue();
        submit(
            &queue,
            vec![
                Step::Emit(vec![Ev::key(29, 1)]),
                Step::Sleep(Duration::from_secs(2)),
            ],
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while batches.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let start = std::time::Instant::now();
        for _ in 0..32 {
            submit(&queue, vec![Step::Sleep(Duration::from_secs(2))]);
        }
        let physical = output::new_owner();
        output::emit_owned(&sink, physical, &[Ev::key(30, 1)]).unwrap();
        output::release_owned(&sink, physical).unwrap();
        assert!(start.elapsed() < Duration::from_millis(200));
        assert!(
            batches
                .lock()
                .unwrap()
                .iter()
                .any(|batch| batch.contains(&Ev::key(30, 0)))
        );
        drop(runner);
    }
}
