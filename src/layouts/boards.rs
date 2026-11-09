//! Physical geometry of the supported boards, authored from public sources only:
//!
//! - Key list, Fn-layer facts and key positions of the K65 PLUS Wireless: Corsair's
//!   "K65 PLUS WIRELESS Manual | Quick Start Guide" page and its top-view drawings
//!   (<https://www.corsair.com/us/en/explorer/gamer/keyboards/k65-plus-wireless/>), measured by hand.
//!   The board has no dedicated Insert/End/PrtSc/ScrLk/Pause keys (they are Fn+[ / Fn+] / Fn+/ /
//!   Fn+O / Fn+P), a Delete key at the end of the function row, the rotary dial in the top-right
//!   corner, a right-hand Home/PgUp/PgDn column and a dropped arrow cluster under a 1.75u right
//!   Shift, and a bottom row Ctrl/Win/Alt (1.25u) + 6.25u space + Alt/Fn/Ctrl (1u).
//! - K100 RGB: Corsair's "K100 RGB Optical-Mechanical Gaming Keyboard" quick start guide
//!   (49-001877 AB, "Getting to know your keyboard" drawing), measured by hand: G1–G6 column on the
//!   left, profile key / iCUE control wheel / Win-lock key above it, LED indicators + logo strip in
//!   the middle, mute key + volume roller above the numpad, four media keys at function-row height,
//!   function row with three equal gaps (Esc to F12 spans the 15u main block), standard full-size
//!   main block, navigation cluster and numpad, bottom row with Fn and Menu.
//! - Bragi key indices: HID keyboard usage ids (USB HID Usage Tables, keyboard page) for normal keys;
//!   modifiers 105..=112, Fn 122, dial/wheel press 137 [hardware-verified on the K65 PLUS, see
//!   docs/hardware-evidence-k65plus.md]; K100 extras (mute 102, Win-lock 114, media 123..=126,
//!   profile 128, G1–G6 131..=136) from docs/research/k100.md §4.2/§5.1.
//! - Linux keycodes: linux/input-event-codes.h and the kernel's HID keyboard usage mapping
//!   (non-US # → KEY_BACKSLASH, non-US \ → KEY_102ND, Application → KEY_COMPOSE).
//!
//! Units are key units (1u = one standard key pitch), origin top-left. Real boards are not perfectly
//! on a 0.25u grid; small measured offsets (e.g. the K100 G-key column gap) are kept as measured.

use super::{Key, Layout};

/// (id, label, Bragi index, Linux keycode) for every key that can appear on a supported board.
type Spec = (&'static str, &'static str, Option<u16>, Option<u16>);

const fn s(id: &'static str, label: &'static str, index: u16, evdev: u16) -> Spec {
    (id, label, Some(index), Some(evdev))
}
const fn x(id: &'static str, label: &'static str, index: u16) -> Spec {
    (id, label, Some(index), None)
}

