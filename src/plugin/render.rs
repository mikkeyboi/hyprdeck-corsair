//! State rendering: one product per keyboard with the sections of DESIGN §6. Rendering is pure
//! (no I/O) given a [`Snapshot`] and the loaded configuration.

use super::model::{
    Battery, Group, Product, Row, Section, button, choice, choice_field, color, form, number,
    option, switch, text_field,
};
use super::service::{INPUT_RULE_COMMANDS, ServiceState};
use super::snapshot::{Link, Snapshot};
use super::{labels, viz};
use crate::bragi::{
    self, PROP_APP_FW, PROP_BATTERY, PROP_BOOT_FW, PROP_BRIGHTNESS, PROP_CHARGE, PROP_LAYOUT,
    PROP_LOCK_ENABLE, PROP_LOCK_MASK, PROP_MAX_POLLING, PROP_MODE, PROP_POLLING, PROP_RADIO_FW,
};
use crate::config::{DeviceLive, DialMode, LiveConfig, Looks};
use crate::devices::{Keyboard, LinkKind, Model, PhysicalLayout};
use crate::effects::{EffectKind, Rgb, Speed};
use crate::ipc::{self, DeviceStatus};
use crate::layouts;
use serde_json::{Value, json};

/// Everything gathered once per request that rendering needs besides the snapshot.
pub struct View<'a> {
    pub viz: bool,
    pub daemon: &'a Daemon,
    pub service: ServiceState,
    pub input_rule: bool,
    pub live: &'a Result<LiveConfig, String>,
    pub looks: &'a Result<Looks, String>,
    pub writes_today: u32,
}

#[derive(Debug, Clone)]
pub enum Daemon {
    NotRunning,
    Running(Vec<DeviceStatus>),
    Error(String),
}

impl Daemon {
    pub fn running(&self) -> bool {
        matches!(self, Daemon::Running(_))
    }
    pub fn status(&self, device: &str) -> Option<&DeviceStatus> {
        match self {
            Daemon::Running(devices) => devices.iter().find(|status| status.id == device),
            _ => None,
        }
    }
    pub fn devices(&self) -> Option<&[DeviceStatus]> {
        match self {
            Daemon::Running(devices) => Some(devices),
            _ => None,
        }
    }
}

pub const LOCK_BITS: [&str; 4] = [
    "Block the Windows key",
    "Block Alt+Tab",
    "Block Alt+F4",
    "Block Shift+Tab",
];
pub const SLEEP_MINUTES: [u32; 6] = [1, 5, 10, 15, 30, 60];
pub const ZONE_KEYS: &str = "keys";

pub fn percent(raw: u32) -> u32 {
    (raw.min(1000) + 5) / 10
}

fn section(id: &'static str, title: &'static str, groups: &[&Group]) -> Section {
    Section {
        id,
        title,
        groups: groups.iter().map(|group| group.id.clone()).collect(),
    }
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Firmware bytes without trailing zero padding.
fn firmware(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(1, |at| at + 1);
    hex_bytes(&bytes[..end.min(bytes.len())])
}

fn charge_label(code: u32) -> Option<(&'static str, Option<bool>)> {
    Some(match code {
        0 => ("Not charging", Some(false)),
        1 => ("Charging", Some(true)),
        2 => ("On battery", Some(false)),
        3 => ("Fully charged", Some(false)),
        _ => return None,
    })
}

fn layout_label(code: u32) -> &'static str {
    match PhysicalLayout::from_code(code) {
        Some(PhysicalLayout::Ansi) => "ANSI (US)",
        Some(PhysicalLayout::Iso) => "ISO (EU/UK)",
        Some(PhysicalLayout::Abnt) => "ABNT (Brazil)",
        Some(PhysicalLayout::Jis) => "JIS (Japan)",
        None => "Unknown",
    }
}

/// Physical layout from the snapshot (ANSI when unknown).
pub fn physical(snapshot: &Snapshot) -> PhysicalLayout {
    snapshot
        .value(PROP_LAYOUT)
        .and_then(PhysicalLayout::from_code)
        .unwrap_or(PhysicalLayout::Ansi)
}

pub fn connection(keyboard: &Keyboard) -> &'static str {
    match keyboard.link {
        LinkKind::Receiver => "SLIPSTREAM 2.4 GHz receiver",
        LinkKind::Usb => "USB",
    }
}

