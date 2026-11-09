//! Action parsing (pure, before any I/O) and execution (DESIGN §6 action list). Every persistent
//! keyboard write reads first, skips identical values and is counted by the write budget.

use super::backend::{Backend, Session, session};
use super::labels;
use super::render::{SLEEP_MINUTES, ZONE_KEYS};
use super::service;
use super::snapshot::{self, owned_by_daemon};
use crate::bragi::{
    self, PROP_BRIGHTNESS, PROP_BRIGHTNESS_2, PROP_LOCK_ENABLE, PROP_LOCK_MASK, PROP_MAX_POLLING,
    PROP_POLLING, PROP_SLEEP_MS, RES_K65_EFFECT,
};
use crate::config::{self, Action, DeviceLive, DialConfig, DialMode, Look, Macro};
use crate::devices::{Keyboard, Model};
use crate::effects::{self, Decoded, Effect, EffectKind, Rgb, Speed};
use crate::layouts;
use crate::storage::{ValueCache, WriteBudget};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

/// A validated action. Produced by [`parse`] without touching any device or file.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    SetEffect(EffectKind),
    SetSpeed(Speed),
    SetColor(Rgb),
    /// Percent 0..=100.
    SetBrightness(u32),
    RereadEffect,
    SaveLook(String),
    ApplyLook(String),
    DeleteLook(String),
    SetWinLock(bool),
    SetLockMask {
        bit: u8,
        value: bool,
    },
    /// Minutes.
    SetSleep(u32),
    /// Hz.
    SetPolling(u32),
    AddRemap {
        key: String,
        action: Action,
    },
    RemoveRemap(String),
    SaveMacro {
        name: String,
        source: String,
    },
    DeleteMacro(String),
    SetDial(DialConfig),
    /// Choice-only change: keep custom assignments even when temporarily leaving Custom.
    SetDialMode(DialMode),
    SetLiveLighting(bool),
    SetBaseColor(Rgb),
    SetZoneColor {
        zone: String,
        color: Rgb,
    },
    SetKeyColors {
        keys: Vec<String>,
        color: Rgb,
    },
    /// `None` target = back to default.
    RemapKey {
        key: String,
        target: Option<Action>,
    },
    ClearKeyColors,
    EnableLiveMode,
    DisableLiveMode,
    RestartLiveMode,
    Rescue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub device: Option<String>,
    pub act: Act,
}

/// Every action name the plugin accepts (DESIGN §6).
pub const ACTIONS: [&str; 28] = [
    "set-effect",
    "set-speed",
    "set-color",
    "set-brightness",
    "brightness-preset",
    "reread-effect",
    "save-look",
    "apply-look",
    "delete-look",
    "set-win-lock",
    "set-lock-mask",
    "set-sleep",
    "set-polling",
    "add-remap",
    "remove-remap",
    "save-macro",
    "delete-macro",
    "set-dial",
    "set-live-lighting",
    "set-base-color",
    "set-zone-color",
    "set-key-colors",
    "remap-key",
    "clear-key-colors",
    "enable-live-mode",
    "disable-live-mode",
    "restart-live-mode",
    "rescue",
];

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// `args.value.<name>` when value is an object, else `args.<name>`.
fn field<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    if let Some(Value::Object(value)) = args.get("value")
        && let Some(found) = value.get(name)
    {
        return Some(found);
    }
    args.get(name)
}

/// A string from `args.value` (when it is a string) or any of `names`.
fn text_arg(args: &Map<String, Value>, names: &[&str]) -> Result<String> {
    if let Some(Value::String(value)) = args.get("value") {
        return Ok(value.clone());
    }
    for name in names {
        match field(args, name) {
            Some(Value::String(value)) => return Ok(value.clone()),
            Some(_) => bail!("args.{name} must be a string"),
            None => {}
        }
    }
    bail!("Missing args.{}", names.first().copied().unwrap_or("value"))
}

fn bool_arg(args: &Map<String, Value>) -> Result<bool> {
    args.get("value")
        .and_then(Value::as_bool)
        .context("args.value must be true or false")
}

/// Integer from a JSON number or numeric string.
fn int_arg(value: Option<&Value>, what: &str) -> Result<i64> {
    let number = match value {
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|n| n.is_finite())
            .with_context(|| format!("{what} must be a finite number"))?,
        Some(Value::String(text)) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .with_context(|| format!("{what} must be a number"))?,
        _ => bail!("{what} is missing"),
    };
    if number.abs() > 1e9 {
        bail!("{what} is out of range");
    }
    Ok(number.round() as i64)
}