const SPECS: &[Spec] = &[
    s("esc", "Esc", 41, 1),
    s("f1", "F1", 58, 59),
    s("f2", "F2", 59, 60),
    s("f3", "F3", 60, 61),
    s("f4", "F4", 61, 62),
    s("f5", "F5", 62, 63),
    s("f6", "F6", 63, 64),
    s("f7", "F7", 64, 65),
    s("f8", "F8", 65, 66),
    s("f9", "F9", 66, 67),
    s("f10", "F10", 67, 68),
    s("f11", "F11", 68, 87),
    s("f12", "F12", 69, 88),
    s("grave", "`", 53, 41),
    s("1", "1", 30, 2),
    s("2", "2", 31, 3),
    s("3", "3", 32, 4),
    s("4", "4", 33, 5),
    s("5", "5", 34, 6),
    s("6", "6", 35, 7),
    s("7", "7", 36, 8),
    s("8", "8", 37, 9),
    s("9", "9", 38, 10),
    s("0", "0", 39, 11),
    s("minus", "-", 45, 12),
    s("equal", "=", 46, 13),
    s("backspace", "Backspace", 42, 14),
    s("tab", "Tab", 43, 15),
    s("q", "Q", 20, 16),
    s("w", "W", 26, 17),
    s("e", "E", 8, 18),
    s("r", "R", 21, 19),
    s("t", "T", 23, 20),
    s("y", "Y", 28, 21),
    s("u", "U", 24, 22),
    s("i", "I", 12, 23),
    s("o", "O", 18, 24),
    s("p", "P", 19, 25),
    s("lbracket", "[", 47, 26),
    s("rbracket", "]", 48, 27),
    s("backslash", "\\", 49, 43),
    s("capslock", "Caps Lock", 57, 58),
    s("a", "A", 4, 30),
    s("s", "S", 22, 31),
    s("d", "D", 7, 32),
    s("f", "F", 9, 33),
    s("g", "G", 10, 34),
    s("h", "H", 11, 35),
    s("j", "J", 13, 36),
    s("k", "K", 14, 37),
    s("l", "L", 15, 38),
    s("semicolon", ";", 51, 39),
    s("apostrophe", "'", 52, 40),
    // ISO: HID "Non-US # and ~" (0x32); Linux reports it as KEY_BACKSLASH.
    s("hash", "#", 50, 43),
    s("enter", "Enter", 40, 28),
    s("l-shift", "Shift", 106, 42),
    // ISO: HID "Non-US \ and |" (0x64) → KEY_102ND.
    s("iso-backslash", "\\", 100, 86),
    s("z", "Z", 29, 44),
    s("x", "X", 27, 45),
    s("c", "C", 6, 46),
    s("v", "V", 25, 47),
    s("b", "B", 5, 48),
    s("n", "N", 17, 49),
    s("m", "M", 16, 50),
    s("comma", ",", 54, 51),
    s("dot", ".", 55, 52),
    s("slash", "/", 56, 53),
    s("r-shift", "Shift", 110, 54),
    s("l-ctrl", "Ctrl", 105, 29),
    s("l-super", "Win", 108, 125),
    s("l-alt", "Alt", 107, 56),
    s("space", "Space", 44, 57),
    s("r-alt", "Alt", 111, 100),
    s("r-super", "Win", 112, 126),
    x("fn", "Fn", 122),
    // HID "Application" (0x65) → KEY_COMPOSE.
    s("menu", "Menu", 101, 127),
    s("r-ctrl", "Ctrl", 109, 97),
    s("printscreen", "PrtSc", 70, 99),
    s("scrolllock", "ScrLk", 71, 70),
    s("pause", "Pause", 72, 119),
    s("insert", "Ins", 73, 110),
    s("home", "Home", 74, 102),
    s("pageup", "PgUp", 75, 104),
    s("delete", "Del", 76, 111),
    s("end", "End", 77, 107),
    s("pagedown", "PgDn", 78, 109),
    s("right", "→", 79, 106),
    s("left", "←", 80, 105),
    s("down", "↓", 81, 108),
    s("up", "↑", 82, 103),
    s("numlock", "Num", 83, 69),
    s("kp-slash", "/", 84, 98),
    s("kp-asterisk", "*", 85, 55),
    s("kp-minus", "-", 86, 74),
    s("kp-plus", "+", 87, 78),
    s("kp-enter", "Enter", 88, 96),
    s("kp1", "1", 89, 79),
    s("kp2", "2", 90, 80),
    s("kp3", "3", 91, 81),
    s("kp4", "4", 92, 75),
    s("kp5", "5", 93, 76),
    s("kp6", "6", 94, 77),
    s("kp7", "7", 95, 71),
    s("kp8", "8", 96, 72),
    s("kp9", "9", 97, 73),
    s("kp0", "0", 98, 82),
    s("kp-dot", ".", 99, 83),
    // K100 RGB extras (docs/research/k100.md §4.2, §5.1).
    s("mute", "Mute", 102, 113),
    x("win-lock", "Win Lock", 114),
    s("stopcd", "Stop", 123, 166),
    s("playpause", "Play/Pause", 124, 164),
    s("nextsong", "Next", 125, 163),
    s("previoussong", "Prev", 126, 165),
    x("profile", "Profile", 128),
    x("g1", "G1", 131),
    x("g2", "G2", 132),
    x("g3", "G3", 133),
    x("g4", "G4", 134),
    x("g5", "G5", 135),
    x("g6", "G6", 136),
    x("wheel", "Wheel", 137),
    // K65 PLUS rotary dial press [HW: bit 137].
    x("dial", "Dial", 137),
    // K100 volume roller: consumer volume events on IF0, no key bit and no LED.
    ("volume-roller", "Volume", None, None),
];

