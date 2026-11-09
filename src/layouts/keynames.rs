//! Canonical key names for remap targets and macro combos -> Linux keycode.
//! Includes letters, digits, F1–F24, modifiers ("ctrl","shift","alt","super", and "l-"/"r-" variants),
//! navigation, editing, keypad, media ("volumeup","mute","playpause","nextsong","previoussong").
//!
//! Keycodes are the `KEY_*` values of linux/input-event-codes.h. Lookups are case-insensitive and
//! ignore '-', '_' and spaces, so "L-Shift", "left_shift" style spellings and "play_pause" resolve.

/// (canonical name, label, Linux keycode); one entry per keycode, in UI order.
const ENTRIES: &[(&str, &str, u16)] = &[
    // Modifiers (unprefixed = left).
    ("ctrl", "Ctrl", 29),
    ("shift", "Shift", 42),
    ("alt", "Alt", 56),
    ("super", "Super", 125),
    ("r-ctrl", "Right Ctrl", 97),
    ("r-shift", "Right Shift", 54),
    ("r-alt", "Right Alt", 100),
    ("r-super", "Right Super", 126),
    // Letters.
    ("a", "A", 30),
    ("b", "B", 48),
    ("c", "C", 46),
    ("d", "D", 32),
    ("e", "E", 18),
    ("f", "F", 33),
    ("g", "G", 34),
    ("h", "H", 35),
    ("i", "I", 23),
    ("j", "J", 36),
    ("k", "K", 37),
    ("l", "L", 38),
    ("m", "M", 50),
    ("n", "N", 49),
    ("o", "O", 24),
    ("p", "P", 25),
    ("q", "Q", 16),
    ("r", "R", 19),
    ("s", "S", 31),
    ("t", "T", 20),
    ("u", "U", 22),
    ("v", "V", 47),
    ("w", "W", 17),
    ("x", "X", 45),
    ("y", "Y", 21),
    ("z", "Z", 44),
    // Digits.
    ("1", "1", 2),
    ("2", "2", 3),
    ("3", "3", 4),
    ("4", "4", 5),
    ("5", "5", 6),
    ("6", "6", 7),
    ("7", "7", 8),
    ("8", "8", 9),
    ("9", "9", 10),
    ("0", "0", 11),
    // Whitespace / editing.
    ("esc", "Esc", 1),
    ("enter", "Enter", 28),
    ("tab", "Tab", 15),
    ("space", "Space", 57),
    ("backspace", "Backspace", 14),
    ("delete", "Delete", 111),
    ("insert", "Insert", 110),
    ("capslock", "Caps Lock", 58),
    // Punctuation.
    ("grave", "`", 41),
    ("minus", "-", 12),
    ("equal", "=", 13),
    ("lbracket", "[", 26),
    ("rbracket", "]", 27),
    ("backslash", "\\", 43),
    ("semicolon", ";", 39),
    ("apostrophe", "'", 40),
    ("comma", ",", 51),
    ("dot", ".", 52),
    ("slash", "/", 53),
    ("iso-backslash", "ISO \\", 86),
    // Navigation.
    ("up", "Up", 103),
    ("down", "Down", 108),
    ("left", "Left", 105),
    ("right", "Right", 106),
    ("home", "Home", 102),
    ("end", "End", 107),
    ("pageup", "Page Up", 104),
    ("pagedown", "Page Down", 109),
    // System.
    ("printscreen", "Print Screen", 99),
    ("scrolllock", "Scroll Lock", 70),
    ("pause", "Pause", 119),
    ("menu", "Menu", 127),
    // Function keys.
    ("f1", "F1", 59),
    ("f2", "F2", 60),
    ("f3", "F3", 61),
    ("f4", "F4", 62),
    ("f5", "F5", 63),
    ("f6", "F6", 64),
    ("f7", "F7", 65),
    ("f8", "F8", 66),
    ("f9", "F9", 67),
    ("f10", "F10", 68),
    ("f11", "F11", 87),
    ("f12", "F12", 88),
    ("f13", "F13", 183),
    ("f14", "F14", 184),
    ("f15", "F15", 185),
    ("f16", "F16", 186),
    ("f17", "F17", 187),
    ("f18", "F18", 188),
    ("f19", "F19", 189),
    ("f20", "F20", 190),
    ("f21", "F21", 191),
    ("f22", "F22", 192),
    ("f23", "F23", 193),
    ("f24", "F24", 194),
    // Keypad.
    ("numlock", "Num Lock", 69),
    ("kp-slash", "Keypad /", 98),
    ("kp-asterisk", "Keypad *", 55),
    ("kp-minus", "Keypad -", 74),
    ("kp-plus", "Keypad +", 78),
    ("kp-enter", "Keypad Enter", 96),
    ("kp-dot", "Keypad .", 83),
    ("kp0", "Keypad 0", 82),
    ("kp1", "Keypad 1", 79),
    ("kp2", "Keypad 2", 80),
    ("kp3", "Keypad 3", 81),
    ("kp4", "Keypad 4", 75),
    ("kp5", "Keypad 5", 76),
    ("kp6", "Keypad 6", 77),
    ("kp7", "Keypad 7", 71),
    ("kp8", "Keypad 8", 72),
    ("kp9", "Keypad 9", 73),
    // Media.
    ("volumeup", "Volume Up", 115),
    ("volumedown", "Volume Down", 114),
    ("mute", "Mute", 113),
    ("playpause", "Play/Pause", 164),
    ("nextsong", "Next Track", 163),
    ("previoussong", "Previous Track", 165),
    ("stopcd", "Stop", 166),
    ("brightnessdown", "Screen Brightness Down", 224),
    ("brightnessup", "Screen Brightness Up", 225),
];