fn color_arg(args: &Map<String, Value>) -> Result<Rgb> {
    let text = text_arg(args, &["color"])?;
    Rgb::parse(&text).with_context(|| format!("{text:?} is not a #RRGGBB colour"))
}

fn name_arg(text: &str, what: &str) -> Result<String> {
    let name = text.trim();
    if name.is_empty() || name.len() > config::MAX_NAME || name.chars().any(char::is_control) {
        bail!("{what} must be 1–{} characters", config::MAX_NAME);
    }
    Ok(name.to_string())
}

fn key_arg(text: &str) -> Result<String> {
    let key = labels::layout_id(text.trim()).to_string();
    if key.is_empty()
        || key.len() > 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        bail!("Invalid key id {text:?}");
    }
    Ok(key)
}

/// Optional action text: empty means "not set".
fn optional_action(value: Option<&Value>, what: &str) -> Result<Option<Action>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => config::parse_action(text)
            .map(Some)
            .with_context(|| what.to_string()),
        Some(_) => bail!("{what} must be text"),
    }
}

fn dial_mode(text: &str) -> Result<DialMode> {
    super::render::DIAL_MODES
        .into_iter()
        .find(|mode| super::render::dial_value(*mode) == text.trim())
        .with_context(|| format!("Unknown dial mode {text:?}"))
}

