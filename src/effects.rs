//! K65 PLUS onboard effect descriptors (resource 0x6D60). See docs/DESIGN.md §2 table.
//!
//! OWNER: foundation. Shared interface.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKind {
    Watercolor,
    SpiralRainbow,
    Rain,
    RainbowWave,
    TypeKey,
    TypeRipple,
    ColorShift,
    ColorPulse,
    ColorWave,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Speed {
    Slow,
    Medium,
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parse "#RRGGBB" (case-insensitive).
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        Some(Self(channel(0)?, channel(2)?, channel(4)?))
    }
    /// "#RRGGBB" uppercase.
    pub fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    pub kind: EffectKind,
    /// None for Static.
    pub speed: Option<Speed>,
    /// Some only for Static.
    pub color: Option<Rgb>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Known(Effect),
    Unknown { id: u16, raw: Vec<u8> },
}

/// Data length the K65 firmware expects for an effect write (data padded with zeros).
pub const K65_DATA_LEN: usize = 93;

/// Per-effect table: (kind, id, value, label, shortcut). Facts from DESIGN §2.
const TABLE: [(EffectKind, u16, &str, &str, &str); 10] = [
    (
        EffectKind::Watercolor,
        0x0022,
        "watercolor",
        "Watercolor",
        "FN+1",
    ),
    (
        EffectKind::SpiralRainbow,
        0xAB87,
        "spiral-rainbow",
        "Spiral Rainbow",
        "FN+2",
    ),
    (EffectKind::Rain, 0xA07E, "rain", "Rain", "FN+3"),
    (
        EffectKind::RainbowWave,
        0xB94C,
        "rainbow-wave",
        "Rainbow Wave",
        "FN+4",
    ),
    (
        EffectKind::TypeKey,
        0xB1F9,
        "type-key",
        "Type Lighting – Key",
        "FN+5",
    ),
    (
        EffectKind::TypeRipple,
        0x09A2,
        "type-ripple",
        "Type Lighting – Ripple",
        "FN+6",
    ),
    (
        EffectKind::ColorShift,
        0xA5FA,
        "color-shift",
        "Color Shift",
        "FN+7",
    ),
    (
        EffectKind::ColorPulse,
        0xAD4F,
        "color-pulse",
        "Color Pulse",
        "FN+8",
    ),
    (
        EffectKind::ColorWave,
        0x7BFF,
        "color-wave",
        "Color Wave",
        "FN+9",
    ),
    (EffectKind::Static, 0x207E, "static", "Static", "FN+0"),
];

impl EffectKind {
    pub const ALL: [EffectKind; 10] = [
        EffectKind::Watercolor,
        EffectKind::SpiralRainbow,
        EffectKind::Rain,
        EffectKind::RainbowWave,
        EffectKind::TypeKey,
        EffectKind::TypeRipple,
        EffectKind::ColorShift,
        EffectKind::ColorPulse,
        EffectKind::ColorWave,
        EffectKind::Static,
    ];
    fn row(self) -> &'static (EffectKind, u16, &'static str, &'static str, &'static str) {
        TABLE
            .iter()
            .find(|row| row.0 == self)
            .expect("every effect kind has a table row")
    }
    pub fn id(self) -> u16 {
        self.row().1
    }
    pub fn label(self) -> &'static str {
        self.row().3
    }
    /// e.g. "FN+3"
    pub fn shortcut(self) -> &'static str {
        self.row().4
    }
    pub fn has_speed(self) -> bool {
        self != EffectKind::Static
    }
    pub fn has_color(self) -> bool {
        self == EffectKind::Static
    }
    /// Stable action/choice value, e.g. "rain".
    pub fn value(self) -> &'static str {
        self.row().2
    }
    pub fn from_value(value: &str) -> Option<Self> {
        TABLE.iter().find(|row| row.2 == value).map(|row| row.0)
    }
    pub fn from_id(id: u16) -> Option<Self> {
        TABLE.iter().find(|row| row.1 == id).map(|row| row.0)
    }
}

