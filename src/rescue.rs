//! `hyprdeck-corsair rescue [--all|<id>]`: close handles 0, 1 and 2 and restore hardware mode.
//! See docs/DESIGN.md §3 rule 7.
//!
//! OWNER: foundation.

use crate::bragi::{Bragi, Hidraw, MODE_HARDWARE, PROP_MODE, Transport};
use crate::devices::{self, Keyboard};
use anyhow::{Result, bail};
use std::time::Duration;

/// How long rescue waits for another process to release the keyboard.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Rescue one device (`Some(id)`) or all discovered devices (`None`). Returns one line per device.
pub fn run(device: Option<&str>) -> anyhow::Result<Vec<String>> {
    run_with(devices::discover(), device, |keyboard| {
        let transport = Hidraw::open_keyboard(keyboard, LOCK_WAIT)?;
        rescue_with(&mut Bragi::new(transport, keyboard.model))
    })
}

fn run_with(
    keyboards: Vec<Keyboard>,
    device: Option<&str>,
    rescue: impl Fn(&Keyboard) -> Result<String>,
) -> Result<Vec<String>> {
    let targets: Vec<Keyboard> = match device {
        Some(id) => match keyboards.into_iter().find(|keyboard| keyboard.id == id) {
            Some(keyboard) => vec![keyboard],
            None => bail!("Keyboard {id} is not connected"),
        },
        None => keyboards,
    };
    if targets.is_empty() {
        return Ok(vec!["No supported Corsair keyboards found".into()]);
    }
    let mut lines = Vec::new();
    let mut failed = false;
    for keyboard in &targets {
        lines.push(match rescue(keyboard) {
            Ok(outcome) => format!("{}: {outcome}", keyboard.id),
            Err(error) => {
                failed = true;
                format!("{}: failed — {error:#}", keyboard.id)
            }
        });
    }
    if failed {
        bail!(
            "Rescue failed for one or more targets: {}",
            lines.join("; ")
        );
    }
    Ok(lines)
}

