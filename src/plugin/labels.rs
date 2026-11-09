//! Display labels, option lists and id mappings shared by rendering, actions and the visualization.

use crate::config::{Action, DeviceLive};
use crate::devices::{Model, PhysicalLayout};
use crate::layouts::{self, Layout, keynames};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::model::option;

/// Local wall-clock "HH:MM:SS" for a UNIX timestamp.
pub fn clock(secs: u64) -> String {
    let time = libc::time_t::try_from(secs).unwrap_or(0);
    // SAFETY: zeroed tm is a valid out-parameter; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return format!("{secs}");
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Host ids must start with a lowercase letter; layout ids such as "1" get a "key-" prefix.
pub fn viz_id(layout_id: &str) -> String {
    if layout_id
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_lowercase)
    {
        layout_id.to_string()
    } else {
        format!("key-{layout_id}")
    }
}

/// Inverse of [`viz_id`]. No layout id itself starts with "key-".
pub fn layout_id(viz_id: &str) -> &str {
    viz_id.strip_prefix("key-").unwrap_or(viz_id)
}

/// Keys that have a Linux keycode and can therefore be remapped, with unambiguous labels.
pub fn remappable_keys(layout: &Layout) -> Vec<(String, String)> {
    let keys: Vec<_> = layout
        .keys
        .iter()
        .filter(|key| key.evdev.is_some())
        .collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for key in &keys {
        *counts.entry(key.label).or_default() += 1;
    }
    keys.iter()
        .map(|key| {
            let label = if counts[key.label] > 1 {
                format!("{} ({})", key.label, key.id)
            } else {
                key.label.to_string()
            };
            (key.id.to_string(), label)
        })
        .collect()
}

/// Whether `key` is a remappable key on either physical layout of `model`.
pub fn is_remappable(model: Model, key: &str) -> bool {
    [PhysicalLayout::Ansi, PhysicalLayout::Iso]
        .iter()
        .any(|physical| {
            layouts::key_by_id(layouts::layout(model, *physical), key)
                .is_some_and(|key| key.evdev.is_some())
        })
}

/// Whether `key` exists on either physical layout of `model`.
pub fn is_layout_key(model: Model, key: &str) -> bool {
    [PhysicalLayout::Ansi, PhysicalLayout::Iso]
        .iter()
        .any(|physical| layouts::key_by_id(layouts::layout(model, *physical), key).is_some())
}

/// Whether a control has a physical LED on either supported layout.
pub fn is_paintable(model: Model, key: &str) -> bool {
    [PhysicalLayout::Ansi, PhysicalLayout::Iso]
        .iter()
        .any(|physical| {
            layouts::key_by_id(layouts::layout(model, *physical), key)
                .is_some_and(|key| key.led.is_some())
        })
}

/// Stable option value for an action; `config::parse_action` reads it back.
pub fn action_value(action: &Action) -> String {
    match action {
        Action::Key { combo } => combo.clone(),
        Action::Text { text } => format!("text:{text}"),
        Action::Macro { name } => format!("macro:{name}"),
        Action::Media { key } => format!("media:{}", key.value()),
        Action::Command { argv } => format!(
            "command:{}",
            serde_json::to_string(argv).expect("string arrays serialize")
        ),
        Action::Disabled => "disabled".into(),
        Action::Brightness { delta } => format!("brightness:{delta:+}"),
    }
}

fn key_label(name: &str) -> String {
    keynames::code(name)
        .and_then(|code| keynames::all().iter().find(|entry| entry.2 == code))
        .map_or_else(|| name.to_string(), |entry| entry.1.to_string())
}

/// Short human label for an action.
pub fn action_label(action: &Action) -> String {
    match action {
        Action::Key { combo } => combo
            .split('+')
            .map(key_label)
            .collect::<Vec<_>>()
            .join("+"),
        Action::Text { text } => {
            let short: String = text.chars().take(24).collect();
            if short.len() < text.len() {
                format!("Type \"{short}…\"")
            } else {
                format!("Type \"{short}\"")
            }
        }
        Action::Macro { name } => format!("Macro: {name}"),
        Action::Media { key } => key.label().to_string(),
        Action::Command { argv } => format!("Run {}", argv.first().map_or("", String::as_str)),
        Action::Disabled => "Disabled".into(),
        Action::Brightness { delta } => format!("Brightness {delta:+} %"),
    }
}

/// Remap targets: "default" (optional), disabled, saved macros, every key name, then any current
/// targets not otherwise listed (e.g. combos). ≤ 1 + 1 + 64 + 256 + 256 options.
pub fn target_options(live: &DeviceLive, with_default: bool) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut options = Vec::new();
    let mut push = |value: String, label: String, options: &mut Vec<Value>| {
        if seen.insert(value.clone()) {
            options.push(option(value, label));
        }
    };
    if with_default {
        push("default".into(), "Default (no remap)".into(), &mut options);
    }
    push(
        "disabled".into(),
        "Disabled (does nothing)".into(),
        &mut options,
    );
    for name in live.macros.keys() {
        push(
            format!("macro:{name}"),
            format!("Macro: {name}"),
            &mut options,
        );
    }
    for (name, label, _) in keynames::all() {
        push((*name).to_string(), (*label).to_string(), &mut options);
    }
    for action in live.remaps.values() {
        push(
            action_value(action),
            format!("{} (current)", action_label(action)),
            &mut options,
        );
    }
    options
}

#[cfg(test)]
mod regressions {
    use super::*;

    #[test]
    fn command_values_preserve_exact_argv_without_shell_execution() {
        for argv in [
            vec!["printf", "hello world"],
            vec![
                "program",
                "a\"b",
                "it's",
                "a\\b",
                "$(not-a-shell)",
                "雪",
                " [literal] ",
            ],
        ] {
            let action = Action::Command {
                argv: argv.into_iter().map(str::to_owned).collect(),
            };
            assert_eq!(
                crate::config::parse_action(&action_value(&action)).unwrap(),
                action
            );
        }
    }
}

#[cfg(test)]
mod safety_regressions {
    use super::*;
    #[test]
    fn command_argv_roundtrip_preserves_boundaries_and_quotes() {
        let action = Action::Command {
            argv: vec![
                "/path with space/tool".into(),
                "hello world".into(),
                "a\"b'c\\d".into(),
                "[literal]".into(),
            ],
        };
        assert_eq!(
            crate::config::parse_action(&action_value(&action)).unwrap(),
            action
        );
    }
}