impl Speed {
    pub const ALL: [Speed; 3] = [Speed::Slow, Speed::Medium, Speed::Fast];
    pub fn byte(self) -> u8 {
        match self {
            Speed::Slow => 3,
            Speed::Medium => 4,
            Speed::Fast => 5,
        }
    }
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            3 => Some(Speed::Slow),
            4 => Some(Speed::Medium),
            5 => Some(Speed::Fast),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Speed::Slow => "Slow",
            Speed::Medium => "Medium",
            Speed::Fast => "Fast",
        }
    }
    pub fn value(self) -> &'static str {
        match self {
            Speed::Slow => "slow",
            Speed::Medium => "medium",
            Speed::Fast => "fast",
        }
    }
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|speed| speed.value() == value)
    }
}

impl Effect {
    /// e.g. "Rain (slow)" or "Static #FF0000".
    pub fn describe(&self) -> String {
        match (self.speed, self.color) {
            (_, Some(color)) => format!("{} {}", self.kind.label(), color.hex()),
            (Some(speed), None) => format!("{} ({})", self.kind.label(), speed.value()),
            (None, None) => self.kind.label().to_string(),
        }
    }
}

impl Decoded {
    /// "Rain (slow)" or "Custom (0xNNNN)".
    pub fn describe(&self) -> String {
        match self {
            Decoded::Known(effect) => effect.describe(),
            Decoded::Unknown { id, .. } => format!("Custom (0x{id:04X})"),
        }
    }
}

/// The exact bytes to write: (header, data) where data is NOT yet padded.
/// The resource writer pads data to 93 bytes and uses data length as the declared length.
pub fn encode_k65(effect: &Effect) -> (Vec<u8>, Vec<u8>) {
    let [low, high] = effect.kind.id().to_le_bytes();
    let speed = effect.speed.unwrap_or(Speed::Medium).byte();
    match effect.kind {
        EffectKind::Watercolor => (
            vec![low, high, 0x03, speed],
            vec![0x00, 0x00, 0x01, 0xff, 0xff, 0xff, 0xff],
        ),
        EffectKind::SpiralRainbow | EffectKind::RainbowWave => (
            vec![low, high, 0x00, speed, direction(effect.kind)],
            Vec::new(),
        ),
        EffectKind::Rain | EffectKind::ColorWave => (
            vec![low, high, 0x02, speed, direction(effect.kind)],
            Vec::new(),
        ),
        EffectKind::TypeKey
        | EffectKind::TypeRipple
        | EffectKind::ColorShift
        | EffectKind::ColorPulse => (vec![low, high, 0x02, speed], Vec::new()),
        EffectKind::Static => {
            let Rgb(red, green, blue) = effect.color.unwrap_or(Rgb(0xff, 0xff, 0xff));
            (
                vec![low, high, 0x01],
                vec![0x00, 0x00, 0x00, 0x01, 0xff, blue, green, red],
            )
        }
    }
}

fn direction(kind: EffectKind) -> u8 {
    match kind {
        EffectKind::SpiralRainbow => 0x06,
        EffectKind::Rain => 0x01,
        EffectKind::RainbowWave | EffectKind::ColorWave => 0x04,
        _ => 0x00,
    }
}

/// `(declared_len, bytes)` for `Bragi::write_resource` on 0x6D60: header followed by data
/// zero-padded to [`K65_DATA_LEN`]; the declared length is the padded data length (header excluded).
pub fn encode_k65_write(effect: &Effect) -> (u32, Vec<u8>) {
    let (mut bytes, mut data) = encode_k65(effect);
    data.resize(K65_DATA_LEN, 0);
    bytes.extend_from_slice(&data);
    (K65_DATA_LEN as u32, bytes)
}

