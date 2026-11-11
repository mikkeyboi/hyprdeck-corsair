//! Native `keyboard` visualization (host feature `keyboard_visualization`): key rectangles from
//! `layouts`, colours per DESIGN §6, shared remap options, paint on live-lighting links and the
//! key-tester stream.

use super::labels;
use super::render::{View, physical};
use super::snapshot::{Link, Snapshot};
use crate::config::DeviceLive;
use crate::devices::Keyboard;
use crate::effects::{EffectKind, Rgb};
use crate::layouts;
use serde_json::{Value, json};

pub fn lighting_applied(status: Option<&crate::ipc::DeviceStatus>, live: &DeviceLive) -> bool {
    status.is_some_and(|status| {
        status.config_revision.as_deref() == Some(crate::config::device_revision(live).as_str())
            && status.owned
            && status.mode == crate::bragi::MODE_SOFTWARE
            && status.frame_applied == Some(true)
            && status.features.iter().any(|feature| feature == "lighting")
            && status.error.is_none()
    })
}

/// Key units rounded to 1/10000 so the JSON stays short and sums stay exact enough.
fn round(value: f32) -> f64 {
    (f64::from(value) * 10_000.0).round() / 10_000.0
}

/// Which tab the picture is on: it offers only that tab's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Lighting,
    Keys,
}

/// The host preview that matches an onboard effect (`keyboard_animation`); Static has a colour.
fn animation(effect: &crate::effects::Effect) -> Option<Value> {
    let kind = match effect.kind {
        EffectKind::Watercolor => "watercolor",
        EffectKind::SpiralRainbow => "spiral",
        EffectKind::Rain => "rain",
        EffectKind::RainbowWave => "rainbow_wave",
        EffectKind::TypeKey => "reactive",
        EffectKind::TypeRipple => "ripple",
        EffectKind::ColorShift => "shift",
        EffectKind::ColorPulse => "pulse",
        EffectKind::ColorWave => "wave",
        EffectKind::Static => return None,
    };
    let speed = effect.speed.map_or("medium", |speed| speed.value());
    Some(json!({"kind": kind, "speed": speed, "colors": []}))
}

pub fn visualization(
    view: &View,
    keyboard: &Keyboard,
    snapshot: &Snapshot,
    live: &DeviceLive,
    purpose: Purpose,
) -> Value {
    let id = keyboard.id.as_str();
    let caps = keyboard.model.capabilities(keyboard.link);
    let layout = layouts::layout(keyboard.model, physical(snapshot));
    let live_lighting = caps.live_lighting && live.lighting.enabled;
    let effect = snapshot
        .effect
        .as_ref()
        .and_then(|reading| reading.effect());
    let static_color = effect
        .filter(|effect| effect.kind == EffectKind::Static)
        .and_then(|effect| effect.color);
    let applied = lighting_applied(view.daemon.status(id), live);
    let status = match (&snapshot.link, live_lighting) {
        (Link::Asleep, _) => "Keyboard asleep — press a key".to_string(),
        (Link::Failed(_), _) => "Keyboard unavailable".to_string(),
        (_, true) if applied => "Custom colours · on the keyboard now".to_string(),
        (_, true) => "Custom colours · saved, not confirmed on the keyboard yet".to_string(),
        _ => match super::render::effect_summary(snapshot) {
            Some(summary) => summary,
            None if caps.onboard_effects => "Effect not read yet".to_string(),
            None => "Onboard lighting".to_string(),
        },
    };
    let preview = (view.animation && !live_lighting && snapshot.online())
        .then(|| effect.as_ref().and_then(animation))
        .flatten();
    let can_remap = view.input_rule && view.live.is_ok();
    let options = labels::target_options(live, true);
    let (mut width, mut height) = (round(layout.width), round(layout.height));
    let keys: Vec<Value> = layout
        .keys
        .iter()
        .map(|key| {
            let color = if live_lighting {
                crate::daemon::frame::key_color(keyboard.model, layout, key, &live.lighting)
            } else {
                static_color
            };
            let remap = live.remaps.get(key.id);
            let mut detail = Vec::new();
            if key.evdev.is_none() {
                detail.push("Handled by the keyboard; can't be remapped".to_string());
            } else if let Some(action) = remap {
                detail.push(format!("Remapped to {}", labels::action_label(action)));
            }
            if live_lighting && let Some(rgb) = live.lighting.keys.get(key.id) {
                detail.push(format!("Own colour {}", rgb.hex()));
            }
            let (x, y, w, h) = (round(key.x), round(key.y), round(key.w), round(key.h));
            width = width.max(x + w);
            height = height.max(y + h);
            let mut value = json!({
                "id": labels::viz_id(key.id),
                "label": key.label,
                "detail": detail.join(" · "),
                "x": x,
                "y": y,
                "w": w,
                "h": h,
                "color": color.map(Rgb::hex),
                "pressed": null,
            });
            if can_remap && purpose == Purpose::Keys && key.evdev.is_some() {
                value["remap"] = json!(remap.map_or("default".to_string(), labels::action_value));
            }
            value
        })
        .collect();
    let mut visualization = json!({
        "kind": "keyboard",
        "name": keyboard.name,
        "connection": super::render::connection(keyboard),
        "status": status,
        "layout": {"width": width, "height": height},
        "keys": keys,
        "stream": {"args": ["--input-stream", id]},
    });
    if let Some(preview) = preview {
        visualization["animation"] = preview;
    }
    if purpose == Purpose::Lighting && caps.live_lighting && view.live.is_ok() {
        visualization["paint"] = json!({
            "label": "Colour selected keys",
            "action": "set-key-colors",
            "args": {"device": id},
        });
    }
    if can_remap && purpose == Purpose::Keys {
        visualization["remap"] = json!({
            "label": "Selected key does",
            "action": "remap-key",
            "args": {"device": id},
            "options": options,
        });
    }
    visualization
}