fn product_status(snapshot: &Snapshot) -> String {
    match &snapshot.link {
        Link::Asleep => "Asleep — press a key".into(),
        Link::Failed(_) => "Unavailable".into(),
        Link::Online => match snapshot.value(PROP_MODE) {
            Some(1) => "Connected · Hardware mode".into(),
            Some(2) if snapshot.via_daemon => "Connected".into(),
            Some(2) => "Connected".into(),
            _ => "Connected".into(),
        },
    }
}

/// "Rain (slow)" / "Static #FF0000" / "Custom (0x1234)".
pub fn effect_summary(snapshot: &Snapshot) -> Option<String> {
    snapshot
        .effect
        .as_ref()
        .and_then(|reading| reading.decoded())
        .map(|decoded| decoded.describe())
}

/// Build the groups and product for one keyboard.
pub fn keyboard(view: &View, keyboard: &Keyboard, snapshot: &Snapshot) -> (Vec<Group>, Product) {
    let id = keyboard.id.as_str();
    let caps = keyboard.model.capabilities(keyboard.link);
    let device_live = match view.live {
        Ok(config) => config.devices.get(id).cloned().unwrap_or_default(),
        Err(_) => DeviceLive::default(),
    };
    let mut groups = Vec::new();
    let mut sections = Vec::new();

    let overview = overview(view, keyboard, snapshot, &device_live);
    let mut overview_ids = vec![overview.id.clone()];
    groups.push(overview);
    if let Link::Failed(error) = &snapshot.link {
        let mut group = Group::new(
            format!("error:{id}"),
            "Keyboard unavailable",
            "Nothing was written to the keyboard",
        );
        group.row(Row::info(
            "error",
            "Couldn't read the keyboard",
            format!("{error} — no setting was changed."),
        ));
        overview_ids.push(group.id.clone());
        groups.push(group);
    }
    sections.push(super::model::Section {
        id: "overview",
        title: "Overview",
        groups: overview_ids,
    });

    if snapshot.online() {
        let mut lighting_groups = vec![lighting(keyboard, snapshot)];
        if caps.onboard_effects {
            lighting_groups.push(looks(view, keyboard));
        }
        sections.push(Section {
            id: "lighting",
            title: "Lighting",
            groups: lighting_groups.iter().map(|g| g.id.clone()).collect(),
        });
        groups.extend(lighting_groups);
    }
    if caps.live_lighting {
        let group = live_lighting(view, keyboard, &device_live);
        sections.push(section("live-lighting", "Per-key lighting", &[&group]));
        groups.push(group);
    }

    let mut key_groups = Vec::new();
    if snapshot.online() && caps.win_lock {
        key_groups.push(lock_group(keyboard, snapshot));
    }
    key_groups.push(remaps(view, keyboard, snapshot, &device_live));
    key_groups.push(macros(view, keyboard, &device_live));
    if keyboard.model == Model::K65PlusWireless {
        key_groups.push(dial(view, keyboard, &device_live));
    }
    sections.push(Section {
        id: "keys",
        title: "Keys & Macros",
        groups: key_groups.iter().map(|g| g.id.clone()).collect(),
    });
    groups.extend(key_groups);

    if snapshot.online() {
        let group = power(keyboard, snapshot);
        sections.push(section("power", "Power & performance", &[&group]));
        groups.push(group);
    }

    let helper = live_mode(view, keyboard, snapshot, &device_live);

    let diagnostics = diagnostics(view, keyboard, snapshot);
    sections.push(section(
        "diagnostics",
        "Diagnostics",
        &[&diagnostics, &helper],
    ));
    groups.push(diagnostics);
    groups.push(helper);

    let battery = (caps.battery && snapshot.online())
        .then(|| snapshot.value(PROP_BATTERY))
        .flatten()
        .map(|per_mille| Battery {
            percentage: Some((f64::from(per_mille.min(1000))) / 10.0),
            charging: snapshot
                .value(PROP_CHARGE)
                .and_then(charge_label)
                .and_then(|(_, charging)| charging),
            source: "Keyboard".into(),
        });
    let product = Product {
        id: id.to_string(),
        name: keyboard.name.to_string(),
        kind: "keyboard",
        description: match keyboard.model {
            Model::K65PlusWireless => "Onboard lighting, keys, dial and power".into(),
            Model::K100Rgb => "Brightness, keys and per-key lighting".into(),
        },
        status: product_status(snapshot),
        connection: connection(keyboard).into(),
        battery,
        sections,
    };
    (groups, product)
}