/// Decode stored bytes read back from 0x6D60.
pub fn decode(bytes: &[u8]) -> Decoded {
    let unknown = |id| Decoded::Unknown {
        id,
        raw: bytes.to_vec(),
    };
    let [low, high, ..] = *bytes else {
        return unknown(bytes.first().copied().map_or(0, u16::from));
    };
    let id = u16::from_le_bytes([low, high]);
    let Some(kind) = EffectKind::from_id(id) else {
        return unknown(id);
    };
    if kind == EffectKind::Static {
        // [id, colour mode, speed, direction, 0, count, (ff B G R)...]
        return match bytes.get(7..11) {
            Some(&[_, blue, green, red]) if bytes.get(6).copied().unwrap_or(0) >= 1 => {
                Decoded::Known(Effect {
                    kind,
                    speed: None,
                    color: Some(Rgb(red, green, blue)),
                })
            }
            _ => unknown(id),
        };
    }
    match bytes.get(3).copied().and_then(Speed::from_byte) {
        Some(speed) => Decoded::Known(Effect {
            kind,
            speed: Some(speed),
            color: None,
        }),
        None => unknown(id),
    }
}

/// Default speed and colour when switching to `kind`.
pub fn default_effect(kind: EffectKind) -> Effect {
    if kind.has_color() {
        Effect {
            kind,
            speed: None,
            color: Some(Rgb(0xff, 0xff, 0xff)),
        }
    } else {
        Effect {
            kind,
            speed: Some(Speed::Medium),
            color: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        text.split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).unwrap())
            .collect()
    }

    #[test]
    fn colours_parse_and_format() {
        assert_eq!(Rgb::parse("#ff8000"), Some(Rgb(255, 128, 0)));
        assert_eq!(Rgb::parse("#FF8000"), Some(Rgb(255, 128, 0)));
        assert_eq!(Rgb(255, 128, 0).hex(), "#FF8000");
        for bad in ["ff8000", "#ff800", "#ff80000", "#gg0000", "", "#"] {
            assert_eq!(Rgb::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn encode_matches_the_design_table() {
        let speed = |kind| Effect {
            kind,
            speed: Some(Speed::Slow),
            color: None,
        };
        let cases: [(EffectKind, &str, &str); 9] = [
            (
                EffectKind::Watercolor,
                "22 00 03 03",
                "00 00 01 ff ff ff ff",
            ),
            (EffectKind::SpiralRainbow, "87 ab 00 03 06", ""),
            (EffectKind::Rain, "7e a0 02 03 01", ""),
            (EffectKind::RainbowWave, "4c b9 00 03 04", ""),
            (EffectKind::TypeKey, "f9 b1 02 03", ""),
            (EffectKind::TypeRipple, "a2 09 02 03", ""),
            (EffectKind::ColorShift, "fa a5 02 03", ""),
            (EffectKind::ColorPulse, "4f ad 02 03", ""),
            (EffectKind::ColorWave, "ff 7b 02 03 04", ""),
        ];
        for (kind, header, data) in cases {
            assert_eq!(
                encode_k65(&speed(kind)),
                (hex(header), hex(data)),
                "{kind:?}"
            );
        }
        let medium = Effect {
            kind: EffectKind::Rain,
            speed: Some(Speed::Medium),
            color: None,
        };
        assert_eq!(encode_k65(&medium).0, hex("7e a0 02 04 01"));
        let fast = Effect {
            kind: EffectKind::Rain,
            speed: Some(Speed::Fast),
            color: None,
        };
        assert_eq!(encode_k65(&fast).0, hex("7e a0 02 05 01"));
    }

    #[test]
    fn static_red_write_matches_hardware_capture() {
        let red = Effect {
            kind: EffectKind::Static,
            speed: None,
            color: Some(Rgb(0xff, 0, 0)),
        };
        assert_eq!(
            encode_k65(&red),
            (hex("7e 20 01"), hex("00 00 00 01 ff 00 00 ff"))
        );
        let (declared, bytes) = encode_k65_write(&red);
        assert_eq!(declared, 93);
        assert_eq!(bytes.len(), 3 + 93);
        assert_eq!(
            &bytes[..11],
            hex("7e 20 01 00 00 00 01 ff 00 00 ff").as_slice()
        );
        assert!(bytes[11..].iter().all(|byte| *byte == 0));
        // The written form decodes back to the same effect.
        assert_eq!(decode(&bytes), Decoded::Known(red));
        let blue = Effect {
            kind: EffectKind::Static,
            speed: None,
            color: Some(Rgb(0, 0, 0xff)),
        };
        assert_eq!(encode_k65(&blue).1, hex("00 00 00 01 ff ff 00 00"));
    }

    #[test]
    fn decodes_hardware_readbacks() {
        assert_eq!(
            decode(&hex("7e a0 02 03")),
            Decoded::Known(Effect {
                kind: EffectKind::Rain,
                speed: Some(Speed::Slow),
                color: None
            })
        );
        assert_eq!(
            decode(&hex("22 00 03 04 00 00 01 ff ff ff ff")),
            Decoded::Known(Effect {
                kind: EffectKind::Watercolor,
                speed: Some(Speed::Medium),
                color: None
            })
        );
        assert_eq!(
            decode(&hex("7e 20 01 00 00 00 01 ff 00 00 ff")),
            Decoded::Known(Effect {
                kind: EffectKind::Static,
                speed: None,
                color: Some(Rgb(0xff, 0, 0))
            })
        );
        assert_eq!(decode(&hex("7e a0 02 03")).describe(), "Rain (slow)");
    }

    #[test]
    fn every_encoded_effect_round_trips() {
        for kind in EffectKind::ALL {
            for speed in Speed::ALL {
                let effect = if kind.has_color() {
                    Effect {
                        kind,
                        speed: None,
                        color: Some(Rgb(1, 2, 3)),
                    }
                } else {
                    Effect {
                        kind,
                        speed: Some(speed),
                        color: None,
                    }
                };
                assert_eq!(decode(&encode_k65_write(&effect).1), Decoded::Known(effect));
            }
        }
    }

    #[test]
    fn unknown_or_truncated_bytes_decode_as_custom() {
        assert_eq!(
            decode(&hex("34 12 01 02")),
            Decoded::Unknown {
                id: 0x1234,
                raw: hex("34 12 01 02")
            }
        );
        assert_eq!(decode(&hex("34 12 01 02")).describe(), "Custom (0x1234)");
        assert!(matches!(decode(&[]), Decoded::Unknown { id: 0, .. }));
        assert!(matches!(decode(&[0x7e]), Decoded::Unknown { .. }));
        assert!(matches!(
            decode(&hex("7e a0 02")),
            Decoded::Unknown { id: 0xA07E, .. }
        ));
        assert!(matches!(
            decode(&hex("7e 20 01 00 00 00 01 ff")),
            Decoded::Unknown { id: 0x207E, .. }
        ));
    }

    #[test]
    fn metadata_is_consistent() {
        let mut ids: Vec<u16> = EffectKind::ALL.iter().map(|kind| kind.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 10);
        for (index, kind) in EffectKind::ALL.into_iter().enumerate() {
            assert_eq!(EffectKind::from_value(kind.value()), Some(kind));
            assert_eq!(kind.shortcut(), format!("FN+{}", (index + 1) % 10));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.value())
            );
            assert_eq!(kind.has_speed(), !kind.has_color());
            let default = default_effect(kind);
            assert_eq!(default.speed.is_some(), kind.has_speed());
            assert_eq!(default.color.is_some(), kind.has_color());
        }
        assert_eq!(EffectKind::from_value("nope"), None);
        assert_eq!(Speed::from_byte(4), Some(Speed::Medium));
        assert_eq!(Speed::from_byte(0), None);
        assert_eq!(Speed::from_value("fast"), Some(Speed::Fast));
    }
}