#[cfg(test)]
mod acknowledgement_regressions {
    use super::*;
    use crate::ipc::DeviceStatus;
    #[test]
    fn old_frame_applied_with_stale_revision_is_never_current_preview() {
        let mut old = DeviceLive::default();
        old.lighting.enabled = true;
        let status = DeviceStatus {
            id: "k65-test".into(),
            owned: true,
            mode: 2,
            grabbed: true,
            features: vec!["lighting".into()],
            error: None,
            generation: Some(9),
            frame_applied: Some(true),
            remaps_active: Some(true),
            config_revision: Some(crate::config::device_revision(&old)),
        };
        assert!(lighting_applied(Some(&status), &old));
        let mut saved = old.clone();
        saved.lighting.base = Some(Rgb(0, 255, 0));
        assert!(!lighting_applied(Some(&status), &saved));
        let mut missing = status.clone();
        missing.config_revision = None;
        assert!(!lighting_applied(Some(&missing), &old));
        missing.config_revision = status.config_revision;
        missing.frame_applied = None;
        assert!(!lighting_applied(Some(&missing), &old));
        missing.frame_applied = Some(true);
        missing.mode = 0;
        assert!(!lighting_applied(Some(&missing), &old));
    }
    #[test]
    fn preview_colors_match_composed_frame_and_ignore_unlit_controls() {
        for model in [
            crate::devices::Model::K65PlusWireless,
            crate::devices::Model::K100Rgb,
        ] {
            let layout = layouts::layout(model, crate::devices::PhysicalLayout::Ansi);
            let mut live = DeviceLive::default();
            live.lighting.enabled = true;
            for key in layout.keys {
                assert_eq!(
                    crate::daemon::frame::key_color(model, layout, key, &live.lighting).is_some(),
                    key.led.is_some()
                );
            }
            let key = layouts::key_by_id(layout, "a").unwrap();
            assert_eq!(
                crate::daemon::frame::key_color(model, layout, key, &live.lighting),
                Some(Rgb(255, 255, 255))
            );
            live.lighting.zones.insert("keys".into(), Rgb(1, 2, 3));
            assert_eq!(
                crate::daemon::frame::key_color(model, layout, key, &live.lighting),
                Some(Rgb(1, 2, 3))
            );
        }
    }
}