/// Validate an action request completely before any I/O.
pub fn parse(action: &str, args: &Map<String, Value>) -> Result<Parsed> {
    if !ACTIONS.contains(&action) {
        bail!("Unknown action {action:?}");
    }
    let service_action = matches!(
        action,
        "enable-live-mode" | "disable-live-mode" | "restart-live-mode"
    );
    let device = match args.get("device") {
        Some(Value::String(device)) if valid_id(device) => Some(device.clone()),
        Some(_) => bail!("args.device must be a keyboard id from the state"),
        None if service_action => None,
        None => bail!("args.device is required"),
    };
    let act = match action {
        "set-effect" => {
            let value = text_arg(args, &["effect"])?;
            Act::SetEffect(
                EffectKind::from_value(value.trim())
                    .with_context(|| format!("Choose an effect (got {value:?})"))?,
            )
        }
        "set-speed" => {
            let value = text_arg(args, &["speed"])?;
            Act::SetSpeed(
                Speed::from_value(value.trim()).with_context(|| {
                    format!("Speed must be slow, medium or fast (got {value:?})")
                })?,
            )
        }
        "set-color" => Act::SetColor(color_arg(args)?),
        "set-brightness" => {
            let value = int_arg(args.get("value"), "Brightness")?;
            if !(0..=100).contains(&value) {
                bail!("Brightness must be 0–100 %");
            }
            Act::SetBrightness(value as u32)
        }
        "brightness-preset" => match args.get("value") {
            Some(Value::String(text)) if text == "off" => Act::SetBrightness(0),
            Some(Value::String(text)) if text == "full" => Act::SetBrightness(100),
            other => {
                let value = int_arg(other, "Brightness preset")?;
                if !(0..=100).contains(&value) {
                    bail!("Brightness preset must be off, full or 0–100");
                }
                Act::SetBrightness(value as u32)
            }
        },
        "reread-effect" => Act::RereadEffect,
        "save-look" => Act::SaveLook(name_arg(&text_arg(args, &["name"])?, "Look name")?),
        "apply-look" => Act::ApplyLook(name_arg(&text_arg(args, &["look", "name"])?, "Look")?),
        "delete-look" => Act::DeleteLook(name_arg(&text_arg(args, &["look", "name"])?, "Look")?),
        "set-win-lock" => Act::SetWinLock(bool_arg(args)?),
        "set-lock-mask" => {
            let bit = int_arg(args.get("bit"), "args.bit")?;
            if !(0..=3).contains(&bit) {
                bail!("args.bit must be 0–3");
            }
            Act::SetLockMask {
                bit: bit as u8,
                value: bool_arg(args)?,
            }
        }
        "set-sleep" => {
            if args.get("value").and_then(Value::as_str) == Some("unset") {
                bail!("Choose a sleep time");
            }
            let minutes = int_arg(args.get("value"), "Sleep time")?;
            match u32::try_from(minutes) {
                Ok(minutes) if SLEEP_MINUTES.contains(&minutes) => Act::SetSleep(minutes),
                _ => bail!("Sleep time must be one of 1, 5, 10, 15, 30 or 60 minutes"),
            }
        }
        "set-polling" => {
            let hz = int_arg(args.get("value"), "Polling rate")?;
            match u32::try_from(hz)
                .ok()
                .filter(|hz| bragi::polling_code(*hz).is_some())
            {
                Some(hz) => Act::SetPolling(hz),
                None => bail!("Unsupported polling rate {hz} Hz"),
            }
        }
        "add-remap" => {
            let key = key_arg(
                field(args, "key")
                    .and_then(Value::as_str)
                    .context("Choose a key")?,
            )?;
            let combo = field(args, "combo").and_then(Value::as_str).unwrap_or("");
            let action = if combo.trim().is_empty() {
                let value = field(args, "action")
                    .and_then(Value::as_str)
                    .context("Choose what the key does")?;
                config::parse_action(value)?
            } else {
                config::parse_action(combo).context("Combo")?
            };
            Act::AddRemap { key, action }
        }
        "remove-remap" => Act::RemoveRemap(key_arg(&text_arg(args, &["key"])?)?),
        "save-macro" => {
            let name = name_arg(
                field(args, "name").and_then(Value::as_str).unwrap_or(""),
                "Macro name",
            )?;
            let source = field(args, "steps")
                .and_then(Value::as_str)
                .context("Enter the macro steps")?
                .trim()
                .to_string();
            config::parse_macro(&source)?;
            Act::SaveMacro { name, source }
        }
        "delete-macro" => Act::DeleteMacro(name_arg(&text_arg(args, &["name"])?, "Macro name")?),
        "set-dial" => {
            let mode = match (args.get("value"), args.get("mode")) {
                (Some(Value::String(mode)), _) => dial_mode(mode)?,
                (Some(Value::Object(value)), _) if value.get("mode").is_some() => dial_mode(
                    value
                        .get("mode")
                        .and_then(Value::as_str)
                        .context("Dial mode must be text")?,
                )?,
                (_, Some(Value::String(mode))) => dial_mode(mode)?,
                _ => bail!("Choose what the dial does"),
            };
            let fields = ["clockwise", "counter-clockwise", "press"];
            let present = fields
                .iter()
                .filter(|name| field(args, name).is_some())
                .count();
            if present == 0 {
                Act::SetDialMode(mode)
            } else {
                // A custom form is a complete explicit edit. Reject partial forms rather than
                // silently erasing fields which were not submitted.
                if mode != DialMode::Custom || present != fields.len() {
                    bail!(
                        "Submit all three custom dial fields; use a mode choice to keep saved actions"
                    );
                }
                Act::SetDial(DialConfig {
                    mode,
                    clockwise: optional_action(field(args, "clockwise"), "Turn right")?,
                    counter_clockwise: optional_action(
                        field(args, "counter-clockwise"),
                        "Turn left",
                    )?,
                    press: optional_action(field(args, "press"), "Press")?,
                })
            }
        }
        "set-live-lighting" => Act::SetLiveLighting(bool_arg(args)?),
        "set-base-color" => Act::SetBaseColor(color_arg(args)?),
        "set-zone-color" => {
            let zone = args
                .get("zone")
                .and_then(Value::as_str)
                .context("args.zone is required")?;
            if !valid_id(zone) {
                bail!("Invalid zone {zone:?}");
            }
            Act::SetZoneColor {
                zone: zone.to_string(),
                color: color_arg(args)?,
            }
        }
        "set-key-colors" => {
            let keys = field(args, "keys")
                .and_then(Value::as_array)
                .context("Select at least one key")?;
            if keys.is_empty() || keys.len() > config::MAX_KEY_COLORS {
                bail!("Select 1–{} keys", config::MAX_KEY_COLORS);
            }
            let keys = keys
                .iter()
                .map(|key| key_arg(key.as_str().context("Key ids must be text")?))
                .collect::<Result<Vec<_>>>()?;
            let color = field(args, "color")
                .and_then(Value::as_str)
                .and_then(Rgb::parse)
                .context("Choose a #RRGGBB colour")?;
            Act::SetKeyColors { keys, color }
        }
        "remap-key" => {
            let key = key_arg(
                field(args, "key")
                    .and_then(Value::as_str)
                    .context("Select one key")?,
            )?;
            let target = field(args, "target")
                .and_then(Value::as_str)
                .context("Choose a target")?;
            let target = if target == "default" {
                None
            } else {
                Some(config::parse_action(target)?)
            };
            Act::RemapKey { key, target }
        }
        "clear-key-colors" => Act::ClearKeyColors,
        "enable-live-mode" => Act::EnableLiveMode,
        "disable-live-mode" => Act::DisableLiveMode,
        "restart-live-mode" => Act::RestartLiveMode,
        "rescue" => Act::Rescue,
        _ => unreachable!("checked against ACTIONS"),
    };
    Ok(Parsed { device, act })
}

