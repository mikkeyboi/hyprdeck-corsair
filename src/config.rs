//! Live-mode configuration (`$XDG_CONFIG_HOME/hyprdeck-corsair/live.json`), written by the plugin
//! and read by the companion daemon; saved looks (`looks.json`). See docs/DESIGN.md §6–§7.
//!
//! OWNER: foundation. Shared interface.

use crate::effects::{Effect, Rgb};
use crate::layouts::keynames;
use crate::storage;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const LIVE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveConfig {
    pub version: u32,
    /// Keyed by device id.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceLive>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceLive {
    /// Source key id (layouts key id) -> action.
    #[serde(default)]
    pub remaps: BTreeMap<String, Action>,
    #[serde(default)]
    pub macros: BTreeMap<String, Macro>,
    #[serde(default)]
    pub dial: DialConfig,
    #[serde(default)]
    pub lighting: LightingConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Action {
    /// Tap a combo, e.g. "ctrl+shift+t" (key names from layouts::keynames).
    Key {
        combo: String,
    },
    /// Type text (US layout).
    Text {
        text: String,
    },
    Macro {
        name: String,
    },
    Media {
        key: MediaKey,
    },
    /// argv executed detached (no shell).
    Command {
        argv: Vec<String>,
    },
    Disabled,
    /// Keyboard backlight step (percent, signed) — dial only.
    Brightness {
        delta: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKey {
    PlayPause,
    Next,
    Previous,
    Stop,
    Mute,
    VolumeUp,
    VolumeDown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    /// Source text in the mini-language (kept for editing).
    pub source: String,
    pub steps: Vec<MacroStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum MacroStep {
    Tap { combo: String },
    Down { key: String },
    Up { key: String },
    Text { text: String },
    Delay { ms: u32 },
    Media { key: MediaKey },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DialMode {
    /// Onboard behaviour; needs no live mode.
    #[default]
    Volume,
    Scroll,
    Zoom,
    Brightness,
    Media,
    Workspace,
    Custom,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DialConfig {
    pub mode: DialMode,
    /// Used when mode == Custom.
    #[serde(default)]
    pub clockwise: Option<Action>,
    #[serde(default)]
    pub counter_clockwise: Option<Action>,
    #[serde(default)]
    pub press: Option<Action>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LightingConfig {
    pub enabled: bool,
    #[serde(default)]
    pub base: Option<Rgb>,
    /// Zone id ("keys", "edge", "logo", "ring") -> colour.
    #[serde(default)]
    pub zones: BTreeMap<String, Rgb>,
    /// Key id -> colour (overrides base/zone).
    #[serde(default)]
    pub keys: BTreeMap<String, Rgb>,
}

/// Stable content correlation for acknowledged runtime settings. Not an authentication token.
/// Compact serde field order and BTreeMap order are deterministic; generations remain separate.
pub fn device_revision(live: &DeviceLive) -> String {
    let bytes = serde_json::to_vec(live).expect("DeviceLive consists of serializable values");
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

impl DeviceLive {
    /// Which live features this device config needs (DESIGN §7).
    pub fn needs_software_mode(&self, dial_capable: bool, lighting_capable: bool) -> bool {
        (dial_capable && self.dial.mode != DialMode::Volume)
            || (lighting_capable && self.lighting.enabled)
    }
    pub fn needs_grab(&self) -> bool {
        !self.remaps.is_empty()
    }
    /// True when nothing is configured (the entry can be dropped).
    pub fn is_empty(&self) -> bool {
        self == &DeviceLive::default()
    }
}

/// Limits enforced on live.json content.
pub const MAX_REMAPS: usize = 256;
pub const MAX_MACROS: usize = 64;
pub const MAX_MACRO_STEPS: usize = 64;
pub const MAX_DELAY_MS: u32 = 5000;
pub const MAX_TEXT: usize = 1024;
pub const MAX_NAME: usize = 64;
pub const MAX_KEY_COLORS: usize = 512;
pub const MAX_ARGV: usize = 32;
pub const MAX_ARGUMENT_BYTES: usize = 1024;
pub const MAX_DEVICES: usize = 64;

impl MediaKey {
    pub const ALL: [MediaKey; 7] = [
        MediaKey::PlayPause,
        MediaKey::Next,
        MediaKey::Previous,
        MediaKey::Stop,
        MediaKey::Mute,
        MediaKey::VolumeUp,
        MediaKey::VolumeDown,
    ];
    /// Stable value (same as serde), e.g. "play_pause".
    pub fn value(self) -> &'static str {
        match self {
            MediaKey::PlayPause => "play_pause",
            MediaKey::Next => "next",
            MediaKey::Previous => "previous",
            MediaKey::Stop => "stop",
            MediaKey::Mute => "mute",
            MediaKey::VolumeUp => "volume_up",
            MediaKey::VolumeDown => "volume_down",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            MediaKey::PlayPause => "Play/Pause",
            MediaKey::Next => "Next track",
            MediaKey::Previous => "Previous track",
            MediaKey::Stop => "Stop",
            MediaKey::Mute => "Mute",
            MediaKey::VolumeUp => "Volume up",
            MediaKey::VolumeDown => "Volume down",
        }
    }
    /// Parse a value or a common alias ("playpause", "prev", "volumeup", …).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_ascii_lowercase().replace('-', "_");
        let alias = match text.as_str() {
            "playpause" | "play" | "pause" => "play_pause",
            "next_song" | "nextsong" => "next",
            "prev" | "previous_song" | "previoussong" => "previous",
            "volumeup" | "vol_up" => "volume_up",
            "volumedown" | "vol_down" => "volume_down",
            other => other,
        };
        Self::ALL.into_iter().find(|key| key.value() == alias)
    }
}

/// Safe identifier used for device ids and layout key ids.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_NAME
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn validate_name(name: &str, what: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.len() > MAX_NAME
        || name.chars().any(char::is_control)
        || name.trim() != name
    {
        bail!("{what} must be 1–{MAX_NAME} characters without leading/trailing spaces");
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.is_empty() || text.len() > MAX_TEXT {
        bail!("Text must be 1–{MAX_TEXT} characters");
    }
    if let Some(bad) = text
        .chars()
        .find(|c| !(c.is_ascii_graphic() || *c == ' ' || *c == '\n' || *c == '\t'))
    {
        bail!("Text can only contain US-keyboard characters (found {bad:?})");
    }
    Ok(())
}

/// Validate and normalise a combo ("Ctrl + Shift + T" -> "ctrl+shift+t").
fn normalize_combo(combo: &str) -> Result<String> {
    let parts: Vec<String> = combo
        .split('+')
        .map(|part| part.trim().to_ascii_lowercase())
        .collect();
    if parts.iter().any(String::is_empty) {
        bail!("Empty key name in combo {combo:?}");
    }
    let normalized = parts.join("+");
    keynames::parse_combo(&normalized).map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(normalized)
}

fn key_name(key: &str) -> Result<String> {
    let key = key.trim().to_ascii_lowercase();
    let code = keynames::code(&key).with_context(|| format!("Unknown key {key:?}"))?;
    Ok(keynames::name(code).map_or(key, str::to_string))
}

fn parse_delay(text: &str) -> Result<u32> {
    let ms: u32 = text
        .trim()
        .parse()
        .with_context(|| format!("Delay {:?} is not a number of milliseconds", text.trim()))?;
    if ms > MAX_DELAY_MS {
        bail!("Delay {ms} ms is longer than {MAX_DELAY_MS} ms");
    }
    Ok(ms)
}

fn parse_media(text: &str) -> Result<MediaKey> {
    MediaKey::parse(text).with_context(|| {
        let known: Vec<&str> = MediaKey::ALL.iter().map(|key| key.value()).collect();
        format!(
            "Unknown media key {:?} (use {})",
            text.trim(),
            known.join(", ")
        )
    })
}

impl Action {
    fn validate(&self, macros: &BTreeMap<String, Macro>) -> Result<()> {
        match self {
            Action::Key { combo } => {
                normalize_combo(combo)?;
            }
            Action::Text { text } => validate_text(text)?,
            Action::Macro { name } => {
                if !macros.contains_key(name) {
                    bail!("Macro {name:?} does not exist");
                }
            }
            Action::Media { .. } | Action::Disabled => {}
            Action::Command { argv } => {
                if argv.is_empty()
                    || argv.len() > MAX_ARGV
                    || argv.iter().any(|arg| {
                        arg.is_empty() || arg.len() > MAX_ARGUMENT_BYTES || arg.contains('\0')
                    })
                {
                    bail!(
                        "Command needs 1–{MAX_ARGV} non-empty arguments of at most {MAX_ARGUMENT_BYTES} bytes"
                    );
                }
            }
            Action::Brightness { delta } => {
                if *delta == 0 || !(-100..=100).contains(delta) {
                    bail!("Brightness step must be between -100 and 100 and not 0");
                }
            }
        }
        Ok(())
    }
}

impl MacroStep {
    fn validate(&self) -> Result<()> {
        match self {
            MacroStep::Tap { combo } => {
                normalize_combo(combo)?;
            }
            MacroStep::Down { key } | MacroStep::Up { key } => {
                key_name(key)?;
            }
            MacroStep::Text { text } => validate_text(text)?,
            MacroStep::Delay { ms } => {
                if *ms > MAX_DELAY_MS {
                    bail!("Delay {ms} ms is longer than {MAX_DELAY_MS} ms");
                }
            }
            MacroStep::Media { .. } => {}
        }
        Ok(())
    }
}

impl LiveConfig {
    /// Structural validation (limits, references, key names). Run on load and before save.
    pub fn validate(&self) -> Result<()> {
        if self.version != LIVE_VERSION {
            bail!(
                "Unsupported live.json version {}; existing file was not overwritten",
                self.version
            );
        }
        if self.devices.len() > MAX_DEVICES {
            bail!("At most {MAX_DEVICES} keyboards in live.json");
        }
        for (device, live) in &self.devices {
            live.validate()
                .with_context(|| format!("Invalid live settings for {device}"))?;
            if !valid_id(device) {
                bail!("Invalid device id {device:?} in live.json");
            }
        }
        storage::bounded_json(self)?;
        Ok(())
    }
}

impl DeviceLive {
    pub fn validate(&self) -> Result<()> {
        if self.macros.len() > MAX_MACROS {
            bail!("At most {MAX_MACROS} macros per keyboard");
        }
        for (name, item) in &self.macros {
            validate_name(name, "Macro name")?;
            if item.steps.is_empty() || item.steps.len() > MAX_MACRO_STEPS {
                bail!("Macro {name:?} must have 1–{MAX_MACRO_STEPS} steps");
            }
            for step in &item.steps {
                step.validate().with_context(|| format!("Macro {name:?}"))?;
            }
        }
        if self.remaps.len() > MAX_REMAPS {
            bail!("At most {MAX_REMAPS} remaps per keyboard");
        }
        for (key, action) in &self.remaps {
            if !valid_id(key) {
                bail!("Invalid remap key id {key:?}");
            }
            action
                .validate(&self.macros)
                .with_context(|| format!("Remap of {key}"))?;
        }
        for (what, action) in [
            ("clockwise", &self.dial.clockwise),
            ("counter-clockwise", &self.dial.counter_clockwise),
            ("press", &self.dial.press),
        ] {
            if let Some(action) = action {
                action
                    .validate(&self.macros)
                    .with_context(|| format!("Dial {what} action"))?;
            }
        }
        if self.lighting.keys.len() > MAX_KEY_COLORS {
            bail!("At most {MAX_KEY_COLORS} per-key colours");
        }
        for key in self.lighting.keys.keys().chain(self.lighting.zones.keys()) {
            if !valid_id(key) {
                bail!("Invalid lighting key or zone id {key:?}");
            }
        }
        storage::bounded_json(self)?;
        Ok(())
    }
}

pub fn live_path() -> Result<PathBuf> {
    Ok(storage::config_dir()?.join("live.json"))
}
/// Load live.json (missing => default with LIVE_VERSION). Errors on wrong version or malformed file
/// (never silently overwrite a malformed file).
pub fn load_live() -> Result<LiveConfig> {
    load_live_at(&live_path()?)
}
/// Atomically modify live.json (storage::update_json).
pub fn update_live(update: impl FnOnce(&mut LiveConfig) -> Result<()>) -> Result<LiveConfig> {
    update_live_at(&live_path()?, update)
}

fn empty_live() -> LiveConfig {
    LiveConfig {
        version: LIVE_VERSION,
        devices: BTreeMap::new(),
    }
}

pub(crate) fn load_live_at(path: &Path) -> Result<LiveConfig> {
    let config = storage::read_json::<LiveConfig>(path)?.unwrap_or_else(empty_live);
    config.validate()?;
    Ok(config)
}

pub(crate) fn update_live_at(
    path: &Path,
    update: impl FnOnce(&mut LiveConfig) -> Result<()>,
) -> Result<LiveConfig> {
    storage::update_json(path, |current: Option<LiveConfig>| {
        let mut config = current.unwrap_or_else(empty_live);
        config.validate()?;
        update(&mut config)?;
        config.devices.retain(|_, live| !live.is_empty());
        config.validate()?;
        Ok(config)
    })
}

/// Parse the macro mini-language (DESIGN §6): "ctrl+c, delay:50, text:hi, down:shift, up:shift, media:play_pause".
/// Validates key names against layouts::keynames; max 64 steps; delay ≤ 5000 ms.
pub fn parse_macro(source: &str) -> Result<Vec<MacroStep>> {
    if source.trim().is_empty() {
        bail!("Macro has no steps");
    }
    let parts: Vec<&str> = source.split(',').collect();
    if parts.len() > MAX_MACRO_STEPS {
        bail!(
            "Macro has {} steps; the maximum is {MAX_MACRO_STEPS}",
            parts.len()
        );
    }
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            parse_step(part).with_context(|| format!("Step {} ({:?})", index + 1, part.trim()))
        })
        .collect()
}

fn parse_step(part: &str) -> Result<MacroStep> {
    let step = part.trim();
    if step.is_empty() {
        bail!("Empty step");
    }
    let (prefix, rest) = match step.split_once(':') {
        Some((prefix, rest)) => (prefix.trim().to_ascii_lowercase(), rest),
        None => (String::new(), step),
    };
    Ok(match prefix.as_str() {
        "down" => MacroStep::Down {
            key: key_name(rest)?,
        },
        "up" => MacroStep::Up {
            key: key_name(rest)?,
        },
        "text" => {
            validate_text(rest)?;
            MacroStep::Text {
                text: rest.to_string(),
            }
        }
        "delay" => MacroStep::Delay {
            ms: parse_delay(rest)?,
        },
        "media" => MacroStep::Media {
            key: parse_media(rest)?,
        },
        "" => MacroStep::Tap {
            combo: normalize_combo(step)?,
        },
        other => {
            bail!("Unknown step type {other:?} (use a combo, down:, up:, text:, delay: or media:)")
        }
    })
}

/// Parse a remap/combo action from UI form values ("disabled", "media:next", "macro:<name>", "ctrl+shift+t", "text:…").
pub fn parse_action(value: &str) -> Result<Action> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("Choose an action");
    }
    if trimmed.eq_ignore_ascii_case("disabled") {
        return Ok(Action::Disabled);
    }
    let Some((prefix, rest)) = trimmed.split_once(':') else {
        return Ok(Action::Key {
            combo: normalize_combo(trimmed)?,
        });
    };
    Ok(match prefix.trim().to_ascii_lowercase().as_str() {
        "media" => Action::Media {
            key: parse_media(rest)?,
        },
        "macro" => {
            let name = rest.trim();
            validate_name(name, "Macro name")?;
            Action::Macro {
                name: name.to_string(),
            }
        }
        "text" => {
            // Keep inner spacing of the text exactly as typed (only the outer value is trimmed).
            let text = value
                .trim_start()
                .split_once(':')
                .map_or("", |(_, text)| text);
            validate_text(text)?;
            Action::Text {
                text: text.to_string(),
            }
        }
        "command" | "cmd" => {
            // Structured argv preserves spaces, quotes, backslashes and Unicode exactly.
            // Legacy bare-word commands remain accepted; they have no shell/quoting semantics.
            let argv: Vec<String> = if rest.trim_start().starts_with('[') {
                serde_json::from_str(rest.trim())
                    .context("Command must be a JSON array of arguments")?
            } else {
                rest.split_whitespace().map(str::to_string).collect()
            };
            let action = Action::Command { argv };
            action.validate(&BTreeMap::new())?;
            action
        }
        "brightness" => {
            let delta: i32 = rest
                .trim()
                .trim_start_matches('+')
                .parse()
                .context("Brightness step must be a number")?;
            let action = Action::Brightness { delta };
            action.validate(&BTreeMap::new())?;
            action
        }
        "key" => Action::Key {
            combo: normalize_combo(rest)?,
        },
        other => bail!("Unknown action type {other:?}"),
    })
}

pub const LOOKS_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Look {
    pub effect: Effect,
    /// Brightness percent 0..=100.
    pub brightness: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Looks {
    pub version: u32,
    /// device id -> look name -> look
    #[serde(default)]
    pub devices: BTreeMap<String, BTreeMap<String, Look>>,
}

pub fn looks_path() -> Result<PathBuf> {
    Ok(storage::config_dir()?.join("looks.json"))
}

pub const MAX_LOOKS: usize = 32;

impl Looks {
    pub fn validate(&self) -> Result<()> {
        if self.version != LOOKS_VERSION {
            bail!(
                "Unsupported looks.json version {}; existing file was not overwritten",
                self.version
            );
        }
        for (device, looks) in &self.devices {
            if !valid_id(device) {
                bail!("Invalid device id {device:?} in looks.json");
            }
            if looks.len() > MAX_LOOKS {
                bail!("At most {MAX_LOOKS} saved looks per keyboard");
            }
            for (name, look) in looks {
                validate_name(name, "Look name")?;
                if look.brightness > 100 {
                    bail!("Look {name:?} has brightness above 100 %");
                }
            }
        }
        Ok(())
    }
}

fn empty_looks() -> Looks {
    Looks {
        version: LOOKS_VERSION,
        devices: BTreeMap::new(),
    }
}

pub fn load_looks() -> Result<Looks> {
    load_looks_at(&looks_path()?)
}
pub fn update_looks(update: impl FnOnce(&mut Looks) -> Result<()>) -> Result<Looks> {
    update_looks_at(&looks_path()?, update)
}

pub(crate) fn load_looks_at(path: &Path) -> Result<Looks> {
    let looks = storage::read_json::<Looks>(path)?.unwrap_or_else(empty_looks);
    looks.validate()?;
    Ok(looks)
}

pub(crate) fn update_looks_at(
    path: &Path,
    update: impl FnOnce(&mut Looks) -> Result<()>,
) -> Result<Looks> {
    storage::update_json(path, |current: Option<Looks>| {
        let mut looks = current.unwrap_or_else(empty_looks);
        looks.validate()?;
        update(&mut looks)?;
        looks.devices.retain(|_, looks| !looks.is_empty());
        looks.validate()?;
        Ok(looks)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::tests::TempDir;
    use crate::effects::{EffectKind, Speed};
    use serde_json::json;

    #[test]
    fn parses_non_key_macro_steps() {
        let steps =
            parse_macro("delay:50, text:hello world ,media:play_pause,delay: 5000").unwrap();
        assert_eq!(
            steps,
            vec![
                MacroStep::Delay { ms: 50 },
                MacroStep::Text {
                    text: "hello world".into()
                },
                MacroStep::Media {
                    key: MediaKey::PlayPause
                },
                MacroStep::Delay { ms: 5000 },
            ]
        );
    }

    #[test]
    fn macro_errors_name_the_failing_step() {
        let error = parse_macro("delay:50, delay:5001").unwrap_err();
        assert!(
            format!("{error:#}").starts_with("Step 2 (\"delay:5001\")"),
            "{error:#}"
        );
        let error = parse_macro("delay:10, media:launch").unwrap_err();
        assert!(format!("{error:#}").contains("Step 2"), "{error:#}");
        assert!(
            format!("{error:#}").contains("Unknown media key"),
            "{error:#}"
        );
        assert!(parse_macro("").is_err());
        assert!(parse_macro("delay:1,,delay:2").is_err());
        assert!(parse_macro("delay:x").is_err());
        assert!(parse_macro("text:héllo").is_err());
        assert!(parse_macro("text:").is_err());
        assert!(parse_macro("boom:1").is_err());
        let too_many = vec!["delay:1"; 65].join(",");
        assert!(
            parse_macro(&too_many)
                .unwrap_err()
                .to_string()
                .contains("maximum is 64")
        );
        assert_eq!(
            parse_macro(&vec!["delay:1"; 64].join(",")).unwrap().len(),
            64
        );
    }

    #[test]
    fn parses_key_macro_steps() {
        let steps = parse_macro("Ctrl + Shift + T, down:shift, up:shift").unwrap();
        assert_eq!(
            steps[0],
            MacroStep::Tap {
                combo: "ctrl+shift+t".into()
            }
        );
        assert!(matches!(&steps[1], MacroStep::Down { .. }));
        assert!(matches!(&steps[2], MacroStep::Up { .. }));
        let error = parse_macro("ctrl+c, ctrl+nope").unwrap_err();
        assert!(format!("{error:#}").starts_with("Step 2"), "{error:#}");
        assert!(parse_macro("down:nope").is_err());
        assert_eq!(
            parse_action("ctrl+shift+t").unwrap(),
            Action::Key {
                combo: "ctrl+shift+t".into()
            }
        );
        assert!(parse_action("ctrl+").is_err());
    }

    #[test]
    fn parses_actions() {
        assert_eq!(parse_action("disabled").unwrap(), Action::Disabled);
        assert_eq!(parse_action(" Disabled ").unwrap(), Action::Disabled);
        assert_eq!(
            parse_action("media:next").unwrap(),
            Action::Media {
                key: MediaKey::Next
            }
        );
        assert_eq!(
            parse_action("media:playpause").unwrap(),
            Action::Media {
                key: MediaKey::PlayPause
            }
        );
        assert_eq!(
            parse_action("macro:Copy all").unwrap(),
            Action::Macro {
                name: "Copy all".into()
            }
        );
        assert_eq!(
            parse_action("text:a, b").unwrap(),
            Action::Text {
                text: "a, b".into()
            }
        );
        assert_eq!(
            parse_action("command:hyprctl dispatch workspace e+1").unwrap(),
            Action::Command {
                argv: vec![
                    "hyprctl".into(),
                    "dispatch".into(),
                    "workspace".into(),
                    "e+1".into()
                ]
            }
        );
        assert_eq!(
            parse_action("brightness:+10").unwrap(),
            Action::Brightness { delta: 10 }
        );
        assert_eq!(
            parse_action("brightness:-5").unwrap(),
            Action::Brightness { delta: -5 }
        );
        for bad in [
            "",
            "media:",
            "macro:",
            "command:",
            "brightness:0",
            "brightness:500",
            "teleport:x",
        ] {
            assert!(parse_action(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn serde_formats_are_stable() {
        assert_eq!(
            serde_json::to_value(Action::Disabled).unwrap(),
            json!({"type": "disabled"})
        );
        assert_eq!(
            serde_json::to_value(Action::Media {
                key: MediaKey::VolumeUp
            })
            .unwrap(),
            json!({"type": "media", "key": "volume_up"})
        );
        assert_eq!(
            serde_json::to_value(MacroStep::Delay { ms: 5 }).unwrap(),
            json!({"type": "delay", "ms": 5})
        );
        assert_eq!(
            serde_json::to_value(DialMode::Workspace).unwrap(),
            json!("workspace")
        );
        let parsed: LiveConfig =
            serde_json::from_value(json!({"version": 1, "devices": {"k65plus-a": {}}})).unwrap();
        assert_eq!(parsed.devices["k65plus-a"], DeviceLive::default());
        for media in MediaKey::ALL {
            assert_eq!(serde_json::to_value(media).unwrap(), json!(media.value()));
            assert_eq!(MediaKey::parse(media.value()), Some(media));
        }
    }

    #[test]
    fn feature_needs() {
        let mut live = DeviceLive::default();
        assert!(!live.needs_software_mode(true, true));
        assert!(!live.needs_grab());
        live.dial.mode = DialMode::Scroll;
        assert!(live.needs_software_mode(true, false));
        assert!(!live.needs_software_mode(false, true));
        live.dial.mode = DialMode::Volume;
        live.lighting.enabled = true;
        assert!(live.needs_software_mode(false, true));
        assert!(!live.needs_software_mode(true, false));
        live.remaps.insert("caps".into(), Action::Disabled);
        assert!(live.needs_grab());
    }

    #[test]
    fn live_config_round_trips_and_validates() {
        let tmp = TempDir::new("config-live");
        let path = tmp.0.join("live.json");
        assert_eq!(
            load_live_at(&path).unwrap(),
            LiveConfig {
                version: LIVE_VERSION,
                devices: BTreeMap::new()
            }
        );
        let saved = update_live_at(&path, |config| {
            let live = config.devices.entry("k65plus-abc".into()).or_default();
            live.macros.insert(
                "Hello".into(),
                Macro {
                    source: "text:hi, delay:10".into(),
                    steps: parse_macro("text:hi, delay:10")?,
                },
            );
            live.remaps.insert(
                "caps".into(),
                Action::Macro {
                    name: "Hello".into(),
                },
            );
            live.dial.mode = DialMode::Custom;
            live.dial.press = Some(Action::Media {
                key: MediaKey::Mute,
            });
            live.lighting.base = Some(Rgb(1, 2, 3));
            Ok(())
        })
        .unwrap();
        assert_eq!(load_live_at(&path).unwrap(), saved);
        // Referencing a missing macro is rejected and the file is left as it was.
        let error = update_live_at(&path, |config| {
            config
                .devices
                .get_mut("k65plus-abc")
                .unwrap()
                .macros
                .clear();
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("does not exist"), "{error:#}");
        assert_eq!(load_live_at(&path).unwrap(), saved);
        // Empty device entries are dropped.
        let cleared = update_live_at(&path, |config| {
            config
                .devices
                .insert("k65plus-abc".into(), DeviceLive::default());
            Ok(())
        })
        .unwrap();
        assert!(cleared.devices.is_empty());
        // Wrong version and malformed files are errors and never overwritten.
        std::fs::write(&path, br#"{"version": 2, "devices": {}}"#).unwrap();
        assert!(load_live_at(&path).is_err());
        assert!(update_live_at(&path, |_| Ok(())).is_err());
        std::fs::write(&path, b"{nope").unwrap();
        assert!(load_live_at(&path).is_err());
        assert!(update_live_at(&path, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{nope");
    }

    #[test]
    fn invalid_live_content_is_rejected() {
        let mut live = DeviceLive::default();
        live.macros.insert(
            " bad".into(),
            Macro {
                source: "delay:1".into(),
                steps: vec![MacroStep::Delay { ms: 1 }],
            },
        );
        assert!(live.validate().is_err());
        let mut live = DeviceLive::default();
        live.macros.insert(
            "ok".into(),
            Macro {
                source: String::new(),
                steps: vec![],
            },
        );
        assert!(live.validate().is_err());
        let mut live = DeviceLive::default();
        live.macros.insert(
            "ok".into(),
            Macro {
                source: String::new(),
                steps: vec![MacroStep::Delay { ms: 9000 }],
            },
        );
        assert!(live.validate().is_err());
        let mut live = DeviceLive::default();
        live.remaps.insert("Bad Key".into(), Action::Disabled);
        assert!(live.validate().is_err());
        let mut live = DeviceLive::default();
        live.dial.clockwise = Some(Action::Command { argv: vec![] });
        assert!(live.validate().is_err());
        let mut config = LiveConfig {
            version: LIVE_VERSION,
            devices: BTreeMap::new(),
        };
        config
            .devices
            .insert("Bad/Id".into(), DeviceLive::default());
        assert!(config.validate().is_err());
    }

    #[test]
    fn looks_round_trip() {
        let tmp = TempDir::new("config-looks");
        let path = tmp.0.join("looks.json");
        assert!(load_looks_at(&path).unwrap().devices.is_empty());
        let look = Look {
            effect: Effect {
                kind: EffectKind::Rain,
                speed: Some(Speed::Slow),
                color: None,
            },
            brightness: 70,
        };
        let saved = update_looks_at(&path, |looks| {
            looks
                .devices
                .entry("k65plus-abc".into())
                .or_default()
                .insert("Evening".into(), look.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(load_looks_at(&path).unwrap(), saved);
        assert_eq!(saved.devices["k65plus-abc"]["Evening"], look);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            raw["devices"]["k65plus-abc"]["Evening"]["effect"]["kind"],
            json!("rain")
        );
        assert!(
            update_looks_at(&path, |looks| {
                looks
                    .devices
                    .get_mut("k65plus-abc")
                    .unwrap()
                    .get_mut("Evening")
                    .unwrap()
                    .brightness = 101;
                Ok(())
            })
            .is_err()
        );
        let removed = update_looks_at(&path, |looks| {
            looks
                .devices
                .get_mut("k65plus-abc")
                .unwrap()
                .remove("Evening");
            Ok(())
        })
        .unwrap();
        assert!(removed.devices.is_empty());
    }
}

#[cfg(test)]
mod aggregate_regressions {
    use super::*;
    use crate::devices::tests::TempDir;

    #[test]
    fn eight_large_macros_cannot_make_live_file_unreadable() {
        let dir = TempDir::new("large-live-regression");
        let path = dir.0.join("live.json");
        let mut accepted = 0;
        for n in 0..32 {
            let result = update_live_at(&path, |config| {
                config
                    .devices
                    .entry("k65-test".into())
                    .or_default()
                    .macros
                    .insert(
                        format!("macro-{n}"),
                        Macro {
                            source: "x".into(),
                            steps: vec![
                                MacroStep::Text {
                                    text: "x".repeat(MAX_TEXT)
                                };
                                MAX_MACRO_STEPS
                            ],
                        },
                    );
                Ok(())
            });
            if result.is_ok() {
                accepted += 1;
                assert!(std::fs::metadata(&path).unwrap().len() <= storage::MAX_JSON);
                load_live_at(&path).unwrap();
            }
        }
        assert!(accepted > 0 && accepted < 32);
        update_live_at(&path, |config| {
            config.devices.clear();
            Ok(())
        })
        .unwrap();
        assert!(load_live_at(&path).unwrap().devices.is_empty());
    }

    #[test]
    fn validation_rejects_aggregate_size_across_devices() {
        let mut config = empty_live();
        for n in 0..8 {
            config
                .devices
                .entry(format!("k65-{n}"))
                .or_default()
                .macros
                .insert(
                    "large".into(),
                    Macro {
                        source: "x".into(),
                        steps: vec![
                            MacroStep::Text {
                                text: "\t".repeat(MAX_TEXT)
                            };
                            MAX_MACRO_STEPS
                        ],
                    },
                );
        }
        assert!(config.validate().is_err());
    }
}

#[cfg(test)]
mod safety_regressions {
    use super::*;
    use crate::devices::tests::TempDir;

    #[test]
    fn aggregate_macro_growth_never_makes_successful_save_unreadable() {
        let tmp = TempDir::new("aggregate-live");
        let path = tmp.0.join("live.json");
        let mut rejected = false;
        for index in 0..32 {
            let result = update_live_at(&path, |config| {
                config
                    .devices
                    .entry("k65plus-size".into())
                    .or_default()
                    .macros
                    .insert(
                        format!("macro-{index}"),
                        Macro {
                            source: "text:x".into(),
                            steps: vec![
                                MacroStep::Text {
                                    text: "x".repeat(MAX_TEXT)
                                };
                                MAX_MACRO_STEPS
                            ],
                        },
                    );
                Ok(())
            });
            if result.is_err() {
                rejected = true;
                break;
            }
            assert!(load_live_at(&path).is_ok(), "successful save is unreadable");
        }
        assert!(rejected, "aggregate oversized configuration was accepted");
        update_live_at(&path, |config| {
            config.devices.clear();
            Ok(())
        })
        .unwrap();
        assert!(load_live_at(&path).unwrap().devices.is_empty());
    }

    #[test]
    fn aggregate_validation_bounds_command_arguments_and_devices() {
        let mut live = LiveConfig {
            version: LIVE_VERSION,
            ..LiveConfig::default()
        };
        live.devices
            .entry("k65plus-argv".into())
            .or_default()
            .dial
            .press = Some(Action::Command {
            argv: vec!["x".repeat(storage::MAX_JSON as usize)],
        });
        assert!(live.validate().is_err());
    }
}