fn spec(id: &str) -> Spec {
    *SPECS
        .iter()
        .find(|spec| spec.0 == id)
        .unwrap_or_else(|| panic!("layout spec missing for key {id:?}"))
}

/// One row item: a key id and its width, or a gap ("" id).
type Item = (&'static str, f32);
const GAP: &str = "";

struct Builder {
    keys: Vec<Key>,
    led: fn(Option<u16>) -> Option<u16>,
}

impl Builder {
    fn new(led: fn(Option<u16>) -> Option<u16>) -> Self {
        Self {
            keys: Vec::with_capacity(128),
            led,
        }
    }

    fn put(&mut self, id: &'static str, x: f32, y: f32, w: f32, h: f32) {
        let (id, label, index, evdev) = spec(id);
        self.keys.push(Key {
            id,
            label,
            x,
            y,
            w,
            h,
            index,
            evdev,
            led: (self.led)(index),
        });
    }

    /// Lays out `items` left to right from `x`, all 1u tall.
    fn row(&mut self, mut x: f32, y: f32, items: &[Item]) {
        for &(id, w) in items {
            if id != GAP {
                self.put(id, x, y, w, 1.0);
            }
            x += w;
        }
    }

    /// Consecutive 1u keys starting at `x`.
    fn ones(&mut self, x: f32, y: f32, ids: &[&'static str]) {
        for (i, id) in ids.iter().enumerate() {
            self.put(id, x + i as f32, y, 1.0, 1.0);
        }
    }

    fn finish(self, name: &'static str, width: f32, height: f32) -> Layout {
        Layout {
            name,
            width,
            height,
            keys: Box::leak(self.keys.into_boxed_slice()),
        }
    }
}

const LETTERS_TOP: [&str; 10] = ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"];
const LETTERS_HOME: [&str; 9] = ["a", "s", "d", "f", "g", "h", "j", "k", "l"];
const LETTERS_BOTTOM: [&str; 7] = ["z", "x", "c", "v", "b", "n", "m"];
const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];

/// The 15u-wide alphanumeric block, rows 1..=4 (number row to shift row), top-left at (x, y).
/// The bottom row differs per board and is laid out by the caller.
fn main_block(b: &mut Builder, x: f32, y: f32, iso: bool, right_shift: f32) {
    b.put("grave", x, y, 1.0, 1.0);
    b.ones(x + 1.0, y, &DIGITS);
    b.ones(x + 11.0, y, &["minus", "equal"]);
    b.put("backspace", x + 13.0, y, 2.0, 1.0);

    let y2 = y + 1.0;
    b.put("tab", x, y2, 1.5, 1.0);
    b.ones(x + 1.5, y2, &LETTERS_TOP);
    b.ones(x + 11.5, y2, &["lbracket", "rbracket"]);

    let y3 = y + 2.0;
    b.put("capslock", x, y3, 1.75, 1.0);
    b.ones(x + 1.75, y3, &LETTERS_HOME);
    b.ones(x + 10.75, y3, &["semicolon", "apostrophe"]);

    let y4 = y + 3.0;
    if iso {
        // ISO: the L-shaped Enter is drawn as its 1.25u stem spanning two rows (rects cannot be
        // L-shaped); the # key sits left of it on the home row and \ next to a 1.25u left Shift.
        b.put("enter", x + 13.75, y2, 1.25, 2.0);
        b.put("hash", x + 12.75, y3, 1.0, 1.0);
        b.put("l-shift", x, y4, 1.25, 1.0);
        b.put("iso-backslash", x + 1.25, y4, 1.0, 1.0);
    } else {
        b.put("backslash", x + 13.5, y2, 1.5, 1.0);
        b.put("enter", x + 12.75, y3, 2.25, 1.0);
        b.put("l-shift", x, y4, 2.25, 1.0);
    }
    // Both variants start Z at 2.25u (ANSI 2.25u Shift; ISO 1.25u Shift + 1u \).
    b.ones(x + 2.25, y4, &LETTERS_BOTTOM);
    b.ones(x + 9.25, y4, &["comma", "dot", "slash"]);
    b.put("r-shift", x + 12.25, y4, right_shift, 1.0);
}

/// LED rule for the K65 PLUS: the direct frame (resource 0x22, wired only) has 123 LEDs and the
/// research map puts each key at its key index. UNVERIFIED on hardware. The spacebar also lights
/// LEDs 0 and 1; only its key-index LED (44) is recorded here.
fn k65_led(index: Option<u16>) -> Option<u16> {
    index.filter(|&i| i < 123)
}

/// LED rule for the K100 RGB: direct-frame LED id == key index (docs/research/k100.md §4.2).
fn k100_led(index: Option<u16>) -> Option<u16> {
    index
}

/// Function row → number row pitch on the K65 PLUS (measured ≈ 1.2u).
const K65_ROW1: f32 = 1.2;
/// Arrow cluster drop below the shift/bottom rows on the K65 PLUS (measured ≈ 0.15u).
const K65_ARROW_DROP: f32 = 0.15;

pub(super) fn k65_plus(iso: bool) -> Layout {
    let mut b = Builder::new(k65_led);
    // Function row: Esc, F1–F12 in groups of four, Delete; 0.25u gaps; dial in the corner.
    b.row(
        0.0,
        0.0,
        &[
            ("esc", 1.0),
            (GAP, 0.25),
            ("f1", 1.0),
            ("f2", 1.0),
            ("f3", 1.0),
            ("f4", 1.0),
            (GAP, 0.25),
            ("f5", 1.0),
            ("f6", 1.0),
            ("f7", 1.0),
            ("f8", 1.0),
            (GAP, 0.25),
            ("f9", 1.0),
            ("f10", 1.0),
            ("f11", 1.0),
            ("f12", 1.0),
            (GAP, 0.25),
            ("delete", 1.0),
            (GAP, 0.25),
            ("dial", 1.0),
        ],
    );
    let y = K65_ROW1;
    main_block(&mut b, 0.0, y, iso, 1.75);
    // Right-hand navigation column.
    b.put("home", 15.25, y, 1.0, 1.0);
    b.put("pageup", 15.25, y + 1.0, 1.0, 1.0);
    b.put("pagedown", 15.25, y + 2.0, 1.0, 1.0);
    // Bottom row (split right side: Alt, Fn, Ctrl at 1u).
    let y5 = y + 4.0;
    b.row(
        0.0,
        y5,
        &[
            ("l-ctrl", 1.25),
            ("l-super", 1.25),
            ("l-alt", 1.25),
            ("space", 6.25),
            ("r-alt", 1.0),
            ("fn", 1.0),
            ("r-ctrl", 1.0),
        ],
    );
    // Dropped arrow cluster.
    b.put("up", 14.25, y + 3.0 + K65_ARROW_DROP, 1.0, 1.0);
    b.ones(13.25, y5 + K65_ARROW_DROP, &["left", "down", "right"]);
    let name = if iso {
        "K65 PLUS Wireless (ISO)"
    } else {
        "K65 PLUS Wireless (ANSI)"
    };
    b.finish(name, 16.25, y5 + K65_ARROW_DROP + 1.0)
}

/// Left edge of the K100 main block (G-key column is at x = 0; measured gap ≈ 0.35u).
const K100_MX: f32 = 1.35;
/// Top of the K100 function row (below the wheel / indicator strip).
const K100_FROW: f32 = 2.0;
/// Function row → number row pitch on the K100 (measured ≈ 1.125u).
const K100_ROW1: f32 = K100_FROW + 1.125;
/// Pitch of the six G keys, which span the function row to the bottom row.
const K100_G_PITCH: f32 = 1.025;
/// Height of the small top-strip keys and the media keys.
const K100_SMALL_H: f32 = 0.75;

pub(super) fn k100_rgb(iso: bool) -> Layout {
    let mut b = Builder::new(k100_led);
    let mx = K100_MX;
    let nav = mx + 15.25;
    let pad = mx + 18.5;

    // Top strip: profile key, iCUE control wheel (centre key), Win-lock key; mute + volume roller.
    let strip_y = 0.45;
    b.put("profile", 0.85, strip_y, 0.8, K100_SMALL_H);
    b.put("wheel", 1.7, 0.0, 1.7, 1.7);
    b.put("win-lock", 3.5, strip_y, 0.8, K100_SMALL_H);
    b.put("mute", pad, strip_y, 0.9, K100_SMALL_H);
    b.put("volume-roller", pad + 1.0, strip_y, 2.0, K100_SMALL_H);

    // G1–G6 column.
    for (i, id) in ["g1", "g2", "g3", "g4", "g5", "g6"].iter().enumerate() {
        b.put(id, 0.0, K100_FROW + i as f32 * K100_G_PITCH, 1.0, 1.0);
    }

    // Function row: Esc and three groups of four with equal 2/3u gaps across the 15u block.
    let fy = K100_FROW;
    b.put("esc", mx, fy, 1.0, 1.0);
    b.ones(mx + 5.0 / 3.0, fy, &["f1", "f2", "f3", "f4"]);
    b.ones(mx + 19.0 / 3.0, fy, &["f5", "f6", "f7", "f8"]);
    b.ones(mx + 11.0, fy, &["f9", "f10", "f11", "f12"]);
    b.ones(nav, fy, &["printscreen", "scrolllock", "pause"]);
    for (i, id) in ["stopcd", "previoussong", "playpause", "nextsong"]
        .iter()
        .enumerate()
    {
        b.put(id, pad + i as f32, fy, 1.0, K100_SMALL_H);
    }

    let y = K100_ROW1;
    main_block(&mut b, mx, y, iso, 2.75);
    let y5 = y + 4.0;
    b.row(
        mx,
        y5,
        &[
            ("l-ctrl", 1.25),
            ("l-super", 1.25),
            ("l-alt", 1.25),
            ("space", 6.25),
            ("r-alt", 1.25),
            ("fn", 1.25),
            ("menu", 1.25),
            ("r-ctrl", 1.25),
        ],
    );

    // Navigation cluster and arrows.
    b.ones(nav, y, &["insert", "home", "pageup"]);
    b.ones(nav, y + 1.0, &["delete", "end", "pagedown"]);
    b.put("up", nav + 1.0, y + 3.0, 1.0, 1.0);
    b.ones(nav, y5, &["left", "down", "right"]);

    // Numpad.
    b.ones(pad, y, &["numlock", "kp-slash", "kp-asterisk", "kp-minus"]);
    b.ones(pad, y + 1.0, &["kp7", "kp8", "kp9"]);
    b.put("kp-plus", pad + 3.0, y + 1.0, 1.0, 2.0);
    b.ones(pad, y + 2.0, &["kp4", "kp5", "kp6"]);
    b.ones(pad, y + 3.0, &["kp1", "kp2", "kp3"]);
    b.put("kp-enter", pad + 3.0, y + 3.0, 1.0, 2.0);
    b.put("kp0", pad, y5, 2.0, 1.0);
    b.put("kp-dot", pad + 2.0, y5, 1.0, 1.0);

    let name = if iso {
        "K100 RGB (ISO)"
    } else {
        "K100 RGB (ANSI)"
    };
    b.finish(name, pad + 4.0, y5 + 1.0)
}