/// Run a parsed action. The caller renders the full state afterwards.
pub fn execute(backend: &dyn Backend, parsed: &Parsed, now: u64) -> Result<()> {
    match &parsed.act {
        Act::EnableLiveMode => return service::enable(backend),
        Act::DisableLiveMode => return service::disable(backend),
        Act::RestartLiveMode => return service::restart(backend),
        _ => {}
    }
    let device = parsed
        .device
        .as_deref()
        .context("args.device is required")?;
    let keyboard = backend
        .keyboards()
        .into_iter()
        .find(|keyboard| keyboard.id == device)
        .with_context(|| format!("Keyboard {device} is not connected — plug it in or refresh"))?;
    let caps = keyboard.model.capabilities(keyboard.link);
    let needs_effects = matches!(
        parsed.act,
        Act::SetEffect(_)
            | Act::SetSpeed(_)
            | Act::SetColor(_)
            | Act::RereadEffect
            | Act::SaveLook(_)
            | Act::ApplyLook(_)
    );
    if needs_effects && !caps.onboard_effects {
        bail!("Onboard effects on this keyboard are set in iCUE; use Per-key lighting instead");
    }
    match &parsed.act {
        Act::SetSleep(_) if !caps.sleep_timeout => bail!("This keyboard has no sleep timer"),
        Act::SetPolling(_) if caps.polling_rates.is_empty() => {
            bail!("This keyboard's polling rate is fixed")
        }
        Act::SetWinLock(_) | Act::SetLockMask { .. } if !caps.win_lock => {
            bail!("This keyboard has no Win lock")
        }
        Act::SetLiveLighting(_)
        | Act::SetBaseColor(_)
        | Act::SetZoneColor { .. }
        | Act::SetKeyColors { .. }
        | Act::ClearKeyColors
            if !caps.live_lighting =>
        {
            bail!("Per-key lighting needs the keyboard connected by USB cable")
        }
        Act::SetDial(_) | Act::SetDialMode(_) if keyboard.model != Model::K65PlusWireless => {
            bail!("Dial modes are only available on the K65 PLUS")
        }
        _ => {}
    }
    match &parsed.act {
        Act::DeleteLook(name) => return delete_look(&keyboard.id, name),
        act if is_live_config(act) => return live_action(backend, &keyboard, act),
        _ => {}
    }
    let daemon = backend.daemon_hello().ok().flatten();
    let via_daemon = owned_by_daemon(daemon.as_deref(), &keyboard);
    if parsed.act == Act::Rescue && via_daemon {
        bail!(
            "Hyprdeck is driving this keyboard for the dial or per-key lighting — switch that \
             feature off first (hardware mode returns by itself)"
        );
    }
    let mut bragi = session(backend, &keyboard, via_daemon)?;
    let result = hardware_action(&mut bragi, &keyboard, &parsed.act, now, via_daemon);
    match (&parsed.act, result) {
        // Hardware-verified on the K100 RGB: the keyboard resets itself on USB to apply a new
        // polling rate, so the session dies mid-action. That is the change taking effect.
        (Act::SetPolling(hz), Err(error))
            if bragi::is_terminal_disconnect(&error) || bragi::is_timeout(&error) =>
        {
            drop(bragi);
            confirm_polling_after_reset(backend, &keyboard.id, *hz)
        }
        (_, result) => result,
    }
}

/// How long a keyboard may take to come back after resetting itself for a polling change.
const POLLING_RESET_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