fn overview(view: &View, keyboard: &Keyboard, snapshot: &Snapshot, live: &DeviceLive) -> Group {
    let id = &keyboard.id;
    let caps = keyboard.model.capabilities(keyboard.link);
    let mut group = Group::new(format!("overview:{id}"), "Keyboard", connection(keyboard));
    if view.viz {
        group.visualization = Some(viz::visualization(view, keyboard, snapshot, live));
    }
    match &snapshot.link {
        Link::Asleep => {
            group.row(Row::info(
                "asleep",
                "Keyboard asleep or off",
                "Press any key on it to wake it, then refresh. Saved settings are still shown.",
            ));
        }
        Link::Failed(_) => {}
        Link::Online => {
            if caps.battery
                && let Some(per_mille) = snapshot.value(PROP_BATTERY)
            {
                let charge = snapshot
                    .value(PROP_CHARGE)
                    .and_then(charge_label)
                    .map_or("", |(label, _)| label);
                let text = format!("{} %", percent(per_mille));
                group.row(Row::info(
                    "battery",
                    "Battery",
                    if charge.is_empty() {
                        text
                    } else {
                        format!("{text} · {charge}")
                    },
                ));
            }
            let mode = match snapshot.value(PROP_MODE) {
                Some(1) => "Hardware mode — the keyboard runs its onboard settings".to_string(),
                Some(2) if snapshot.via_daemon => {
                    "Hyprdeck is driving the dial or lighting right now".to_string()
                }
                Some(2) => {
                    "Software mode with nothing driving it — use Rescue in Diagnostics if this persists"
                        .to_string()
                }
                Some(other) => format!("Unknown mode {other}"),
                None => "Unknown".to_string(),
            };
            group.row(Row::info("mode", "Mode", mode));
            if let Some(code) = snapshot.value(PROP_LAYOUT) {
                group.row(Row::info("layout", "Layout", layout_label(code)));
            }
            let mut parts = Vec::new();
            if let Some(bytes) = snapshot.props.get(&PROP_APP_FW) {
                parts.push(format!("Keyboard {}", firmware(bytes)));
            }
            if let Some(bytes) = &snapshot.receiver_firmware {
                parts.push(format!("Receiver {}", firmware(bytes)));
            }
            if !parts.is_empty() {
                group.row(Row::info("firmware", "Firmware", parts.join(" · ")));
            }
            if let Some(hz) = snapshot.value(PROP_POLLING).and_then(bragi::polling_hz) {
                group.row(Row::info("polling", "Polling rate", format!("{hz} Hz")));
            }
            if let Some(summary) = effect_summary(snapshot) {
                group.row(Row::info("effect", "Onboard effect", summary));
            }
        }
    }
    group
}