/// alias -> canonical name (both compared after normalisation).
const ALIASES: &[(&str, &str)] = &[
    ("control", "ctrl"),
    ("lctrl", "ctrl"),
    ("leftctrl", "ctrl"),
    ("leftcontrol", "ctrl"),
    ("rightctrl", "r-ctrl"),
    ("rcontrol", "r-ctrl"),
    ("rightcontrol", "r-ctrl"),
    ("lshift", "shift"),
    ("leftshift", "shift"),
    ("rightshift", "r-shift"),
    ("lalt", "alt"),
    ("leftalt", "alt"),
    ("option", "alt"),
    ("rightalt", "r-alt"),
    ("altgr", "r-alt"),
    ("win", "super"),
    ("meta", "super"),
    ("cmd", "super"),
    ("command", "super"),
    ("logo", "super"),
    ("lsuper", "super"),
    ("lwin", "super"),
    ("lmeta", "super"),
    ("leftsuper", "super"),
    ("leftmeta", "super"),
    ("leftwin", "super"),
    ("rwin", "r-super"),
    ("rmeta", "r-super"),
    ("rightsuper", "r-super"),
    ("rightmeta", "r-super"),
    ("rightwin", "r-super"),
    ("escape", "esc"),
    ("return", "enter"),
    ("ret", "enter"),
    ("spacebar", "space"),
    ("bksp", "backspace"),
    ("del", "delete"),
    ("ins", "insert"),
    ("caps", "capslock"),
    ("backtick", "grave"),
    ("tilde", "grave"),
    ("dash", "minus"),
    ("hyphen", "minus"),
    ("equals", "equal"),
    ("leftbrace", "lbracket"),
    ("lbrace", "lbracket"),
    ("bracketleft", "lbracket"),
    ("rightbrace", "rbracket"),
    ("rbrace", "rbracket"),
    ("bracketright", "rbracket"),
    ("semi", "semicolon"),
    ("quote", "apostrophe"),
    ("apos", "apostrophe"),
    ("period", "dot"),
    ("102nd", "iso-backslash"),
    ("pgup", "pageup"),
    ("prior", "pageup"),
    ("pgdn", "pagedown"),
    ("pgdown", "pagedown"),
    ("print", "printscreen"),
    ("prtsc", "printscreen"),
    ("prtscr", "printscreen"),
    ("sysrq", "printscreen"),
    ("scrlk", "scrolllock"),
    ("scroll", "scrolllock"),
    ("break", "pause"),
    ("compose", "menu"),
    ("application", "menu"),
    ("apps", "menu"),
    ("numlk", "numlock"),
    ("kpdivide", "kp-slash"),
    ("kpmultiply", "kp-asterisk"),
    ("kpstar", "kp-asterisk"),
    ("kpsubtract", "kp-minus"),
    ("kpadd", "kp-plus"),
    ("kpperiod", "kp-dot"),
    ("kpdecimal", "kp-dot"),
    ("kpdel", "kp-dot"),
    ("volup", "volumeup"),
    ("voldown", "volumedown"),
    ("volumemute", "mute"),
    ("play", "playpause"),
    ("next", "nextsong"),
    ("nexttrack", "nextsong"),
    ("prev", "previoussong"),
    ("previous", "previoussong"),
    ("prevtrack", "previoussong"),
    ("previoustrack", "previoussong"),
    ("prevsong", "previoussong"),
    ("stop", "stopcd"),
];