/// Wait for the keyboard to re-enumerate, then confirm it reports the requested rate.
fn confirm_polling_after_reset(backend: &dyn Backend, id: &str, hz: u32) -> Result<()> {
    let code = u32::from(bragi::polling_code(hz).context("Unsupported polling rate")?);
    let deadline = std::time::Instant::now() + POLLING_RESET_WAIT;
    let mut last = None;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let Some(keyboard) = backend.keyboards().into_iter().find(|k| k.id == id) else {
            continue;
        };
        match session(backend, &keyboard, false).and_then(|mut bragi| bragi.get_u32(PROP_POLLING)) {
            Ok(now) if now == code => {
                ValueCache::set(id, "polling_hz", json!(hz))?;
                return Ok(());
            }
            Ok(now) => {
                last = Some(format!(
                    "it reports {} Hz",
                    bragi::polling_hz(now).unwrap_or(0)
                ))
            }
            Err(error) => last = Some(format!("{error:#}")),
        }
    }
    bail!(
        "The keyboard reset to apply {hz} Hz but did not confirm it ({}) — refresh to check",
        last.unwrap_or_else(|| "it has not reconnected yet".into())
    )
}

fn is_live_config(act: &Act) -> bool {
    matches!(
        act,
        Act::AddRemap { .. }
            | Act::RemoveRemap(_)
            | Act::SaveMacro { .. }
            | Act::DeleteMacro(_)
            | Act::SetDial(_)
            | Act::SetDialMode(_)
            | Act::SetLiveLighting(_)
            | Act::SetBaseColor(_)
            | Act::SetZoneColor { .. }
            | Act::SetKeyColors { .. }
            | Act::RemapKey { .. }
            | Act::ClearKeyColors
    )
}

type EffectChange<'a> = &'a dyn Fn(Option<Effect>) -> Result<Effect>;

/// Read-compare-skip for properties, then one software-mode dip for every write that is needed
/// (effect slot and/or properties). Each actual write is counted by the write budget.
fn apply(
    bragi: &mut Session,
    device: &str,
    now: u64,
    effect: Option<EffectChange>,
    props: &[(u8, u32, usize)],
    via_daemon: bool,
) -> Result<()> {
    let mut changed = Vec::new();
    for &(prop, value, width) in props {
        if bragi.get_u32(prop)? != value {
            changed.push((prop, value, width));
        }
    }
    if effect.is_none() && changed.is_empty() {
        return Ok(());
    }
    let written = bragi.with_software_mode(|bragi| {
        let mut cached = None;
        if let Some(change) = effect {
            let raw = bragi.read_resource(RES_K65_EFFECT)?;
            let current = match effects::decode(&raw) {
                Decoded::Known(effect) => Some(effect),
                Decoded::Unknown { .. } => None,
            };
            let target = change(current)?;
            if current == Some(target) {
                cached = Some(raw);
            } else {
                if !via_daemon {
                    WriteBudget::record(device)?;
                }
                let (declared, bytes) = effects::encode_k65_write(&target);
                bragi.write_resource(1, RES_K65_EFFECT, declared, &bytes, false)?;
                let (mut stored, data) = effects::encode_k65(&target);
                stored.extend_from_slice(&data);
                cached = Some(stored);
            }
        }
        for &(prop, value, width) in &changed {
            if !via_daemon {
                WriteBudget::record(device)?;
            }
            bragi.set(prop, value, width)?;
        }
        Ok(cached)
    })?;
    if let Some(raw) = written {
        snapshot::cache_effect(device, &raw, now);
    }
    Ok(())
}

fn brightness_props(model: Model, percent: u32) -> Vec<(u8, u32, usize)> {
    let raw = percent.min(100) * 10;
    let mut props = vec![(PROP_BRIGHTNESS, raw, 2)];
    if model == Model::K100Rgb {
        props.push((PROP_BRIGHTNESS_2, raw, 2));
    }
    props
}