fn lighting(keyboard: &Keyboard, snapshot: &Snapshot) -> Group {
    let id = keyboard.id.as_str();
    let caps = keyboard.model.capabilities(keyboard.link);
    let mut group = Group::new(
        format!("lighting:{id}"),
        "Lighting",
        if caps.onboard_effects {
            "Saved on the keyboard; works without hyprdeck running"
        } else {
            "Brightness is saved on the keyboard"
        },
    );
    if caps.onboard_effects {
        let reading = snapshot.effect.as_ref();
        let current = reading.and_then(|reading| reading.effect());
        let mut subtitle = match reading {
            Some(reading) if !reading.raw.is_empty() => format!(
                "{} · {} · applies immediately (also FN+1…0)",
                reading.decoded().map(|d| d.describe()).unwrap_or_default(),
                as_of_plain(reading.read_at)
            ),
            _ => "Not read yet · applies immediately (also FN+1…0)".to_string(),
        };
        if let Some(error) = reading.and_then(|reading| reading.error.as_ref()) {
            subtitle = format!("{subtitle} · last read failed: {error}");
        }
        let mut options: Vec<Value> = EffectKind::ALL
            .iter()
            .map(|kind| {
                option(
                    kind.value(),
                    format!("{} ({})", kind.label(), kind.shortcut()),
                )
            })
            .collect();
        let value = match current {
            Some(effect) => effect.kind.value().to_string(),
            None => {
                let label = reading
                    .and_then(|reading| reading.decoded())
                    .map_or("Unknown — not read yet".to_string(), |d| d.describe());
                options.insert(0, option("unknown", label));
                "unknown".to_string()
            }
        };
        group.row(Row::info("effect", "Effect", subtitle).controlled(choice(
            &value,
            options,
            "set-effect",
            id,
            json!({}),
        )));
        if let Some(effect) = current {
            if effect.kind.has_speed() {
                let speed = effect.speed.unwrap_or(Speed::Medium);
                let options = Speed::ALL
                    .iter()
                    .map(|speed| option(speed.value(), speed.label()))
                    .collect();
                group.row(
                    Row::info("speed", "Speed", "Animation speed of the current effect")
                        .controlled(choice(speed.value(), options, "set-speed", id, json!({}))),
                );
            }
            if effect.kind.has_color() {
                let value = effect.color.unwrap_or(Rgb(0xff, 0xff, 0xff)).hex();
                group.row(
                    Row::info("color", "Colour", "Colour of the Static effect").controlled(color(
                        &value,
                        "set-color",
                        id,
                        json!({}),
                    )),
                );
            }
        }
    }
    match snapshot.value(PROP_BRIGHTNESS) {
        Some(raw) => {
            let value = percent(raw);
            group.row(
                Row::info(
                    "brightness",
                    "Brightness",
                    format!("{value} % · FN+F5/F6 also change it"),
                )
                .controlled(number(
                    f64::from(value),
                    0.0,
                    100.0,
                    5.0,
                    "set-brightness",
                    id,
                )),
            );
            group.row(
                Row::info("lights-off", "Lights off", "Brightness 0; keeps the effect").controlled(
                    button(
                        "Lights off",
                        "brightness-preset",
                        id,
                        json!({"value": "off"}),
                    ),
                ),
            );
            group.row(
                Row::info("full", "Full brightness", "Brightness 100 %").controlled(button(
                    "Full",
                    "brightness-preset",
                    id,
                    json!({"value": "full"}),
                )),
            );
        }
        None => {
            let error = snapshot
                .prop_errors
                .get(&PROP_BRIGHTNESS)
                .cloned()
                .unwrap_or_else(|| "not reported".into());
            group.row(Row::info(
                "brightness",
                "Brightness",
                format!("Unavailable: {error}"),
            ));
        }
    }
    if caps.onboard_effects {
        group.row(
            Row::info(
                "reread",
                "Re-read from keyboard",
                "Fetches the effect now (FN shortcuts are not reported automatically)",
            )
            .controlled(button("Re-read", "reread-effect", id, json!({}))),
        );
    } else {
        group.row(Row::info(
            "onboard-note",
            "Onboard effects",
            "Set in iCUE on this model. Use Per-key lighting for colours from hyprdeck.",
        ));
    }
    group
}

fn as_of_plain(read_at: u64) -> String {
    if read_at == 0 {
        "not read yet".into()
    } else {
        format!("as of {}", labels::clock(read_at))
    }
}

fn looks(view: &View, keyboard: &Keyboard) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("looks:{id}"),
        "Saved looks",
        "Effect + brightness presets stored on this computer",
    );
    let looks = match view.looks {
        Ok(looks) => looks.devices.get(id).cloned().unwrap_or_default(),
        Err(error) => {
            group.row(Row::info(
                "looks-error",
                "Saved looks unavailable",
                format!("{error} — fix or remove looks.json"),
            ));
            return group;
        }
    };
    group.row(
        Row::info("save-look", "Save current look", "Name it to reuse later").controlled(form(
            "Save",
            "save-look",
            id,
            json!({}),
            vec![text_field("name", "Name", "")],
        )),
    );
    if looks.is_empty() {
        group.row(Row::info(
            "no-looks",
            "No saved looks yet",
            "Save one above",
        ));
        return group;
    }
    let options: Vec<Value> = looks
        .iter()
        .map(|(name, look)| {
            option(
                name.as_str(),
                format!("{name} — {}, {} %", look.effect.describe(), look.brightness),
            )
        })
        .collect();
    let first = looks.keys().next().cloned().unwrap_or_default();
    group.row(
        Row::info("apply-look", "Apply look", "Sets effect and brightness").controlled(choice(
            &first,
            options.clone(),
            "apply-look",
            id,
            json!({}),
        )),
    );
    group.row(
        Row::info(
            "delete-look",
            "Delete look",
            "Removes it from this computer only",
        )
        .controlled(form(
            "Delete",
            "delete-look",
            id,
            json!({}),
            vec![choice_field("look", "Look", &first, options)],
        )),
    );
    group
}

fn companion_note(view: &View) -> &'static str {
    match view.service {
        ServiceState::Running => "",
        ServiceState::Stopped => "not running — choose it again to restart",
        ServiceState::NotInstalled => "not running — choose it again to restart",
    }
}