/// Single printable characters accepted as key names (US layout).
const CHARS: &[(char, &str)] = &[
    ('`', "grave"),
    ('-', "minus"),
    ('=', "equal"),
    ('[', "lbracket"),
    (']', "rbracket"),
    ('\\', "backslash"),
    (';', "semicolon"),
    ('\'', "apostrophe"),
    (',', "comma"),
    ('.', "dot"),
    ('/', "slash"),
];

/// Linux keycodes of the modifier keys (left and right Ctrl/Shift/Alt/Super).
const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn canonical_code(normalized: &str) -> Option<u16> {
    ENTRIES
        .iter()
        .find(|(name, _, _)| normalize(name) == normalized)
        .map(|&(_, _, code)| code)
}

/// Lowercase name -> keycode. Accepts aliases (e.g. "control"→ctrl, "win"/"meta"→super, "esc"/"escape").
pub fn code(name: &str) -> Option<u16> {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Some(&(_, canonical)) = CHARS.iter().find(|(ch, _)| *ch == c)
    {
        return canonical_code(canonical);
    }
    let normalized = normalize(trimmed);
    if normalized.is_empty() {
        return None;
    }
    canonical_code(&normalized).or_else(|| {
        ALIASES
            .iter()
            .find(|(alias, _)| normalize(alias) == normalized)
            .and_then(|(_, canonical)| canonical_code(&normalize(canonical)))
    })
}

/// Canonical display name for a keycode (e.g. 29 → "ctrl").
pub fn name(code: u16) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|&&(_, _, c)| c == code)
        .map(|&(name, _, _)| name)
}

/// All canonical (name, label, code) entries, ordered for UI choice lists (≤ 256 entries).
pub fn all() -> &'static [(&'static str, &'static str, u16)] {
    ENTRIES
}

