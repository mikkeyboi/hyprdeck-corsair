//! IF2 (hidraw event node) report parsing [HW]: `01 02 <bitmap>` key state and
//! `01 05 60 00 <i32 LE>` dial rotation. Only sent while the keyboard is in software mode.

use crate::layouts::{self, Layout};
use std::collections::BTreeSet;

/// Bragi key index of the K65 PLUS dial press [HW].
pub const DIAL_PRESS_INDEX: u16 = 137;
/// IF2 report size.
pub const REPORT_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Set bits of the key bitmap (Bragi key indices currently held).
    Keys(BTreeSet<u16>),
    /// Dial rotation: +n clockwise, −n counter-clockwise.
    Dial(i32),
    Other,
}

/// Parse one IF2 report as read from the hidraw node.
pub fn parse(report: &[u8]) -> Report {
    match report {
        [0x01, 0x02, bitmap @ ..] => Report::Keys(
            bitmap
                .iter()
                .enumerate()
                .flat_map(|(byte_index, byte)| {
                    (0..8u16)
                        .filter(move |bit| byte & (1 << bit) != 0)
                        .map(move |bit| byte_index as u16 * 8 + bit)
                })
                .collect(),
        ),
        [0x01, 0x05, 0x60, 0x00, a, b, c, d, ..] => {
            Report::Dial(i32::from_le_bytes([*a, *b, *c, *d]))
        }
        _ => Report::Other,
    }
}

/// Edge detection over successive key bitmaps.
#[derive(Debug, Default)]
pub struct KeyTracker {
    held: BTreeSet<u16>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct KeyChange {
    pub pressed: Vec<u16>,
    pub released: Vec<u16>,
}

impl KeyTracker {
    pub fn update(&mut self, now: BTreeSet<u16>) -> KeyChange {
        let change = KeyChange {
            pressed: now.difference(&self.held).copied().collect(),
            released: self.held.difference(&now).copied().collect(),
        };
        self.held = now;
        change
    }
    pub fn held(&self) -> &BTreeSet<u16> {
        &self.held
    }
    pub fn clear(&mut self) {
        self.held.clear();
    }
}

/// Layout key ids for a set of Bragi key indices (unknown indices are skipped).
pub fn ids_for_indices(layout: &Layout, indices: &BTreeSet<u16>) -> Vec<String> {
    indices
        .iter()
        .filter_map(|index| layouts::key_by_index(layout, *index))
        .map(|key| key.id.to_string())
        .collect()
}

/// Layout key ids for a set of evdev keycodes (unknown codes are skipped).
pub fn ids_for_codes(layout: &Layout, codes: &BTreeSet<u16>) -> Vec<String> {
    codes
        .iter()
        .filter_map(|code| layouts::key_by_evdev(layout, *code))
        .map(|key| key.id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Model, PhysicalLayout};

    fn report(bytes: &[u8]) -> Vec<u8> {
        let mut out = bytes.to_vec();
        out.resize(REPORT_LEN, 0);
        out
    }

    #[test]
    fn parses_dial_rotation() {
        assert_eq!(
            parse(&report(&[0x01, 0x05, 0x60, 0x00, 0x01, 0, 0, 0])),
            Report::Dial(1)
        );
        assert_eq!(
            parse(&report(&[0x01, 0x05, 0x60, 0x00, 0xff, 0xff, 0xff, 0xff])),
            Report::Dial(-1)
        );
        assert_eq!(
            parse(&report(&[0x01, 0x05, 0x60, 0x00, 0xfd, 0xff, 0xff, 0xff])),
            Report::Dial(-3)
        );
        // Other notification kinds and truncated reports are ignored.
        assert_eq!(parse(&report(&[0x01, 0x05, 0x61, 0x00, 1])), Report::Other);
        assert_eq!(parse(&[0x01, 0x05, 0x60, 0x00, 1]), Report::Other);
        assert_eq!(parse(&report(&[0x00, 0x02, 0xff])), Report::Other);
        assert_eq!(parse(&[]), Report::Other);
    }

    #[test]
    fn parses_key_bitmap() {
        // a = 4 (byte 0 bit 4), space = 44 (byte 5 bit 4), dial press = 137 (byte 17 bit 1).
        let mut bytes = vec![0x01, 0x02];
        let mut bitmap = [0u8; 20];
        bitmap[0] = 1 << 4;
        bitmap[5] = 1 << 4;
        bitmap[17] = 1 << 1;
        bytes.extend_from_slice(&bitmap);
        let Report::Keys(keys) = parse(&report(&bytes)) else {
            panic!("not a key report")
        };
        assert_eq!(keys.into_iter().collect::<Vec<_>>(), vec![4, 44, 137]);
        assert_eq!(parse(&report(&[0x01, 0x02])), Report::Keys(BTreeSet::new()));
    }

    #[test]
    fn tracks_edges_and_maps_ids() {
        let mut tracker = KeyTracker::default();
        let change = tracker.update([4, 137].into());
        assert_eq!(change.pressed, vec![4, 137]);
        assert!(change.released.is_empty());
        let change = tracker.update([137].into());
        assert_eq!(change.released, vec![4]);
        assert!(change.pressed.is_empty());
        let layout = layouts::layout(Model::K65PlusWireless, PhysicalLayout::Ansi);
        assert_eq!(
            ids_for_indices(layout, &[4, 137, 9999].into()),
            vec!["a".to_string(), "dial".to_string()]
        );
        assert_eq!(ids_for_codes(layout, &[30].into()), vec!["a".to_string()]);
    }
}