fn live_lighting(view: &View, keyboard: &Keyboard, live: &DeviceLive) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("live-lighting:{id}"),
        "Per-key lighting",
        "Your own colours per key and zone; the onboard effect returns when this is off",
    );
    if let Err(error) = view.live {
        group.row(Row::info(
            "live-error",
            "Live settings unavailable",
            error.clone(),
        ));
        return group;
    }
    let lighting = &live.lighting;
    group.row(
        Row::info(
            "enabled",
            "Use per-key lighting",
            "Off returns the keyboard to its onboard effect",
        )
        .controlled(switch(lighting.enabled, "set-live-lighting", id, json!({}))),
    );
    let base = lighting.base.unwrap_or(Rgb(0xff, 0xff, 0xff)).hex();
    group.row(
        Row::info(
            "base",
            "Base colour",
            "Every LED without a zone or key colour",
        )
        .controlled(color(&base, "set-base-color", id, json!({}))),
    );
    if keyboard.model == Model::K100Rgb {
        let zones = std::iter::once((ZONE_KEYS, "All keys")).chain(
            layouts::zones(keyboard.model)
                .iter()
                .map(|zone| (zone.id, zone.label)),
        );
        for (zone, label) in zones {
            let value = lighting
                .zones
                .get(zone)
                .or(lighting.base.as_ref())
                .copied()
                .unwrap_or(Rgb(0xff, 0xff, 0xff))
                .hex();
            group.row(
                Row::info(format!("zone-{zone}"), label, "Overrides the base colour")
                    .controlled(color(&value, "set-zone-color", id, json!({"zone": zone}))),
            );
        }
    }
    let count = lighting.keys.len();
    group.row(Row::info(
        "per-key",
        "Single keys",
        match (count, view.viz) {
            (0, true) => "Select keys on the keyboard picture, then apply a colour".to_string(),
            (0, false) => "Needs a Hyprdeck version with the keyboard picture".to_string(),
            (n, _) => format!("{n} key(s) have their own colour"),
        },
    ));
    if count > 0 {
        group.row(
            Row::info(
                "clear",
                "Clear per-key colours",
                "Keeps base and zone colours",
            )
            .controlled(button("Clear", "clear-key-colors", id, json!({}))),
        );
    }
    group
}

fn lock_group(keyboard: &Keyboard, snapshot: &Snapshot) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("keys-lock:{id}"),
        "Gaming lock",
        "Saved on the keyboard",
    );
    let (Some(enabled), Some(mask)) = (
        snapshot.value(PROP_LOCK_ENABLE),
        snapshot.value(PROP_LOCK_MASK),
    ) else {
        group.row(Row::info(
            "lock-unavailable",
            "Win lock",
            "The keyboard did not report its lock settings",
        ));
        return group;
    };
    let locked: Vec<&str> = (0..4)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| LOCK_BITS[bit].trim_start_matches("Block "))
        .collect();
    let subtitle = if locked.is_empty() {
        "Nothing selected below — turning it on blocks the Windows key".to_string()
    } else {
        format!("Blocks: {}", locked.join(", "))
    };
    group.row(
        Row::info("win-lock", "Win lock", subtitle).controlled(switch(
            enabled == 1,
            "set-win-lock",
            id,
            json!({}),
        )),
    );
    for (bit, label) in LOCK_BITS.iter().enumerate() {
        group.row(
            Row::info(
                format!("lock-bit-{bit}"),
                *label,
                "Applies while Win lock is on",
            )
            .controlled(switch(
                mask & (1 << bit) != 0,
                "set-lock-mask",
                id,
                json!({"bit": bit}),
            )),
        );
    }
    group
}