/// Parse "ctrl+shift+t" into keycodes (modifiers first). Err text names the bad token.
pub fn parse_combo(combo: &str) -> Result<Vec<u16>, String> {
    if combo.trim().is_empty() {
        return Err("empty key combo".to_string());
    }
    let mut codes = Vec::new();
    for token in combo.split('+') {
        let token = token.trim();
        if token.is_empty() {
            return Err(format!("empty key name in combo \"{combo}\""));
        }
        let Some(code) = code(token) else {
            return Err(format!("unknown key \"{token}\""));
        };
        if codes.contains(&code) {
            return Err(format!("key \"{token}\" appears twice"));
        }
        codes.push(code);
    }
    // Stable: modifiers keep their order, then the other keys keep theirs.
    codes.sort_by_key(|code| !MODIFIERS.contains(code));
    Ok(codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_bounded_and_unique() {
        assert!(ENTRIES.len() <= 256);
        for (i, (name, label, code)) in ENTRIES.iter().enumerate() {
            assert!(!label.is_empty() && label.len() <= 32, "{name}");
            assert!(*code > 0 && *code < 0x2ff, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name}"
            );
            for (other, _, other_code) in &ENTRIES[i + 1..] {
                assert_ne!(normalize(name), normalize(other), "{name} / {other}");
                assert_ne!(code, other_code, "{name} / {other}");
            }
        }
    }

    #[test]
    fn aliases_resolve_and_do_not_shadow_canonical_names() {
        for (alias, canonical) in ALIASES {
            assert!(
                canonical_code(&normalize(canonical)).is_some(),
                "{alias} -> {canonical}"
            );
            assert!(
                canonical_code(&normalize(alias)).is_none(),
                "alias {alias} shadows a canonical name"
            );
        }
        for (_, canonical) in CHARS {
            assert!(canonical_code(canonical).is_some(), "{canonical}");
        }
    }

    #[test]
    fn known_codes() {
        let cases = [
            ("a", 30),
            ("z", 44),
            ("1", 2),
            ("0", 11),
            ("esc", 1),
            ("escape", 1),
            ("enter", 28),
            ("ctrl", 29),
            ("control", 29),
            ("l-ctrl", 29),
            ("r-ctrl", 97),
            ("shift", 42),
            ("r-shift", 54),
            ("alt", 56),
            ("r-alt", 100),
            ("super", 125),
            ("win", 125),
            ("meta", 125),
            ("r-super", 126),
            ("f1", 59),
            ("f10", 68),
            ("f11", 87),
            ("f12", 88),
            ("f13", 183),
            ("f24", 194),
            ("space", 57),
            ("delete", 111),
            ("pageup", 104),
            ("pagedown", 109),
            ("printscreen", 99),
            ("menu", 127),
            ("kp0", 82),
            ("kp-enter", 96),
            ("volumeup", 115),
            ("volumedown", 114),
            ("mute", 113),
            ("playpause", 164),
            ("nextsong", 163),
            ("previoussong", 165),
            ("stopcd", 166),
            ("iso-backslash", 86),
        ];
        for (name, expected) in cases {
            assert_eq!(code(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn lookups_are_forgiving() {
        assert_eq!(code(" Ctrl "), Some(29));
        assert_eq!(code("L_Shift"), Some(42));
        assert_eq!(code("left-shift"), Some(42));
        assert_eq!(code("play_pause"), Some(164));
        assert_eq!(code("Volume-Up"), Some(115));
        assert_eq!(code("kp_1"), Some(79));
        assert_eq!(code("-"), Some(12));
        assert_eq!(code("/"), Some(53));
        assert_eq!(code("\\"), Some(43));
        assert_eq!(code(""), None);
        assert_eq!(code("--"), None);
        assert_eq!(code("hyper"), None);
        assert_eq!(code("f25"), None);
    }

    #[test]
    fn names_round_trip() {
        for &(name_, _, code_) in all() {
            assert_eq!(name(code_), Some(name_));
            assert_eq!(code(name_), Some(code_));
        }
        assert_eq!(name(29), Some("ctrl"));
        assert_eq!(name(0), None);
    }

    #[test]
    fn combos() {
        assert_eq!(parse_combo("ctrl+shift+t"), Ok(vec![29, 42, 20]));
        assert_eq!(parse_combo("t+shift+ctrl"), Ok(vec![42, 29, 20]));
        assert_eq!(parse_combo(" super + 1 "), Ok(vec![125, 2]));
        assert_eq!(parse_combo("ctrl+-"), Ok(vec![29, 12]));
        assert_eq!(parse_combo("f5"), Ok(vec![63]));
        assert_eq!(parse_combo("volumeup"), Ok(vec![115]));
        let err = parse_combo("ctrl+bogus").unwrap_err();
        assert!(err.contains("bogus"), "{err}");
        assert!(parse_combo("").is_err());
        assert!(parse_combo("ctrl+").unwrap_err().contains("empty"));
        assert!(parse_combo("a+a").unwrap_err().contains("twice"));
        assert!(parse_combo("ctrl+control").unwrap_err().contains("twice"));
    }
}