fn hardware_action(
    bragi: &mut Session,
    keyboard: &Keyboard,
    act: &Act,
    now: u64,
    via_daemon: bool,
) -> Result<()> {
    let device = keyboard.id.as_str();
    match act {
        Act::SetEffect(kind) => {
            let kind = *kind;
            apply(
                bragi,
                device,
                now,
                Some(&|current: Option<Effect>| {
                    let mut effect = effects::default_effect(kind);
                    if let Some(current) = current {
                        if kind.has_speed() && current.speed.is_some() {
                            effect.speed = current.speed;
                        }
                        if kind.has_color() && current.color.is_some() {
                            effect.color = current.color;
                        }
                    }
                    Ok(effect)
                }),
                &[],
                via_daemon,
            )
        }
        Act::SetSpeed(speed) => {
            let speed = *speed;
            apply(
                bragi,
                device,
                now,
                Some(&|current: Option<Effect>| match current {
                    Some(effect) if effect.kind.has_speed() => Ok(Effect {
                        speed: Some(speed),
                        ..effect
                    }),
                    Some(effect) => bail!("{} has no speed setting", effect.kind.label()),
                    None => bail!("The current effect is unknown — choose an effect first"),
                }),
                &[],
                via_daemon,
            )
        }
        Act::SetColor(color) => {
            let color = *color;
            apply(
                bragi,
                device,
                now,
                Some(&|_| {
                    Ok(Effect {
                        kind: EffectKind::Static,
                        speed: None,
                        color: Some(color),
                    })
                }),
                &[],
                via_daemon,
            )
        }
        Act::SetBrightness(percent) => apply(
            bragi,
            device,
            now,
            None,
            &brightness_props(keyboard.model, *percent),
            via_daemon,
        ),
        Act::RereadEffect => snapshot::read_effect(bragi, device, now).map(|_| ()),
        Act::SaveLook(name) => {
            let reading = snapshot::read_effect(bragi, device, now)?;
            let effect = reading.effect().context(
                "The current effect is a custom one hyprdeck can't save — pick an effect first",
            )?;
            let brightness = super::render::percent(bragi.get_u32(PROP_BRIGHTNESS)?);
            config::update_looks(|looks| {
                looks
                    .devices
                    .entry(device.to_string())
                    .or_default()
                    .insert(name.clone(), Look { effect, brightness });
                Ok(())
            })?;
            Ok(())
        }
        Act::ApplyLook(name) => {
            let looks = config::load_looks()?;
            let look = looks
                .devices
                .get(device)
                .and_then(|looks| looks.get(name))
                .cloned()
                .with_context(|| format!("No saved look named {name:?}"))?;
            let effect = look.effect;
            apply(
                bragi,
                device,
                now,
                Some(&move |_| Ok(effect)),
                &brightness_props(keyboard.model, look.brightness),
                via_daemon,
            )
        }
        Act::SetWinLock(value) => {
            let mask = bragi.get_u32(PROP_LOCK_MASK)?;
            let mut props = Vec::new();
            if *value && mask == 0 {
                // Nothing selected would make the lock do nothing: block the Windows key.
                props.push((PROP_LOCK_MASK, 1, 1));
            }
            props.push((PROP_LOCK_ENABLE, u32::from(*value), 1));
            apply(bragi, device, now, None, &props, via_daemon)
        }
        Act::SetLockMask { bit, value } => {
            let mask = bragi.get_u32(PROP_LOCK_MASK)? & 0x0F;
            let new = if *value {
                mask | (1 << bit)
            } else {
                mask & !(1 << bit)
            };
            apply(
                bragi,
                device,
                now,
                None,
                &[(PROP_LOCK_MASK, new, 1)],
                via_daemon,
            )
        }
        Act::SetSleep(minutes) => {
            let cached = ValueCache::get(device, "sleep_minutes").and_then(|value| value.as_u64());
            if cached == Some(u64::from(*minutes)) {
                return Ok(());
            }
            if !via_daemon {
                WriteBudget::record(device)?;
            }
            let ms = minutes * 60_000;
            bragi.with_software_mode(|bragi| bragi.set(PROP_SLEEP_MS, ms, 4))?;
            ValueCache::set(device, "sleep_minutes", json!(minutes))?;
            Ok(())
        }
        Act::SetPolling(hz) => {
            let code = bragi::polling_code(*hz).context("Unsupported polling rate")?;
            let max = bragi.get_u32(PROP_MAX_POLLING)?;
            if u32::from(code) > max {
                let max_hz = bragi::polling_hz(max).unwrap_or(0);
                bail!("This keyboard supports up to {max_hz} Hz");
            }
            apply(
                bragi,
                device,
                now,
                None,
                &[(PROP_POLLING, u32::from(code), 1)],
                via_daemon,
            )?;
            ValueCache::set(device, "polling_hz", json!(hz))?;
            Ok(())
        }
        Act::Rescue => crate::rescue::rescue_with(bragi).map(|_| ()),
        other => bail!("Action {other:?} does not talk to the keyboard"),
    }
}

fn delete_look(device: &str, name: &str) -> Result<()> {
    config::update_looks(|looks| {
        let removed = looks
            .devices
            .get_mut(device)
            .and_then(|looks| looks.remove(name));
        if removed.is_none() {
            bail!("No saved look named {name:?}");
        }
        Ok(())
    })?;
    Ok(())
}