fn remaps(view: &View, keyboard: &Keyboard, snapshot: &Snapshot, live: &DeviceLive) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("remaps:{id}"),
        "Remaps",
        "Change what a key does on this computer (not saved on the keyboard)",
    );
    if let Err(error) = view.live {
        group.row(Row::info(
            "live-error",
            "Live settings unavailable",
            error.clone(),
        ));
        return group;
    }
    let status = if !view.input_rule {
        "Needs the input permission rule (see Diagnostics); remaps are saved but inactive"
    } else if view.service != ServiceState::Running {
        "Saved but not running — add or change a remap to restart"
    } else {
        if view.daemon.status(id).is_some_and(|status| {
            status.grabbed
                && status.features.iter().any(|feature| feature == "remap")
                && status.remaps_active == Some(true)
                && status.config_revision.as_deref()
                    == Some(crate::config::device_revision(live).as_str())
                && status.error.is_none()
        }) {
            "Active"
        } else {
            "Saved — not active yet"
        }
    };
    group.row(Row::info("remap-status", "Status", status));
    let layout = layouts::layout(keyboard.model, physical(snapshot));
    let keys = labels::remappable_keys(layout);
    let first = keys
        .first()
        .map(|(value, _)| value.clone())
        .unwrap_or_default();
    let key_options: Vec<Value> = keys
        .into_iter()
        .map(|(value, label)| option(value, label))
        .collect();
    let targets = labels::target_options(live, false);
    if !key_options.is_empty() {
        group.row(
            Row::info(
                "add-remap",
                "Add remap",
                "A combo (e.g. ctrl+shift+t) overrides the action",
            )
            .controlled(form(
                "Add remap",
                "add-remap",
                id,
                json!({}),
                vec![
                    choice_field("key", "Key", &first, key_options),
                    choice_field("action", "Does", "disabled", targets),
                    text_field("combo", "Combo (optional)", ""),
                ],
            )),
        );
    }
    for (key, action) in &live.remaps {
        let key_label = layouts::key_by_id(layout, key).map_or(key.as_str(), |key| key.label);
        group.row(
            Row::info(
                format!("remap-{key}"),
                format!("{key_label} → {}", labels::action_label(action)),
                format!("Key id {key}"),
            )
            .controlled(button("Remove", "remove-remap", id, json!({"key": key}))),
        );
    }
    group
}

fn macros(view: &View, keyboard: &Keyboard, live: &DeviceLive) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("macros:{id}"),
        "Macros",
        "Bind one with a remap or the dial",
    );
    if view.live.is_err() {
        return group;
    }
    group.row(
        Row::info(
            "save-macro",
            "Save macro",
            "Steps: ctrl+c, delay:50, text:hello, down:shift, up:shift, media:play_pause",
        )
        .controlled(form(
            "Save macro",
            "save-macro",
            id,
            json!({}),
            vec![
                text_field("name", "Name", ""),
                text_field("steps", "Steps", ""),
            ],
        )),
    );
    if live.macros.is_empty() {
        group.row(Row::info(
            "no-macros",
            "No macros yet",
            "Saving one with an existing name replaces it",
        ));
    }
    for (name, item) in &live.macros {
        group.row(
            Row::info(format!("macro:{name}"), name.as_str(), item.source.as_str())
                .controlled(button("Delete", "delete-macro", id, json!({"name": name}))),
        );
    }
    group
}

pub fn dial_label(mode: DialMode) -> &'static str {
    match mode {
        DialMode::Volume => "Volume (onboard)",
        DialMode::Scroll => "Scroll",
        DialMode::Zoom => "Zoom",
        DialMode::Brightness => "Keyboard brightness",
        DialMode::Media => "Media (next/previous, press play/pause)",
        DialMode::Workspace => "Workspaces (left/right)",
        DialMode::Custom => "Custom",
    }
}

pub const DIAL_MODES: [DialMode; 7] = [
    DialMode::Volume,
    DialMode::Scroll,
    DialMode::Zoom,
    DialMode::Brightness,
    DialMode::Media,
    DialMode::Workspace,
    DialMode::Custom,
];

pub fn dial_value(mode: DialMode) -> &'static str {
    match mode {
        DialMode::Volume => "volume",
        DialMode::Scroll => "scroll",
        DialMode::Zoom => "zoom",
        DialMode::Brightness => "brightness",
        DialMode::Media => "media",
        DialMode::Workspace => "workspace",
        DialMode::Custom => "custom",
    }
}

fn dial(view: &View, keyboard: &Keyboard, live: &DeviceLive) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("dial:{id}"),
        "Dial",
        "Volume is built into the keyboard; hyprdeck handles the other modes in the background",
    );
    if view.live.is_err() {
        return group;
    }
    let mode = live.dial.mode;
    let options = DIAL_MODES
        .iter()
        .map(|mode| option(dial_value(*mode), dial_label(*mode)))
        .collect();
    let subtitle = if mode != DialMode::Volume && view.service != ServiceState::Running {
        format!("{} — {}", dial_label(mode), companion_note(view))
    } else {
        dial_label(mode).to_string()
    };
    group.row(
        Row::info("dial-mode", "Dial does", subtitle).controlled(choice(
            dial_value(mode),
            options,
            "set-dial",
            id,
            json!({}),
        )),
    );
    if mode == DialMode::Custom {
        let text = |action: &Option<crate::config::Action>| {
            action
                .as_ref()
                .map(labels::action_value)
                .unwrap_or_default()
        };
        group.row(
            Row::info(
                "dial-custom",
                "Custom dial actions",
                "Each: a combo, media:next, macro:<name>, text:…, command:… or brightness:+10",
            )
            .controlled(form(
                "Apply",
                "set-dial",
                id,
                json!({"mode": "custom"}),
                vec![
                    text_field("clockwise", "Turn right", &text(&live.dial.clockwise)),
                    text_field(
                        "counter-clockwise",
                        "Turn left",
                        &text(&live.dial.counter_clockwise),
                    ),
                    text_field("press", "Press", &text(&live.dial.press)),
                ],
            )),
        );
    }
    group
}

