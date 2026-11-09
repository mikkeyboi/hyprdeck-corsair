//! Key geometry, Bragi key indices, Linux keycodes and LED indices for supported boards,
//! plus the canonical key-name table used by remaps/macros. See docs/DESIGN.md §2 (events), §6.
//!
//! OWNER: layouts. Shared interface. Data must be authored from public facts (HID usage tables,
//! linux/input-event-codes.h, physical board photos/manuals) — never copied from GPL data files.

mod boards;
pub mod keynames;

use std::sync::LazyLock;

use crate::devices::{Model, PhysicalLayout};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Key {
    /// Stable id, `valid_id` compatible: lowercase ascii, digits, '-', '_' (e.g. "esc", "f1", "l-shift").
    pub id: &'static str,
    /// Short label drawn on the key (≤ 32 bytes), e.g. "Esc", "⇧", "Enter".
    pub label: &'static str,
    /// Geometry in key units (1u = one standard key), origin top-left.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Bragi key index (IF2 bitmap bit). Equals the HID keyboard usage for normal keys;
    /// modifiers 105..=112; Fn 122; dial press 137 (K65). None for keys without a bit.
    pub index: Option<u16>,
    /// Linux input keycode (KEY_*), None for Fn / dial / LED-only elements.
    pub evdev: Option<u16>,
    /// Index into the direct-lighting frame (resource 0x22), None when not lit/unknown.
    pub led: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub name: &'static str,
    pub width: f32,
    pub height: f32,
    pub keys: &'static [Key],
}

/// LED-only zones for live lighting (K100 RGB: "edge", "logo", "ring"; "keys" covers all keys).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    pub id: &'static str,
    pub label: &'static str,
    pub leds: &'static [u16],
}

/// Layout for a model; ABNT/JIS fall back to ISO/ANSI respectively.
pub fn layout(model: Model, physical: PhysicalLayout) -> &'static Layout {
    static K65_ANSI: LazyLock<Layout> = LazyLock::new(|| boards::k65_plus(false));
    static K65_ISO: LazyLock<Layout> = LazyLock::new(|| boards::k65_plus(true));
    static K100_ANSI: LazyLock<Layout> = LazyLock::new(|| boards::k100_rgb(false));
    static K100_ISO: LazyLock<Layout> = LazyLock::new(|| boards::k100_rgb(true));
    let iso = matches!(physical, PhysicalLayout::Iso | PhysicalLayout::Abnt);
    match (model, iso) {
        (Model::K65PlusWireless, false) => &K65_ANSI,
        (Model::K65PlusWireless, true) => &K65_ISO,
        (Model::K100Rgb, false) => &K100_ANSI,
        (Model::K100Rgb, true) => &K100_ISO,
    }
}

/// K100 RGB LightEdge: top 138..=159 (left→right), left 160..=170 and right 171..=181 (top→bottom).
const K100_EDGE: [u16; 44] = {
    let mut leds = [0u16; 44];
    let mut i = 0;
    while i < 44 {
        leds[i] = 138 + i as u16;
        i += 1;
    }
    leds
};
/// Control-wheel ring 182..=189 (ckb-next's angles start at 67.5° and go clockwise).
const K100_RING: [u16; 8] = [182, 183, 184, 185, 186, 187, 188, 189];
/// Logo left / centre / right.
const K100_LOGO: [u16; 3] = [190, 191, 192];

static K100_ZONES: [Zone; 3] = [
    Zone {
        id: "edge",
        label: "LightEdge",
        leds: &K100_EDGE,
    },
    Zone {
        id: "logo",
        label: "Logo",
        leds: &K100_LOGO,
    },
    Zone {
        id: "ring",
        label: "Wheel ring",
        leds: &K100_RING,
    },
];

/// Extra LED zones for the model (empty for K65 PLUS).
pub fn zones(model: Model) -> &'static [Zone] {
    match model {
        Model::K65PlusWireless => &[],
        Model::K100Rgb => &K100_ZONES,
    }
}

