//! Per-key lighting frames for resource 0x22: `12 00` + R,G,B per LED (DESIGN §2).

use crate::config::LightingConfig;
use crate::devices::Model;
use crate::effects::Rgb;
use crate::layouts::{self, Layout};

/// Colour of LEDs nothing else sets (enabling live lighting must not blank the board).
pub const DEFAULT_BASE: Rgb = Rgb(255, 255, 255);
/// Frame data prefix.
pub const PREFIX: [u8; 2] = [0x12, 0x00];

/// Compose the frame data: base colour, then the "keys" zone, then LED zones, then per-key colours.
pub fn compose(
    model: Model,
    layout: &Layout,
    lighting: &LightingConfig,
    led_count: usize,
) -> Vec<u8> {
    let mut leds = vec![lighting.base.unwrap_or(DEFAULT_BASE); led_count];
    let set = |leds: &mut Vec<Rgb>, led: u16, colour: Rgb| {
        if let Some(slot) = leds.get_mut(usize::from(led)) {
            *slot = colour;
        }
    };
    if let Some(colour) = lighting.zones.get("keys") {
        for led in layout.keys.iter().filter_map(|key| key.led) {
            set(&mut leds, led, *colour);
        }
    }
    for zone in layouts::zones(model) {
        if let Some(colour) = lighting.zones.get(zone.id) {
            for led in zone.leds {
                set(&mut leds, *led, *colour);
            }
        }
    }
    for (id, colour) in &lighting.keys {
        if let Some(led) = layouts::key_by_id(layout, id).and_then(|key| key.led) {
            set(&mut leds, led, *colour);
        }
    }
    if model == Model::K65PlusWireless {
        // The spacebar also lights LED 0; LED 1 stays dark (research §9.1, unverified).
        if let Some(space) = layouts::key_by_id(layout, "space").and_then(|key| key.led)
            && let Some(colour) = leds.get(usize::from(space)).copied()
        {
            set(&mut leds, 0, colour);
        }
        set(&mut leds, 1, Rgb(0, 0, 0));
    }
    let mut frame = Vec::with_capacity(PREFIX.len() + led_count * 3);
    frame.extend_from_slice(&PREFIX);
    for Rgb(r, g, b) in leds {
        frame.extend_from_slice(&[r, g, b]);
    }
    frame
}

/// Resolve a physical key's colour using the exact frame compositor. Non-LED controls are not
/// paintable and have no colour, even when a base or keys zone is configured.
pub fn key_color(
    model: Model,
    layout: &Layout,
    key: &layouts::Key,
    lighting: &LightingConfig,
) -> Option<Rgb> {
    let led = usize::from(key.led?);
    let count = model.capabilities(crate::devices::LinkKind::Usb).led_count;
    let frame = compose(model, layout, lighting, count);
    let rgb = frame.get(PREFIX.len() + led * 3..PREFIX.len() + led * 3 + 3)?;
    Some(Rgb(rgb[0], rgb[1], rgb[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::PhysicalLayout;

    fn led_colour(frame: &[u8], led: u16) -> Rgb {
        let at = 2 + usize::from(led) * 3;
        Rgb(frame[at], frame[at + 1], frame[at + 2])
    }

    #[test]
    fn k100_frame_layers_base_zones_and_keys() {
        let layout = layouts::layout(Model::K100Rgb, PhysicalLayout::Ansi);
        let mut lighting = LightingConfig {
            enabled: true,
            base: Some(Rgb(1, 2, 3)),
            ..LightingConfig::default()
        };
        lighting.zones.insert("keys".into(), Rgb(10, 10, 10));
        lighting.zones.insert("logo".into(), Rgb(0, 0, 255));
        lighting.zones.insert("ring".into(), Rgb(0, 255, 0));
        lighting.keys.insert("a".into(), Rgb(255, 0, 0));
        lighting.keys.insert("not-a-key".into(), Rgb(9, 9, 9));
        let frame = compose(Model::K100Rgb, layout, &lighting, 193);
        assert_eq!(frame.len(), 2 + 193 * 3);
        assert_eq!(&frame[..2], &[0x12, 0x00]);
        let a = layouts::key_by_id(layout, "a").unwrap().led.unwrap();
        assert_eq!(led_colour(&frame, a), Rgb(255, 0, 0));
        let s = layouts::key_by_id(layout, "s").unwrap().led.unwrap();
        assert_eq!(led_colour(&frame, s), Rgb(10, 10, 10));
        assert_eq!(led_colour(&frame, 190), Rgb(0, 0, 255));
        assert_eq!(led_colour(&frame, 182), Rgb(0, 255, 0));
        assert_eq!(led_colour(&frame, 150), Rgb(1, 2, 3), "edge keeps the base");
    }

    #[test]
    fn default_base_and_k65_spacebar_leds() {
        let layout = layouts::layout(Model::K65PlusWireless, PhysicalLayout::Ansi);
        let mut lighting = LightingConfig::default();
        let frame = compose(Model::K65PlusWireless, layout, &lighting, 123);
        assert_eq!(frame.len(), 2 + 123 * 3);
        assert_eq!(led_colour(&frame, 10), DEFAULT_BASE);
        assert_eq!(led_colour(&frame, 1), Rgb(0, 0, 0));
        lighting.keys.insert("space".into(), Rgb(0, 0, 7));
        let frame = compose(Model::K65PlusWireless, layout, &lighting, 123);
        assert_eq!(led_colour(&frame, 44), Rgb(0, 0, 7));
        assert_eq!(led_colour(&frame, 0), Rgb(0, 0, 7));
        // Identical config gives an identical frame (writes are skipped on no change).
        assert_eq!(
            frame,
            compose(Model::K65PlusWireless, layout, &lighting, 123)
        );
    }
}