fn power(keyboard: &Keyboard, snapshot: &Snapshot) -> Group {
    let id = keyboard.id.as_str();
    let caps = keyboard.model.capabilities(keyboard.link);
    let mut group = Group::new(
        format!("power:{id}"),
        "Power & performance",
        "Saved on the keyboard",
    );
    if caps.sleep_timeout {
        let cached = crate::storage::ValueCache::get(id, "sleep_minutes")
            .and_then(|value| value.as_u64())
            .and_then(|minutes| u32::try_from(minutes).ok())
            .filter(|minutes| SLEEP_MINUTES.contains(minutes));
        let mut options: Vec<Value> = SLEEP_MINUTES
            .iter()
            .map(|minutes| {
                option(
                    minutes.to_string(),
                    if *minutes == 1 {
                        "1 minute".to_string()
                    } else {
                        format!("{minutes} minutes")
                    },
                )
            })
            .collect();
        let value = match cached {
            Some(minutes) => minutes.to_string(),
            None => {
                options.insert(0, option("unset", "Not set from hyprdeck"));
                "unset".to_string()
            }
        };
        group.row(
            Row::info(
                "sleep",
                "Sleep after",
                "The keyboard can't report this; shows the last value set here",
            )
            .controlled(choice(&value, options, "set-sleep", id, json!({}))),
        );
    }
    let max = snapshot.value(PROP_MAX_POLLING);
    let current = snapshot.value(PROP_POLLING).and_then(bragi::polling_hz);
    match max {
        // A fixed-rate keyboard shows its rate in Overview only.
        Some(_) if caps.polling_rates.is_empty() => {}
        Some(max) => {
            let mut rates: Vec<u32> = caps
                .polling_rates
                .iter()
                .copied()
                .filter(|hz| bragi::polling_code(*hz).is_some_and(|code| u32::from(code) <= max))
                .collect();
            if let Some(hz) = current
                && !rates.contains(&hz)
            {
                rates.push(hz);
                rates.sort_unstable();
            }
            if let Some(&fallback) = rates.last() {
                let value = current.unwrap_or(fallback);
                let options = rates
                    .iter()
                    .map(|hz| option(hz.to_string(), format!("{hz} Hz")))
                    .collect();
                group.row(
                    Row::info(
                        "polling",
                        "Polling rate",
                        "The keyboard reconnects for about a second to apply a change",
                    )
                    .controlled(choice(
                        &value.to_string(),
                        options,
                        "set-polling",
                        id,
                        json!({}),
                    )),
                );
            }
        }
        None => {
            group.row(Row::info(
                "polling",
                "Polling rate",
                current.map_or("Unknown".to_string(), |hz| format!("{hz} Hz")),
            ));
        }
    }
    group
}