fn macro_users(live: &DeviceLive, name: &str) -> Vec<String> {
    let uses = |action: &Option<Action>| matches!(action, Some(Action::Macro { name: used }) if used == name);
    let mut users: Vec<String> = live
        .remaps
        .iter()
        .filter(|(_, action)| matches!(action, Action::Macro { name: used } if used == name))
        .map(|(key, _)| format!("remap of {key}"))
        .collect();
    if uses(&live.dial.clockwise) || uses(&live.dial.counter_clockwise) || uses(&live.dial.press) {
        users.push("the dial".to_string());
    }
    users
}

fn live_action(backend: &dyn Backend, keyboard: &Keyboard, act: &Act) -> Result<()> {
    let model = keyboard.model;
    match act {
        Act::AddRemap { key, .. } | Act::RemapKey { key, .. }
            if !labels::is_remappable(model, key) =>
        {
            if labels::is_layout_key(model, key) {
                bail!("{key} is handled by the keyboard itself and can't be remapped");
            }
            bail!("Unknown key {key:?} for this keyboard");
        }
        Act::SetKeyColors { keys, .. } => {
            if let Some(bad) = keys.iter().find(|key| !labels::is_layout_key(model, key)) {
                bail!("Unknown key {bad:?} for this keyboard");
            }
            if !keys.iter().any(|key| labels::is_paintable(model, key)) {
                bail!("Selected controls have no LEDs; no lighting settings were changed");
            }
        }
        Act::SetZoneColor { zone, .. }
            if zone != ZONE_KEYS && !layouts::zones(model).iter().any(|z| z.id == zone) =>
        {
            bail!("Unknown lighting zone {zone:?}");
        }
        _ => {}
    }
    let saved = config::update_live(|config| {
        let live = config.devices.entry(keyboard.id.clone()).or_default();
        match act {
            Act::AddRemap { key, action } => {
                live.remaps.insert(key.clone(), action.clone());
            }
            Act::RemapKey { key, target } => match target {
                Some(action) => {
                    live.remaps.insert(key.clone(), action.clone());
                }
                None => {
                    live.remaps.remove(key);
                }
            },
            Act::RemoveRemap(key) => {
                if live.remaps.remove(key).is_none() {
                    bail!("{key} is not remapped");
                }
            }
            Act::SaveMacro { name, source } => {
                let steps = config::parse_macro(source)?;
                live.macros.insert(
                    name.clone(),
                    Macro {
                        source: source.clone(),
                        steps,
                    },
                );
            }
            Act::DeleteMacro(name) => {
                let users = macro_users(live, name);
                if !users.is_empty() {
                    bail!(
                        "Macro {name:?} is used by {} — change that first",
                        users.join(", ")
                    );
                }
                if live.macros.remove(name).is_none() {
                    bail!("No macro named {name:?}");
                }
            }
            Act::SetDial(dial) => live.dial = dial.clone(),
            Act::SetDialMode(mode) => live.dial.mode = *mode,
            Act::SetLiveLighting(value) => live.lighting.enabled = *value,
            Act::SetBaseColor(color) => live.lighting.base = Some(*color),
            Act::SetZoneColor { zone, color } => {
                live.lighting.zones.insert(zone.clone(), *color);
            }
            Act::SetKeyColors { keys, color } => {
                for key in keys.iter().filter(|key| labels::is_paintable(model, key)) {
                    live.lighting.keys.insert(key.clone(), *color);
                }
                live.lighting.enabled = true;
            }
            Act::ClearKeyColors => live.lighting.keys.clear(),
            other => bail!("Action {other:?} is not a live setting"),
        }
        Ok(())
    })?;
    sync_helper(backend, &saved)
}

/// The background helper runs exactly while some keyboard has a feature that needs it
/// (custom dial, per-key lighting, remaps). Nothing for the user to enable or restart.
pub(super) fn sync_helper(backend: &dyn Backend, saved: &config::LiveConfig) -> Result<()> {
    let needed = saved.devices.values().any(|device| {
        device.dial.mode != DialMode::Volume || device.lighting.enabled || !device.remaps.is_empty()
    });
    let running = matches!(backend.daemon_hello(), Ok(Some(_)));
    if !needed {
        return service::ensure_stopped(backend);
    }
    if running {
        match backend.daemon_reload() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => {
                return Err(error).context(
                    "Saved, but the keyboard helper did not pick up the change — try again",
                );
            }
        }
    }
    service::ensure_running(backend)
        .context("Saved, but the keyboard helper could not be started")?;
    let deadline = std::time::Instant::now() + HELPER_START_WAIT;
    loop {
        if matches!(backend.daemon_hello(), Ok(Some(_))) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "Saved, but the keyboard helper did not start — see `systemctl --user status {}`",
                service::UNIT_NAME
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// How long the helper may take to answer after being started.
const HELPER_START_WAIT: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 100 } else { 3000 });