pub fn key_by_id<'a>(layout: &'a Layout, id: &str) -> Option<&'a Key> {
    layout.keys.iter().find(|k| k.id == id)
}
pub fn key_by_index(layout: &Layout, index: u16) -> Option<&Key> {
    layout.keys.iter().find(|k| k.index == Some(index))
}
pub fn key_by_evdev(layout: &Layout, code: u16) -> Option<&Key> {
    layout.keys.iter().find(|k| k.evdev == Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(Model, PhysicalLayout); 4] = [
        (Model::K65PlusWireless, PhysicalLayout::Ansi),
        (Model::K65PlusWireless, PhysicalLayout::Iso),
        (Model::K100Rgb, PhysicalLayout::Ansi),
        (Model::K100Rgb, PhysicalLayout::Iso),
    ];

    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    }

    fn overlaps(a: &Key, b: &Key) -> bool {
        const EPS: f32 = 1e-4;
        a.x + EPS < b.x + b.w
            && b.x + EPS < a.x + a.w
            && a.y + EPS < b.y + b.h
            && b.y + EPS < a.y + a.h
    }

    #[test]
    fn layout_invariants() {
        for (model, physical) in ALL {
            let layout = layout(model, physical);
            let name = layout.name;
            assert!(
                !layout.keys.is_empty() && layout.keys.len() <= 512,
                "{name}"
            );
            assert!(layout.width.is_finite() && layout.width > 0.0, "{name}");
            assert!(layout.height.is_finite() && layout.height > 0.0, "{name}");
            for (i, key) in layout.keys.iter().enumerate() {
                let id = key.id;
                assert!(valid_id(id), "{name}: bad id {id:?}");
                assert!(
                    !key.label.is_empty() && key.label.len() <= 32,
                    "{name}: {id}"
                );
                for v in [key.x, key.y, key.w, key.h] {
                    assert!(v.is_finite(), "{name}: {id}");
                }
                assert!(key.w > 0.0 && key.h > 0.0, "{name}: {id}");
                assert!(key.x >= 0.0 && key.y >= 0.0, "{name}: {id}");
                assert!(
                    key.x + key.w <= layout.width + 1e-4,
                    "{name}: {id} past width"
                );
                assert!(
                    key.y + key.h <= layout.height + 1e-4,
                    "{name}: {id} past height"
                );
                if let Some(code) = key.evdev {
                    assert!(
                        keynames::name(code).is_some(),
                        "{name}: {id} evdev {code} has no key name"
                    );
                }
                for other in &layout.keys[i + 1..] {
                    assert_ne!(key.id, other.id, "{name}: duplicate id");
                    if key.index.is_some() {
                        assert_ne!(key.index, other.index, "{name}: {id}/{} index", other.id);
                    }
                    if key.evdev.is_some() {
                        assert_ne!(key.evdev, other.evdev, "{name}: {id}/{} evdev", other.id);
                    }
                    if key.led.is_some() {
                        assert_ne!(key.led, other.led, "{name}: {id}/{} led", other.id);
                    }
                    assert!(!overlaps(key, other), "{name}: {id} overlaps {}", other.id);
                }
            }
        }
    }

    #[test]
    fn lookups_resolve() {
        for (model, physical) in ALL {
            let layout = layout(model, physical);
            for key in layout.keys {
                assert_eq!(key_by_id(layout, key.id), Some(key));
                if let Some(index) = key.index {
                    assert_eq!(key_by_index(layout, index), Some(key));
                }
                if let Some(code) = key.evdev {
                    assert_eq!(key_by_evdev(layout, code), Some(key));
                }
            }
        }
    }

    fn check(layout: &Layout, id: &str, index: Option<u16>, evdev: Option<u16>, led: Option<u16>) {
        let key = key_by_id(layout, id).unwrap_or_else(|| panic!("{}: no {id}", layout.name));
        assert_eq!(
            (key.index, key.evdev, key.led),
            (index, evdev, led),
            "{}: {id}",
            layout.name
        );
    }

    #[test]
    fn k65_plus_hardware_verified_indices() {
        for physical in [PhysicalLayout::Ansi, PhysicalLayout::Iso] {
            let l = layout(Model::K65PlusWireless, physical);
            check(l, "a", Some(4), Some(30), Some(4));
            check(l, "1", Some(30), Some(2), Some(30));
            check(l, "space", Some(44), Some(57), Some(44));
            check(l, "f6", Some(63), Some(64), Some(63));
            check(l, "fn", Some(122), None, Some(122));
            check(l, "dial", Some(137), None, None);
            check(l, "l-ctrl", Some(105), Some(29), Some(105));
            assert!(key_by_id(l, "r-super").is_none());
        }
    }

    #[test]
    fn k65_plus_shape() {
        let ansi = layout(Model::K65PlusWireless, PhysicalLayout::Ansi);
        let iso = layout(Model::K65PlusWireless, PhysicalLayout::Iso);
        // 81 keys + dial on ANSI; ISO swaps \ for # and adds the 102nd key.
        assert_eq!(ansi.keys.len(), 82);
        assert_eq!(iso.keys.len(), 83);
        assert!(key_by_id(ansi, "backslash").is_some());
        assert!(key_by_id(ansi, "hash").is_none());
        assert!(key_by_id(iso, "backslash").is_none());
        check(iso, "hash", Some(50), Some(43), Some(50));
        check(iso, "iso-backslash", Some(100), Some(86), Some(100));
        // Fn-layer keys are not physical keys on the 75% board.
        for id in [
            "insert",
            "end",
            "printscreen",
            "scrolllock",
            "pause",
            "menu",
            "numlock",
        ] {
            assert!(key_by_id(ansi, id).is_none(), "{id}");
        }
        // Dial top-right, Delete at the end of the function row, nav column on the right edge.
        let dial = key_by_id(ansi, "dial").unwrap();
        assert_eq!((dial.x + dial.w, dial.y), (ansi.width, 0.0));
        let del = key_by_id(ansi, "delete").unwrap();
        assert_eq!((del.y, del.index), (0.0, Some(76)));
        for id in ["home", "pageup", "pagedown", "right"] {
            let k = key_by_id(ansi, id).unwrap();
            assert_eq!(k.x + k.w, ansi.width, "{id}");
        }
        let right = key_by_id(ansi, "right").unwrap();
        assert_eq!(right.y + right.h, ansi.height);
    }

    #[test]
    fn k100_rgb_indices() {
        for physical in [PhysicalLayout::Ansi, PhysicalLayout::Iso] {
            let l = layout(Model::K100Rgb, physical);
            for (i, id) in ["g1", "g2", "g3", "g4", "g5", "g6"].iter().enumerate() {
                let index = 131 + i as u16;
                check(l, id, Some(index), None, Some(index));
            }
            check(l, "profile", Some(128), None, Some(128));
            check(l, "win-lock", Some(114), None, Some(114));
            check(l, "wheel", Some(137), None, Some(137));
            check(l, "volume-roller", None, None, None);
            check(l, "mute", Some(102), Some(113), Some(102));
            check(l, "stopcd", Some(123), Some(166), Some(123));
            check(l, "playpause", Some(124), Some(164), Some(124));
            check(l, "nextsong", Some(125), Some(163), Some(125));
            check(l, "previoussong", Some(126), Some(165), Some(126));
            check(l, "esc", Some(41), Some(1), Some(41));
            check(l, "f12", Some(69), Some(88), Some(69));
            check(l, "kp-enter", Some(88), Some(96), Some(88));
            check(l, "kp0", Some(98), Some(82), Some(98));
            check(l, "menu", Some(101), Some(127), Some(101));
            check(l, "r-ctrl", Some(109), Some(97), Some(109));
            check(l, "fn", Some(122), None, Some(122));
            assert!(key_by_id(l, "dial").is_none());
            assert!(l.keys.iter().all(|k| k.led.is_none_or(|led| led < 193)));
        }
        let ansi = layout(Model::K100Rgb, PhysicalLayout::Ansi);
        let iso = layout(Model::K100Rgb, PhysicalLayout::Iso);
        // 104 ANSI keys + 6 G + profile, wheel, Win-lock, mute, roller, 4 media.
        assert_eq!(ansi.keys.len(), 119);
        assert_eq!(iso.keys.len(), 120);
        check(iso, "hash", Some(50), Some(43), Some(50));
        check(iso, "iso-backslash", Some(100), Some(86), Some(100));
    }

    #[test]
    fn fallbacks() {
        for model in [Model::K65PlusWireless, Model::K100Rgb] {
            assert_eq!(
                layout(model, PhysicalLayout::Abnt),
                layout(model, PhysicalLayout::Iso)
            );
            assert_eq!(
                layout(model, PhysicalLayout::Jis),
                layout(model, PhysicalLayout::Ansi)
            );
            assert!(std::ptr::eq(
                layout(model, PhysicalLayout::Ansi),
                layout(model, PhysicalLayout::Ansi)
            ));
        }
    }

    #[test]
    fn k100_zones() {
        assert!(zones(Model::K65PlusWireless).is_empty());
        let zones = zones(Model::K100Rgb);
        let ids: Vec<_> = zones.iter().map(|z| z.id).collect();
        assert_eq!(ids, ["edge", "logo", "ring"]);
        let edge = &zones[0];
        assert_eq!(edge.leds.len(), 44);
        assert_eq!((edge.leds[0], edge.leds[43]), (138, 181));
        assert_eq!(zones[1].leds, [190, 191, 192]);
        assert_eq!(zones[2].leds, [182, 183, 184, 185, 186, 187, 188, 189]);
        let key_leds: Vec<u16> = [PhysicalLayout::Ansi, PhysicalLayout::Iso]
            .iter()
            .flat_map(|&p| layout(Model::K100Rgb, p).keys.iter().filter_map(|k| k.led))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for zone in zones {
            assert!(valid_id(zone.id) && zone.label.len() <= 32);
            for &led in zone.leds {
                assert!(led < 193, "{}", zone.id);
                assert!(seen.insert(led), "led {led} in two zones");
                assert!(!key_leds.contains(&led), "zone led {led} is also a key led");
            }
        }
    }
}