fn live_mode(view: &View, keyboard: &Keyboard, snapshot: &Snapshot, live: &DeviceLive) -> Group {
    let id = keyboard.id.as_str();
    let caps = keyboard.model.capabilities(keyboard.link);
    let mut group = Group::new(
        format!("live-mode:{id}"),
        "Background helper",
        "Starts by itself when the dial, per-key lighting or remaps need it, and stops when they don't",
    );
    let mut status = view.service.label().to_string();
    if let Some(device) = view.daemon.status(id) {
        status.push_str(if device.mode == 0 {
            " · mode uncertain — Rescue required"
        } else if device.owned {
            " · driving this keyboard"
        } else {
            " · keyboard stays in hardware mode"
        });
        if device.grabbed
            && device.features.iter().any(|feature| feature == "remap")
            && device.remaps_active == Some(true)
            && device.config_revision.as_deref()
                == Some(crate::config::device_revision(live).as_str())
            && device.error.is_none()
        {
            status.push_str(" · remaps active");
        }
        if let Some(error) = &device.error {
            status.push_str(&format!(" · {error}"));
        }
    } else if view.service == ServiceState::Running {
        status.push_str(" · not handling this keyboard");
    }
    if let Some(mode) = snapshot.value(PROP_MODE) {
        status.push_str(match mode {
            1 => " · keyboard in hardware mode",
            2 => " · keyboard in software mode",
            _ => "",
        });
    }
    group.row(Row::info("status", "Helper", status));
    if let Daemon::Error(error) = view.daemon {
        group.row(Row::info(
            "daemon-error",
            "Helper not reachable",
            error.clone(),
        ));
    }
    let mut needs = Vec::new();
    if keyboard.model == Model::K65PlusWireless && live.dial.mode != DialMode::Volume {
        needs.push(format!("dial: {}", dial_label(live.dial.mode)));
    }
    if caps.live_lighting && live.lighting.enabled {
        needs.push("per-key lighting".to_string());
    }
    if !live.remaps.is_empty() {
        needs.push(format!("{} remap(s)", live.remaps.len()));
    }
    group.row(Row::info(
        "needs",
        "Used for",
        if needs.is_empty() {
            "Nothing needs it right now".to_string()
        } else {
            needs.join(", ")
        },
    ));
    group.row(Row::info(
        "input-rule",
        "Input rule (opt-in)",
        if view.input_rule {
            "Installed — remaps, macros, the dial and the key tester can use input devices"
                .to_string()
        } else {
            format!(
                "Not installed. Needed for remaps, macros, the dial and the key tester; it lets \
                 programs in your session read these keyboards and inject input. From the release \
                 assets: {INPUT_RULE_COMMANDS}"
            )
        },
    ));
    group
}

fn diagnostics(view: &View, keyboard: &Keyboard, snapshot: &Snapshot) -> Group {
    let id = keyboard.id.as_str();
    let mut group = Group::new(
        format!("diagnostics:{id}"),
        "Diagnostics",
        "Technical details for troubleshooting",
    )
    .collapsed();
    let mut nodes = vec![format!("Commands {}", keyboard.cmd_node.display())];
    if let Some(node) = &keyboard.event_node {
        nodes.push(format!("Events {}", node.display()));
    }
    if !keyboard.evdev_nodes.is_empty() {
        let inputs: Vec<String> = keyboard
            .evdev_nodes
            .iter()
            .map(|node| node.display().to_string())
            .collect();
        nodes.push(format!("Input {}", inputs.join(", ")));
    }
    group.row(Row::info("nodes", "Device nodes", nodes.join(" · ")));
    group.row(Row::info(
        "usb",
        "USB",
        format!(
            "{:04x}:{:04x} · serial {} · {}",
            keyboard.vid,
            keyboard.pid,
            if keyboard.serial.is_empty() {
                "none"
            } else {
                &keyboard.serial
            },
            keyboard.usb_path.display()
        ),
    ));
    group.row(Row::info(
        "transport",
        "Transport",
        if snapshot.via_daemon {
            "Through the background helper"
        } else {
            "Direct (hidraw, released after each request)"
        },
    ));
    let mut dump: Vec<String> = snapshot
        .props
        .iter()
        .map(|(prop, bytes)| format!("{prop:02x}={}", ipc::to_hex(bytes)))
        .collect();
    dump.extend(
        snapshot
            .prop_errors
            .iter()
            .map(|(prop, error)| format!("{prop:02x}: {error}")),
    );
    group.row(Row::info(
        "raw",
        "Raw GET dump",
        if dump.is_empty() {
            "Nothing read".to_string()
        } else {
            dump.join(" · ")
        },
    ));
    if let Some(reading) = &snapshot.effect
        && !reading.raw.is_empty()
    {
        group.row(Row::info(
            "effect-raw",
            "Stored effect bytes",
            format!(
                "{} ({})",
                ipc::to_hex(&reading.raw),
                as_of_plain(reading.read_at)
            ),
        ));
    }
    if let Some(bytes) = snapshot.props.get(&PROP_BOOT_FW) {
        group.row(Row::info("bootloader", "Bootloader", firmware(bytes)));
    }
    if let Some(bytes) = snapshot.props.get(&PROP_RADIO_FW) {
        group.row(Row::info(
            "radio",
            "Radio firmware / hardware",
            firmware(bytes),
        ));
    }
    group.row(Row::info(
        "budget",
        "Keyboard writes today",
        format!(
            "{} of {} (a daily cap protects the keyboard's memory)",
            view.writes_today,
            crate::storage::WriteBudget::DAILY_LIMIT
        ),
    ));
    group.row(
        Row::info(
            "rescue",
            "Rescue keyboard (restore hardware mode)",
            "Closes open handles and switches back to onboard settings",
        )
        .controlled(button("Rescue", "rescue", id, json!({}))),
    );
    group
}