#[cfg(test)]
mod safety_regressions {
    use super::*;
    use crate::devices::LinkKind;
    use crate::ipc::DeviceStatus;
    use std::path::PathBuf;

    struct Fake {
        reload: Option<bool>,
    }
    impl Backend for Fake {
        fn keyboards(&self) -> Vec<Keyboard> {
            vec![]
        }
        fn unsupported(&self) -> Vec<(u16, String)> {
            vec![]
        }
        fn daemon_hello(&self) -> Result<Option<Vec<DeviceStatus>>> {
            Ok(Some(Vec::new()))
        }
        fn daemon_reload(&self) -> Result<bool> {
            self.reload.context("reload rejected")
        }
        fn open(&self, _: &Keyboard, _: bool) -> Result<Box<dyn crate::bragi::Transport>> {
            bail!("no hardware")
        }
        fn open_receiver(&self, _: &Keyboard) -> Result<Box<dyn crate::bragi::Transport>> {
            bail!("no hardware")
        }
        fn input_rule_installed(&self) -> bool {
            false
        }
        fn systemctl(&self, _: &[&str]) -> Result<()> {
            bail!("no services")
        }
        fn unit_path(&self) -> Result<PathBuf> {
            bail!("no services")
        }
        fn exe(&self) -> Result<PathBuf> {
            bail!("no executable")
        }
    }
    fn keyboard() -> Keyboard {
        Keyboard {
            id: format!("k65plus-safety-c-{}", std::process::id()),
            model: Model::K65PlusWireless,
            link: LinkKind::Usb,
            name: "K65",
            vid: 0x1b1c,
            pid: 0x2b10,
            serial: "test".into(),
            usb_path: "/nonexistent".into(),
            cmd_node: "/nonexistent".into(),
            event_node: None,
            evdev_nodes: vec![],
            frame_len: 64,
            target: 8,
        }
    }
    #[test]
    fn saved_not_applied_is_an_error_and_dial_mode_preserves_custom_fields() {
        super::super::tests::test_root();
        let keyboard = keyboard();
        let original = DialConfig {
            mode: DialMode::Custom,
            clockwise: Some(Action::Command {
                argv: vec!["printf".into(), "hello world".into()],
            }),
            counter_clockwise: Some(Action::Disabled),
            press: Some(Action::Key {
                combo: "ctrl+t".into(),
            }),
        };
        config::update_live(|config| {
            config.devices.entry(keyboard.id.clone()).or_default().dial = original.clone();
            Ok(())
        })
        .unwrap();
        for reload in [None, Some(false)] {
            let act = parse(
                "set-dial",
                json!({"device": keyboard.id, "value": "scroll"})
                    .as_object()
                    .unwrap(),
            )
            .unwrap()
            .act;
            let result = live_action(&Fake { reload }, &keyboard, &act);
            assert!(result.is_err(), "failed reload incorrectly completed");
            assert!(format!("{:#}", result.unwrap_err()).contains("Saved, but"));
        }
        let act = parse(
            "set-dial",
            json!({"device": keyboard.id, "value": "custom"})
                .as_object()
                .unwrap(),
        )
        .unwrap()
        .act;
        live_action(&Fake { reload: Some(true) }, &keyboard, &act).unwrap();
        assert_eq!(
            config::load_live().unwrap().devices[&keyboard.id].dial,
            original
        );
    }
    #[test]
    fn unlit_dial_selection_rejected_before_save_or_enable() {
        super::super::tests::test_root();
        let mut keyboard = keyboard();
        keyboard.id.push_str("-unlit");
        let before = config::load_live()
            .unwrap()
            .devices
            .get(&keyboard.id)
            .cloned();
        let result = live_action(
            &Fake { reload: Some(true) },
            &keyboard,
            &Act::SetKeyColors {
                keys: vec!["dial".into()],
                color: Rgb(255, 0, 0),
            },
        );
        assert!(result.is_err(), "unlit dial paint was accepted");
        assert_eq!(
            config::load_live()
                .unwrap()
                .devices
                .get(&keyboard.id)
                .cloned(),
            before
        );
    }
}