/// Close handles 0, 1 and 2, SET 03=1 (skipped when already in hardware mode) and verify GET 03 == 1.
pub(crate) fn rescue_with<T: Transport>(bragi: &mut Bragi<T>) -> Result<String> {
    let mut failures = Vec::new();
    // Each attempt is independent; a CLOSE error never suppresses mode restoration.
    for handle in [0, 1, 2] {
        if let Err(error) = bragi.close_handle(handle) {
            failures.push(format!("close {handle}: {error:#}"));
        }
    }
    let before = match bragi.mode() {
        Ok(mode) => Some(mode),
        Err(error) => {
            failures.push(format!("initial mode read: {error:#}"));
            None
        }
    };
    if before != Some(MODE_HARDWARE)
        && let Err(error) = bragi.set(PROP_MODE, u32::from(MODE_HARDWARE), 1)
    {
        failures.push(format!("restore mode: {error:#}"));
    }
    match bragi.mode() {
        Ok(MODE_HARDWARE) => {}
        Ok(mode) => failures.push(format!(
            "keyboard is still in mode {mode} — unplug and replug it"
        )),
        Err(error) => failures.push(format!("verify mode: {error:#}")),
    }
    if !failures.is_empty() {
        bail!("Rescue incomplete: {}", failures.join("; "));
    }
    Ok(match before {
        Some(MODE_HARDWARE) => "already in hardware mode".into(),
        Some(mode) => format!("hardware mode restored (was mode {mode})"),
        None => "hardware mode restored (previous mode unknown)".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bragi::tests::FakeKeyboard;
    use crate::bragi::{CMD_CLOSE, MODE_SOFTWARE, RES_K65_EFFECT};
    use crate::devices::{LinkKind, Model};
    use std::path::PathBuf;

    fn keyboard(id: &str) -> Keyboard {
        Keyboard {
            id: id.into(),
            model: Model::K65PlusWireless,
            link: LinkKind::Receiver,
            name: "Corsair K65 PLUS Wireless",
            vid: 0x1b1c,
            pid: 0x2b07,
            serial: "abc".into(),
            usb_path: PathBuf::from("/nonexistent"),
            cmd_node: PathBuf::from("/nonexistent/hidraw"),
            event_node: None,
            evdev_nodes: Vec::new(),
            frame_len: 64,
            target: 0x09,
        }
    }

    #[test]
    fn restores_software_mode_and_closes_handles() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        fake.open.insert(1, RES_K65_EFFECT);
        fake.open.insert(2, 0x0022);
        let mut bragi = Bragi::new(fake, Model::K65PlusWireless);
        assert_eq!(
            rescue_with(&mut bragi).unwrap(),
            "hardware mode restored (was mode 2)"
        );
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
        assert!(bragi.transport.open.is_empty());
        assert_eq!(bragi.transport.log[0], vec![CMD_CLOSE, 1, 0]);
        assert_eq!(bragi.transport.log[1], vec![CMD_CLOSE, 1, 1]);
        assert_eq!(bragi.transport.log[2], vec![CMD_CLOSE, 1, 2]);
    }

    #[test]
    fn hardware_mode_needs_no_write() {
        let mut bragi = Bragi::new(FakeKeyboard::new(), Model::K65PlusWireless);
        assert_eq!(rescue_with(&mut bragi).unwrap(), "already in hardware mode");
        assert!(!bragi.transport.cmds().contains(&crate::bragi::CMD_SET));
    }

    #[test]
    fn offline_and_stuck_keyboards_report_errors() {
        let mut fake = FakeKeyboard::new();
        fake.offline = true;
        let error = rescue_with(&mut Bragi::new(fake, Model::K65PlusWireless)).unwrap_err();
        assert!(error.to_string().contains("asleep"));
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        fake.stuck = true;
        assert!(rescue_with(&mut Bragi::new(fake, Model::K65PlusWireless)).is_err());
    }

    #[test]
    fn one_line_per_device() {
        let keyboards = vec![keyboard("k65plus-a"), keyboard("k65plus-b")];
        let lines = run_with(keyboards.clone(), None, |keyboard| {
            if keyboard.id == "k65plus-a" {
                Ok("ok".into())
            } else {
                bail!("busy")
            }
        })
        .unwrap_err();
        assert!(format!("{lines:#}").contains("k65plus-a: ok"));
        assert!(format!("{lines:#}").contains("k65plus-b: failed — busy"));
        let lines = run_with(keyboards.clone(), Some("k65plus-b"), |_| Ok("ok".into())).unwrap();
        assert_eq!(lines, vec!["k65plus-b: ok"]);
        assert!(run_with(keyboards, Some("k100-x"), |_| Ok("ok".into())).is_err());
        assert_eq!(
            run_with(Vec::new(), None, |_| Ok("ok".into())).unwrap(),
            vec!["No supported Corsair keyboards found"]
        );
    }
}

#[cfg(test)]
mod aggregate_cleanup_regressions {
    use super::*;
    use crate::bragi::{CMD_CLOSE, CMD_SET, MODE_SOFTWARE, tests::FakeKeyboard};
    #[test]
    fn rejected_closes_do_not_suppress_other_closes_and_mode_restore() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        fake.fail_cmd.insert(CMD_CLOSE, 7);
        let mut bragi = Bragi::new(fake, crate::devices::Model::K65PlusWireless);
        assert!(rescue_with(&mut bragi).is_err());
        assert_eq!(bragi.transport.mode, MODE_HARDWARE);
        assert_eq!(
            bragi
                .transport
                .cmds()
                .iter()
                .filter(|cmd| **cmd == CMD_CLOSE)
                .count(),
            3
        );
        assert!(bragi.transport.cmds().contains(&CMD_SET));
    }
}

#[cfg(test)]
mod timeout_cleanup_regressions {
    use super::*;
    use crate::bragi::{
        CMD_CLOSE, CMD_SET, MODE_SOFTWARE, Reply, ReplyTimeout, tests::FakeKeyboard,
    };
    struct CloseTimeout(FakeKeyboard);
    impl Transport for CloseTimeout {
        fn xfer(&mut self, payload: &[u8]) -> Result<Reply> {
            if payload[0] == CMD_CLOSE {
                self.0.log.push(payload.to_vec());
                return Err(ReplyTimeout { cmd: CMD_CLOSE }.into());
            }
            self.0.xfer(payload)
        }
    }
    #[test]
    fn every_close_timeout_still_attempts_mode_restore_and_reports_all_errors() {
        let mut fake = FakeKeyboard::new();
        fake.mode = MODE_SOFTWARE;
        let mut bragi = Bragi::new(CloseTimeout(fake), crate::devices::Model::K65PlusWireless);
        let error = format!("{:#}", rescue_with(&mut bragi).unwrap_err());
        for handle in 0..=2 {
            assert!(error.contains(&format!("close {handle}")));
        }
        assert_eq!(bragi.transport.0.mode, MODE_HARDWARE);
        assert!(bragi.transport.0.cmds().contains(&CMD_SET));
    }
}
