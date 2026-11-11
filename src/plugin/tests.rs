//! Plugin tests: fake keyboards (bragi's FakeKeyboard), a fake daemon and a fake systemctl.
//! Storage goes to a per-process temp XDG tree; every test uses its own device id.

use super::backend::{Backend, LOCK_WAIT};
use super::*;
use crate::bragi::tests::FakeKeyboard;
use crate::bragi::{
    CMD_OPEN, CMD_SET, MODE_HARDWARE, PROP_APP_FW, PROP_BATTERY, PROP_BOOT_FW, PROP_BRIGHTNESS,
    PROP_BRIGHTNESS_2, PROP_CHARGE, PROP_CHILDREN, PROP_LAYOUT, PROP_LOCK_ENABLE, PROP_LOCK_MASK,
    PROP_MODE, PROP_RADIO_FW, PROP_SLEEP_MS, RES_K65_EFFECT, Reply, Transport,
};
use crate::devices::{Keyboard, LinkKind, Model};
use crate::ipc::DeviceStatus;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Once;

const NOW: u64 = 1_800_000_000;

pub(crate) fn test_root() -> PathBuf {
    static INIT: Once = Once::new();
    let root = std::env::temp_dir().join(format!("hyprdeck-corsair-plugin-{}", std::process::id()));
    INIT.call_once(|| {
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["config", "state", "runtime", "sys", "dev"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        // SAFETY: set once, before any test in this module reads these variables.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
            std::env::set_var("XDG_STATE_HOME", root.join("state"));
            std::env::set_var("XDG_RUNTIME_DIR", root.join("runtime"));
            std::env::set_var(crate::devices::ENV_SYSFS, root.join("sys"));
            std::env::set_var(crate::devices::ENV_DEV, root.join("dev"));
        }
    });
    root
}

#[derive(Clone)]
struct Shared(Rc<RefCell<FakeKeyboard>>);

impl Transport for Shared {
    fn xfer(&mut self, payload: &[u8]) -> anyhow::Result<Reply> {
        self.0.borrow_mut().xfer(payload)
    }
    fn sleep(&mut self, duration: Duration) {
        self.0.borrow_mut().sleep(duration);
    }
}

fn k65_fake() -> FakeKeyboard {
    let mut fake = FakeKeyboard::new();
    fake.props.insert(PROP_BATTERY, 420);
    fake.props.insert(PROP_CHARGE, 2);
    fake.props.insert(PROP_APP_FW, 0x009a_1a05);
    fake.props.insert(PROP_BOOT_FW, 0x000e_0900);
    fake.props.insert(PROP_RADIO_FW, 0x2701);
    fake.props.insert(PROP_LAYOUT, 1);
    fake.props.insert(PROP_LOCK_ENABLE, 0);
    fake.props.insert(PROP_LOCK_MASK, 0);
    fake
}

struct Fake {
    keyboards: Vec<Keyboard>,
    device: Rc<RefCell<FakeKeyboard>>,
    receiver: Rc<RefCell<FakeKeyboard>>,
    daemon: RefCell<Option<Vec<DeviceStatus>>>,
    open_error: Option<String>,
    opens: Cell<u32>,
    reloads: Cell<u32>,
    systemctl: RefCell<Vec<String>>,
    unit: PathBuf,
    input_rule: bool,
    now: Cell<u64>,
}

impl Fake {
    fn new(tag: &str, model: Model, link: LinkKind) -> Self {
        let root = test_root();
        let serial = format!("{tag}{}", std::process::id());
        let keyboard = Keyboard {
            id: format!("{}-{serial}", model.id_prefix()),
            model,
            link,
            name: model.display_name(),
            vid: 0x1b1c,
            pid: match (model, link) {
                (Model::K65PlusWireless, LinkKind::Receiver) => 0x2b07,
                (Model::K65PlusWireless, LinkKind::Usb) => 0x2b10,
                (Model::K100Rgb, _) => 0x1b7c,
            },
            serial,
            usb_path: PathBuf::from("/sys/devices/fake/1-3"),
            cmd_node: PathBuf::from("/nonexistent/hidraw3"),
            event_node: Some(PathBuf::from("/nonexistent/hidraw4")),
            evdev_nodes: vec![PathBuf::from("/nonexistent/event5")],
            frame_len: model.frame_len(),
            target: if link == LinkKind::Receiver {
                0x09
            } else {
                0x08
            },
        };
        let mut device = k65_fake();
        if model == Model::K100Rgb {
            device.props.insert(PROP_BRIGHTNESS_2, 700);
            device.props.insert(crate::bragi::PROP_MAX_POLLING, 7);
            device.resources.clear();
        }
        let mut receiver = FakeKeyboard::new();
        receiver.props.insert(PROP_CHILDREN, 0x02);
        receiver.props.insert(PROP_APP_FW, 0x4401);
        Self {
            keyboards: vec![keyboard],
            device: Rc::new(RefCell::new(device)),
            receiver: Rc::new(RefCell::new(receiver)),
            daemon: RefCell::new(None),
            open_error: None,
            opens: Cell::new(0),
            reloads: Cell::new(0),
            systemctl: RefCell::new(Vec::new()),
            unit: root.join(format!("config/systemd-{tag}/hyprdeck-corsair.service")),
            input_rule: true,
            now: Cell::new(NOW),
        }
    }
    fn k65(tag: &str) -> Self {
        Self::new(tag, Model::K65PlusWireless, LinkKind::Receiver)
    }
    fn id(&self) -> String {
        self.keyboards[0].id.clone()
    }
    fn log(&self) -> Vec<Vec<u8>> {
        self.device.borrow().log.clone()
    }
    fn clear_log(&self) {
        self.device.borrow_mut().log.clear();
    }
    fn sets(&self, prop: u8) -> Vec<u32> {
        self.log()
            .iter()
            .filter(|payload| payload[0] == CMD_SET && payload[1] == prop)
            .map(|payload| {
                let mut bytes = [0u8; 4];
                for (slot, byte) in bytes.iter_mut().zip(&payload[3..]) {
                    *slot = *byte;
                }
                u32::from_le_bytes(bytes)
            })
            .collect()
    }
    fn state(&self, viz: bool) -> Value {
        let validated = Validated {
            viz,
            animation: viz,
            action: None,
        };
        let state = dispatch(self, &validated).unwrap();
        let envelope = success(state);
        assert!(envelope["error"].is_null(), "{}", envelope["error"]);
        envelope["state"].clone()
    }
    fn action(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let mut args = args;
        args["device"] = json!(self.id());
        self.action_raw(name, args)
    }
    fn action_raw(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let request = Request {
            api_version: json!(1),
            method: json!("action"),
            action: json!(name),
            args,
            host_features: json!([HOST_FEATURE_KEYBOARD]),
        };
        let validated = validate(request)?;
        let state = dispatch(self, &validated)?;
        Ok(success(state)["state"].clone())
    }
}

impl Backend for Fake {
    fn keyboards(&self) -> Vec<Keyboard> {
        self.keyboards.clone()
    }
    fn unsupported(&self) -> Vec<(u16, String)> {
        vec![(
            0x1bab,
            "Corsair K100 AIR Wireless (not supported yet)".into(),
        )]
    }
    fn daemon_hello(&self) -> anyhow::Result<Option<Vec<DeviceStatus>>> {
        Ok(self.daemon.borrow().clone())
    }
    fn daemon_reload(&self) -> anyhow::Result<bool> {
        self.reloads.set(self.reloads.get() + 1);
        Ok(true)
    }
    fn open(&self, _keyboard: &Keyboard, _via_daemon: bool) -> anyhow::Result<Box<dyn Transport>> {
        let _ = LOCK_WAIT;
        if let Some(error) = &self.open_error {
            anyhow::bail!("{error}");
        }
        self.opens.set(self.opens.get() + 1);
        Ok(Box::new(Shared(self.device.clone())))
    }
    fn open_receiver(&self, _keyboard: &Keyboard) -> anyhow::Result<Box<dyn Transport>> {
        if let Some(error) = &self.open_error {
            anyhow::bail!("{error}");
        }
        Ok(Box::new(Shared(self.receiver.clone())))
    }
    fn input_rule_installed(&self) -> bool {
        self.input_rule
    }
    fn systemctl(&self, args: &[&str]) -> anyhow::Result<()> {
        self.systemctl.borrow_mut().push(args.join(" "));
        // The fake helper answers as soon as its unit is started, and stops with it.
        match args.first().copied() {
            Some("enable") if self.daemon.borrow().is_none() => {
                *self.daemon.borrow_mut() = Some(Vec::new());
            }
            Some("disable") => *self.daemon.borrow_mut() = None,
            _ => {}
        }
        Ok(())
    }
    fn unit_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.unit.clone())
    }
    fn exe(&self) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(
            "/home/u/.local/share/hyprdeck/plugins/corsair/hyprdeck-corsair",
        ))
    }
    fn now(&self) -> u64 {
        self.now.get()
    }
}

// ---------------------------------------------------------------------------------------------
// Host validation rules (docs/PLUGINS.md + host keyboard visualization), restated for tests.

fn host_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn text(value: &Value, required: bool) {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("expected text, got {value}"));
    assert!(!required || !text.trim().is_empty(), "empty required text");
    assert!(text.len() <= 16_384 && !text.contains('\0'));
}

fn color_ok(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn check_options(options: &Value, max: usize, current: Option<&Value>) {
    let options = options.as_array().expect("options array");
    assert!(
        !options.is_empty() && options.len() <= max,
        "{} options",
        options.len()
    );
    let mut values = HashSet::new();
    for option in options {
        text(&option["value"], false);
        text(&option["label"], true);
        assert!(
            values.insert(option["value"].as_str().unwrap().to_string()),
            "duplicate option"
        );
    }
    if let Some(current) = current {
        assert!(
            values.contains(current.as_str().unwrap()),
            "value {current} not in options"
        );
    }
}

fn check_number(control: &Value) {
    let [value, min, max, step] =
        ["value", "min", "max", "step"].map(|k| control[k].as_f64().unwrap());
    assert!(value.is_finite() && min.is_finite() && max.is_finite() && step > 0.0);
    assert!(min <= value && value <= max);
}

fn check_control(control: &Value) {
    if control.is_null() {
        return;
    }
    let kind = control["kind"].as_str().unwrap();
    if kind != "form" || control.get("label").is_some() {
        text(&control["action"], true);
    }
    assert!(control["action"].as_str().unwrap().len() <= 128);
    assert!(control["args"].is_object());
    assert!(control["args"]["device"].is_string() || control["args"].get("device").is_none());
    match kind {
        "button" => {
            text(&control["label"], true);
            assert!(control["destructive"].is_boolean());
        }
        "switch" => assert!(control["value"].is_boolean()),
        "number" => check_number(control),
        "choice" => check_options(&control["options"], 256, Some(&control["value"])),
        "text" => text(&control["value"], false),
        "color" => assert!(color_ok(control["value"].as_str().unwrap())),
        "form" => {
            text(&control["label"], true);
            let fields = control["fields"].as_array().unwrap();
            assert!(!fields.is_empty() && fields.len() <= 32);
            let mut ids = HashSet::new();
            for field in fields {
                let id = field["id"].as_str().unwrap();
                assert!(host_id(id) && ids.insert(id.to_string()), "field id {id}");
                text(&field["label"], true);
                match field["kind"].as_str().unwrap() {
                    "text" => text(&field["value"], false),
                    "choice" => check_options(&field["options"], 1024, Some(&field["value"])),
                    "number" => check_number(field),
                    "switch" => assert!(field["value"].is_boolean()),
                    "color" => assert!(color_ok(field["value"].as_str().unwrap())),
                    other => panic!("unknown field kind {other}"),
                }
            }
        }
        other => panic!("unknown control kind {other}"),
    }
}

fn check_visualization(viz: &Value) {
    assert_eq!(viz["kind"], "keyboard");
    text(&viz["name"], true);
    text(&viz["connection"], false);
    text(&viz["status"], false);
    let (width, height) = (
        viz["layout"]["width"].as_f64().unwrap(),
        viz["layout"]["height"].as_f64().unwrap(),
    );
    assert!((1.0..=64.0).contains(&width) && (1.0..=64.0).contains(&height));
    let options: Option<HashSet<String>> = viz.get("remap").map(|remap| {
        text(&remap["label"], true);
        text(&remap["action"], true);
        check_options(&remap["options"], 1024, None);
        remap["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap().to_string())
            .collect()
    });
    if let Some(paint) = viz.get("paint") {
        text(&paint["label"], true);
        text(&paint["action"], true);
    }
    let keys = viz["keys"].as_array().unwrap();
    assert!(!keys.is_empty() && keys.len() <= 512);
    let mut ids = HashSet::new();
    for key in keys {
        let id = key["id"].as_str().unwrap();
        assert!(host_id(id) && ids.insert(id.to_string()), "key id {id}");
        let label = key["label"].as_str().unwrap();
        assert!(
            !label.trim().is_empty() && label.len() <= 32 && !label.chars().any(char::is_control)
        );
        let [x, y, w, h] = ["x", "y", "w", "h"].map(|k| key[k].as_f64().unwrap());
        assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0, "{id}");
        assert!(
            x + w <= width + 1e-6 && y + h <= height + 1e-6,
            "{id} outside layout"
        );
        assert!(key["color"].is_null() || color_ok(key["color"].as_str().unwrap()));
        assert!(key["pressed"].is_null() || key["pressed"].is_boolean());
        if let Some(remap) = key.get("remap").filter(|v| !v.is_null()) {
            assert!(
                options.as_ref().unwrap().contains(remap.as_str().unwrap()),
                "{id} remap"
            );
        }
    }
    if let Some(stream) = viz.get("stream") {
        let args = stream["args"].as_array().unwrap();
        assert!(!args.is_empty() && args.len() <= 16);
    }
}

/// Validate a state the way the host does; returns the number of rows.
fn check_state(state: &Value) -> usize {
    assert!(serde_json::to_string(state).unwrap().len() < 1024 * 1024);
    text(&state["title"], true);
    let refresh = state["refresh_interval_ms"].as_u64().unwrap();
    assert!((250..=10_000).contains(&refresh));
    let groups = state["groups"].as_array().unwrap();
    assert!(groups.len() <= 128);
    let mut group_ids = HashSet::new();
    let mut rows_total = 0;
    for group in groups {
        let id = group["id"].as_str().unwrap();
        assert!(
            !id.is_empty() && group_ids.insert(id.to_string()),
            "group {id}"
        );
        text(&group["title"], true);
        text(&group["description"], false);
        let mut row_ids = HashSet::new();
        for row in group["rows"].as_array().unwrap() {
            rows_total += 1;
            let row_id = row["id"].as_str().unwrap();
            assert!(
                !row_id.is_empty() && row_ids.insert(row_id.to_string()),
                "row {id}/{row_id}"
            );
            text(&row["title"], true);
            text(&row["subtitle"], false);
            check_control(&row["control"]);
        }
        if let Some(viz) = group.get("visualization") {
            check_visualization(viz);
        }
    }
    assert!(rows_total <= 1024);
    let mut product_ids = HashSet::new();
    for product in state["products"].as_array().unwrap() {
        let id = product["id"].as_str().unwrap();
        assert!(host_id(id) && product_ids.insert(id.to_string()));
        assert_eq!(product["kind"], "keyboard");
        text(&product["name"], true);
        for key in ["description", "status", "connection"] {
            text(&product[key], false);
        }
        if !product["battery"].is_null() {
            let battery = &product["battery"];
            if let Some(percentage) = battery["percentage"].as_f64() {
                assert!((0.0..=100.0).contains(&percentage));
            }
            assert!(battery["charging"].is_null() || battery["charging"].is_boolean());
            text(&battery["source"], false);
        }
        let sections = product["sections"].as_array().unwrap();
        assert!(sections.len() <= 12);
        let mut section_ids = HashSet::new();
        for section in sections {
            let section_id = section["id"].as_str().unwrap();
            assert!(host_id(section_id) && section_ids.insert(section_id.to_string()));
            text(&section["title"], true);
            let mut refs = HashSet::new();
            for group in section["groups"].as_array().unwrap() {
                let group = group.as_str().unwrap();
                assert!(group_ids.contains(group), "missing group {group}");
                assert!(refs.insert(group.to_string()));
            }
        }
    }
    rows_total
}

fn group<'a>(state: &'a Value, prefix: &str) -> &'a Value {
    state["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["id"].as_str().unwrap().starts_with(prefix))
        .unwrap_or_else(|| panic!("no group {prefix}"))
}

fn row<'a>(state: &'a Value, group_prefix: &str, row: &str) -> &'a Value {
    group(state, group_prefix)["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"] == row)
        .unwrap_or_else(|| panic!("no row {group_prefix}/{row}"))
}

fn has_group(state: &Value, prefix: &str) -> bool {
    state["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|group| group["id"].as_str().unwrap().starts_with(prefix))
}

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

// ---------------------------------------------------------------------------------------------
// Request validation

#[test]
fn dispatch_rejects_bad_requests_before_io() {
    test_root();
    for (input, expected) in [
        (&b"not json"[..], "Invalid request JSON"),
        (br#"{"api_version":2,"method":"state","args":{}}"#, "api_version"),
        (br#"{"api_version":"1","method":"state","args":{}}"#, "api_version"),
        (br#"{"api_version":1,"method":"state","args":[]}"#, "args must be"),
        (br#"{"api_version":1,"method":"state","action":"rescue","args":{}}"#, "must not include"),
        (br#"{"api_version":1,"method":"poke","args":{}}"#, "Unknown method"),
        (br#"{"api_version":1,"method":"action","args":{}}"#, "action name"),
        (br#"{"api_version":1,"method":"action","action":"flash","args":{"device":"k65plus-a"}}"#, "Unknown action"),
        (br#"{"api_version":1,"method":"action","action":"set-brightness","args":{"value":50}}"#, "device"),
        (br#"{"api_version":1,"method":"action","action":"set-brightness","args":{"device":"../x","value":50}}"#, "device"),
        (br#"{"api_version":1,"method":"action","action":"set-brightness","args":{"device":"k65plus-a","value":101}}"#, "0–100"),
    ] {
        let envelope = run_request(input);
        assert_eq!(envelope["api_version"], 1);
        assert!(envelope["state"].is_null());
        let error = envelope["error"].as_str().unwrap();
        assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    }
}

#[test]
fn action_arguments_are_validated() {
    let device = json!({"device": "k65plus-abc"});
    let with = |extra: Value| {
        let mut map = args(device.clone());
        map.extend(args(extra));
        map
    };
    let ok = |name: &str, extra: Value| actions::parse(name, &with(extra)).unwrap().act;
    let bad =
        |name: &str, extra: Value| actions::parse(name, &with(extra)).unwrap_err().to_string();
    use crate::effects::{EffectKind, Rgb, Speed};
    use actions::Act;
    assert_eq!(
        ok("set-effect", json!({"value": "rain"})),
        Act::SetEffect(EffectKind::Rain)
    );
    assert_eq!(
        ok("set-effect", json!({"effect": "static"})),
        Act::SetEffect(EffectKind::Static)
    );
    assert!(bad("set-effect", json!({"value": "unknown"})).contains("Choose an effect"));
    assert_eq!(
        ok("set-speed", json!({"value": "fast"})),
        Act::SetSpeed(Speed::Fast)
    );
    assert!(bad("set-speed", json!({"value": "warp"})).contains("slow"));
    assert_eq!(
        ok("set-color", json!({"value": "#ff0080"})),
        Act::SetColor(Rgb(0xff, 0, 0x80))
    );
    assert!(bad("set-color", json!({"value": "red"})).contains("#RRGGBB"));
    assert_eq!(
        ok("set-brightness", json!({"value": 55})),
        Act::SetBrightness(55)
    );
    assert_eq!(
        ok("set-brightness", json!({"value": 42.6})),
        Act::SetBrightness(43)
    );
    assert!(bad("set-brightness", json!({"value": -1})).contains("0–100"));
    assert!(bad("set-brightness", json!({"value": "lots"})).contains("number"));
    assert_eq!(
        ok("brightness-preset", json!({"value": "off"})),
        Act::SetBrightness(0)
    );
    assert_eq!(
        ok("brightness-preset", json!({"value": "full"})),
        Act::SetBrightness(100)
    );
    assert_eq!(ok("set-sleep", json!({"value": "15"})), Act::SetSleep(15));
    assert!(bad("set-sleep", json!({"value": "7"})).contains("one of"));
    assert!(bad("set-sleep", json!({"value": "unset"})).contains("Choose"));
    assert_eq!(
        ok("set-polling", json!({"value": "1000"})),
        Act::SetPolling(1000)
    );
    assert!(bad("set-polling", json!({"value": "1500"})).contains("Unsupported"));
    assert_eq!(
        ok("set-win-lock", json!({"value": true})),
        Act::SetWinLock(true)
    );
    assert!(bad("set-win-lock", json!({"value": "yes"})).contains("true or false"));
    assert_eq!(
        ok("set-lock-mask", json!({"bit": 2, "value": false})),
        Act::SetLockMask {
            bit: 2,
            value: false
        }
    );
    assert!(bad("set-lock-mask", json!({"bit": 4, "value": true})).contains("0–3"));
    assert_eq!(
        ok("save-look", json!({"value": {"name": " Night "}})),
        Act::SaveLook("Night".into())
    );
    assert!(bad("save-look", json!({"value": {"name": "  "}})).contains("Look name"));
    assert_eq!(
        ok("delete-look", json!({"value": {"look": "Night"}})),
        Act::DeleteLook("Night".into())
    );
    assert_eq!(
        ok("apply-look", json!({"value": "Night"})),
        Act::ApplyLook("Night".into())
    );
    match ok(
        "add-remap",
        json!({"value": {"key": "capslock", "action": "disabled", "combo": "ctrl+shift+t"}}),
    ) {
        Act::AddRemap { key, action } => {
            assert_eq!(key, "capslock");
            assert_eq!(
                action,
                crate::config::Action::Key {
                    combo: "ctrl+shift+t".into()
                }
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        bad(
            "add-remap",
            json!({"value": {"key": "capslock", "action": "warp:9"}})
        )
        .contains("Unknown action type")
    );
    assert!(
        bad(
            "add-remap",
            json!({"value": {"key": "Caps!", "action": "a"}})
        )
        .contains("Invalid key")
    );
    assert!(
        bad(
            "save-macro",
            json!({"value": {"name": "m", "steps": "ctrl+c, delay:99999"}})
        )
        .contains("Step 2")
    );
    match ok(
        "set-dial",
        json!({"mode": "custom", "value": {"clockwise": "media:next", "counter-clockwise": "", "press": "macro:m"}}),
    ) {
        Act::SetDial(dial) => {
            assert_eq!(dial.mode, crate::config::DialMode::Custom);
            assert!(
                dial.clockwise.is_some()
                    && dial.counter_clockwise.is_none()
                    && dial.press.is_some()
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        ok("set-dial", json!({"value": "scroll"})),
        Act::SetDialMode(_)
    ));
    assert!(bad("set-dial", json!({"value": "spin"})).contains("Unknown dial mode"));
    match ok(
        "set-key-colors",
        json!({"value": {"keys": ["esc", "key-1"], "color": "#00FF00"}}),
    ) {
        Act::SetKeyColors { keys, color } => {
            assert_eq!(keys, ["esc", "1"]);
            assert_eq!(color, Rgb(0, 0xff, 0));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        bad(
            "set-key-colors",
            json!({"value": {"keys": [], "color": "#00FF00"}})
        )
        .contains("Select")
    );
    assert_eq!(
        ok(
            "remap-key",
            json!({"value": {"key": "key-1", "target": "default"}})
        ),
        Act::RemapKey {
            key: "1".into(),
            target: None
        }
    );
    assert!(bad("set-zone-color", json!({"value": "#FFFFFF"})).contains("zone"));
    // Service actions do not need a device; every other action does.
    assert!(actions::parse("enable-live-mode", &args(json!({}))).is_ok());
    assert!(actions::parse("rescue", &args(json!({}))).is_err());
    for name in actions::ACTIONS {
        assert!(!name.is_empty());
    }
}

// ---------------------------------------------------------------------------------------------
// State rendering

#[test]
fn k65_state_is_valid_and_complete() {
    let fake = Fake::k65("state");
    let state = fake.state(true);
    let rows = check_state(&state);
    assert!(rows > 20);
    let product = &state["products"][0];
    assert_eq!(product["id"], fake.id());
    assert_eq!(product["status"], "Connected");
    assert_eq!(product["connection"], "SLIPSTREAM 2.4 GHz receiver");
    assert_eq!(product["battery"]["percentage"], 42.0);
    assert_eq!(product["battery"]["charging"], false);
    let sections: Vec<&str> = product["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(sections, ["lighting", "keys", "dial", "device", "advanced"]);
    // Effect read back once (Rain slow from the fake) and shown with its time.
    let effect = row(&state, "lighting:", "effect");
    assert_eq!(effect["control"]["value"], "rain");
    assert!(effect["subtitle"].as_str().unwrap().contains("FN+1"));
    assert_eq!(
        row(&state, "lighting:", "speed")["control"]["value"],
        "slow"
    );
    assert_eq!(
        row(&state, "lighting:", "brightness")["control"]["value"],
        70.0
    );
    assert_eq!(
        row(&state, "device:", "battery")["subtitle"],
        "42 % · On battery"
    );
    assert_eq!(
        row(&state, "device:", "firmware")["subtitle"],
        "Keyboard 05 1A 9A · Receiver 01 44"
    );
    // The K65 PLUS refuses polling changes, so its rate is shown without a control.
    let polling = row(&state, "device:", "polling");
    assert!(polling.get("control").is_none_or(Value::is_null));
    assert!(polling["subtitle"].as_str().unwrap().contains("fixed"));
    assert_eq!(row(&state, "device:", "sleep")["control"]["value"], "unset");
    assert!(
        group(&state, "diagnostics:")["collapsed"]
            .as_bool()
            .unwrap()
    );
    // State reads only: no SET at all, and the mode is back to hardware.
    assert!(fake.sets(PROP_BRIGHTNESS).is_empty());
    assert_eq!(fake.device.borrow().mode, MODE_HARDWARE);
}

#[test]
fn refresh_never_switches_mode_once_the_effect_is_cached() {
    let fake = Fake::k65("cadence");
    let opens = |fake: &Fake| fake.log().iter().filter(|p| p[0] == CMD_OPEN).count();
    // Nothing cached yet: one read.
    fake.state(false);
    assert_eq!(opens(&fake), 1);
    // From then on a refresh only GETs, however old the cached effect is.
    for later in [30, 61, 3600, 86_400] {
        fake.clear_log();
        fake.now.set(NOW + later);
        fake.state(false);
        assert!(fake.log().iter().all(|p| p[0] == crate::bragi::CMD_GET));
    }
}

#[test]
fn visualization_follows_host_features() {
    let fake = Fake::k65("viz");
    let without = fake.state(false);
    for id in ["lighting:", "remaps:"] {
        assert!(
            group(&without, id)
                .get("visualization")
                .is_none_or(Value::is_null)
        );
    }
    let with = fake.state(true);
    // The Lighting picture previews the onboard effect and never offers remapping.
    let lighting = &group(&with, "lighting:")["visualization"];
    check_visualization(lighting);
    assert_eq!(lighting["status"], "Rain (slow)");
    assert_eq!(
        lighting["animation"],
        json!({"kind": "rain", "speed": "slow", "colors": []})
    );
    assert!(lighting.get("remap").is_none() && lighting.get("paint").is_none());
    let viz = &group(&with, "remaps:")["visualization"];
    check_visualization(viz);
    assert_eq!(viz["name"], with["products"][0]["name"]);
    assert_eq!(viz["stream"]["args"], json!(["--input-stream", fake.id()]));
    // Receiver link: no per-key paint; input rule present: shared remap options.
    assert!(viz.get("paint").is_none());
    assert!(viz["remap"]["options"].as_array().unwrap().len() > 100);
    let keys = viz["keys"].as_array().unwrap();
    assert!(keys.iter().any(|key| key["id"] == "key-1"));
    assert!(keys.iter().all(|key| key["color"].is_null()));
    // Without the input rule there is no remap block.
    let mut no_rule = Fake::k65("viz-norule");
    no_rule.input_rule = false;
    let state = no_rule.state(true);
    let viz = &group(&state, "remaps:")["visualization"];
    assert!(viz.get("remap").is_none());
    assert!(
        viz["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|key| key.get("remap").is_none())
    );
}

#[test]
fn k100_state_offers_live_lighting_and_paint() {
    let fake = Fake::new("k100", Model::K100Rgb, LinkKind::Usb);
    let state = fake.state(true);
    check_state(&state);
    assert!(has_group(&state, "live-lighting:"));
    assert!(!has_group(&state, "looks:"));
    assert!(!has_group(&state, "dial:"));
    let viz = &group(&state, "lighting:")["visualization"];
    assert_eq!(viz["paint"]["action"], "set-key-colors");
    assert!(viz.get("animation").is_none());
    assert!(
        group(&state, "remaps:")["visualization"]
            .get("paint")
            .is_none()
    );
    assert!(row(&state, "live-lighting:", "zone-edge")["control"]["kind"] == "color");
    assert_eq!(
        row(&state, "device:", "polling")["control"]["options"]
            .as_array()
            .unwrap()
            .len(),
        7
    );
    // No effect readback on the K100 (onboard effects belong to iCUE).
    assert!(fake.log().iter().all(|p| p[0] != CMD_OPEN));
    // Brightness writes both 0x02 and 0x6D in one software-mode dip.
    fake.action("set-brightness", json!({"value": 40})).unwrap();
    assert_eq!(fake.sets(PROP_BRIGHTNESS), [400]);
    assert_eq!(fake.sets(PROP_BRIGHTNESS_2), [400]);
    assert_eq!(fake.sets(PROP_MODE), [2, 1]);
    assert!(
        fake.action("set-effect", json!({"value": "rain"}))
            .unwrap_err()
            .to_string()
            .contains("iCUE")
    );
}

#[test]
fn asleep_keyboard_is_isolated_and_not_touched() {
    let fake = Fake::k65("asleep");
    fake.receiver.borrow_mut().props.insert(PROP_CHILDREN, 0);
    let state = fake.state(true);
    check_state(&state);
    assert_eq!(state["products"][0]["status"], "Asleep — press a key");
    assert!(state["products"][0]["battery"].is_null());
    assert_eq!(fake.opens.get(), 0, "no keyboard I/O while asleep");
    assert!(!has_group(&state, "lighting:"));
    assert!(has_group(&state, "remaps:"));
    // Offline status (6) from the keyboard itself also counts as asleep.
    let offline = Fake::k65("offline");
    offline.device.borrow_mut().offline = true;
    let state = offline.state(false);
    assert_eq!(state["products"][0]["status"], "Asleep — press a key");
}

#[test]
fn failed_read_becomes_an_info_group() {
    let mut fake = Fake::k65("failed");
    fake.open_error = Some("Keyboard busy — another process is using it".into());
    let state = fake.state(true);
    check_state(&state);
    let error = row(&state, "notice:", "error");
    assert!(
        error["subtitle"]
            .as_str()
            .unwrap()
            .ends_with("unplug and replug it.")
    );
    assert!(error["control"].is_null());
    assert_eq!(state["products"][0]["status"], "Unavailable");
}

#[test]
fn no_keyboards_state_is_valid() {
    let mut fake = Fake::k65("none");
    fake.keyboards.clear();
    let state = fake.state(true);
    check_state(&state);
    assert!(state["products"].as_array().unwrap().is_empty());
    assert!(
        row(&state, "no-keyboards", "unsupported-1bab")["title"]
            .as_str()
            .unwrap()
            .contains("K100 AIR")
    );
}

#[test]
fn daemon_owned_keyboard_reports_live_mode() {
    let fake = Fake::k65("owned");
    fake.device.borrow_mut().mode = 2;
    *fake.daemon.borrow_mut() = Some(vec![DeviceStatus {
        id: fake.id(),
        owned: true,
        mode: 2,
        grabbed: false,
        features: vec!["dial".into()],
        error: None,
        generation: None,
        frame_applied: None,
        remaps_active: None,
        config_revision: None,
    }]);
    std::fs::create_dir_all(fake.unit.parent().unwrap()).unwrap();
    std::fs::write(&fake.unit, "x").unwrap();
    let state = fake.state(true);
    check_state(&state);
    assert_eq!(state["products"][0]["status"], "Connected");
    let status = row(&state, "live-mode:", "status")["subtitle"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        status.starts_with("Running · driving this keyboard"),
        "{status}"
    );
    // Already in software mode: no mode switching for the effect read.
    assert!(fake.sets(PROP_MODE).is_empty());
    assert!(
        fake.action("rescue", json!({}))
            .unwrap_err()
            .to_string()
            .contains("switch that")
    );
}

// ---------------------------------------------------------------------------------------------
// Actions

#[test]
fn brightness_is_read_compared_and_budgeted() {
    let fake = Fake::k65("bright");
    // Prime the effect cache so the states returned by actions need no software-mode dip.
    fake.state(false);
    fake.clear_log();
    let before = crate::storage::WriteBudget::used_today(&fake.id());
    fake.action("set-brightness", json!({"value": 70})).unwrap();
    assert!(
        fake.sets(PROP_BRIGHTNESS).is_empty(),
        "identical value must be skipped"
    );
    assert!(fake.sets(PROP_MODE).is_empty());
    let state = fake.action("set-brightness", json!({"value": 55})).unwrap();
    assert_eq!(fake.sets(PROP_BRIGHTNESS), [550]);
    assert_eq!(fake.sets(PROP_MODE), [2, 1]);
    assert_eq!(fake.device.borrow().mode, MODE_HARDWARE);
    assert_eq!(
        row(&state, "lighting:", "brightness")["control"]["value"],
        55.0
    );
    assert_eq!(
        crate::storage::WriteBudget::used_today(&fake.id()),
        before + 1
    );
    fake.action("brightness-preset", json!({"value": "off"}))
        .unwrap();
    assert_eq!(fake.sets(PROP_BRIGHTNESS), [550, 0]);
}

#[test]
fn effect_changes_write_the_slot_and_skip_identical() {
    let fake = Fake::k65("effect");
    // Fake holds Rain slow: asking for Rain keeps the speed and writes nothing.
    fake.action("set-effect", json!({"value": "rain"})).unwrap();
    assert!(fake.device.borrow().written.is_empty());
    let state = fake
        .action("set-color", json!({"value": "#FF0000"}))
        .unwrap();
    let written = fake
        .device
        .borrow()
        .written
        .get(&RES_K65_EFFECT)
        .cloned()
        .unwrap();
    // [declared len u32 = 93][7e 20 01][00 00 00 01 ff B G R] + padding.
    assert_eq!(&written[..4], &93u32.to_le_bytes());
    assert_eq!(
        &written[4..15],
        &[0x7e, 0x20, 0x01, 0, 0, 0, 1, 0xff, 0, 0, 0xff]
    );
    assert_eq!(written.len(), 4 + 3 + 93);
    assert_eq!(fake.device.borrow().mode, MODE_HARDWARE);
    assert!(fake.device.borrow().open.is_empty(), "handle closed");
    // The state after the write uses the cached value: Static red with a colour row.
    assert_eq!(
        row(&state, "lighting:", "effect")["control"]["value"],
        "static"
    );
    assert_eq!(
        row(&state, "lighting:", "color")["control"]["value"],
        "#FF0000"
    );
    let viz = &group(&state, "lighting:")["visualization"];
    assert!(viz.get("animation").is_none());
    assert!(
        viz["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|key| key["color"] == "#FF0000")
    );
    // Speed on Static is refused inside the dip, and hardware mode is restored.
    fake.device
        .borrow_mut()
        .resources
        .insert(RES_K65_EFFECT, written[4..15].to_vec());
    let error = fake
        .action("set-speed", json!({"value": "fast"}))
        .unwrap_err();
    assert!(error.to_string().contains("no speed"), "{error:#}");
    assert_eq!(fake.device.borrow().mode, MODE_HARDWARE);
}

#[test]
fn looks_save_apply_delete() {
    let fake = Fake::k65("looks");
    let state = fake
        .action("save-look", json!({"value": {"name": "Rainy"}}))
        .unwrap();
    check_state(&state);
    assert_eq!(
        row(&state, "looks:", "apply-look")["control"]["value"],
        "Rainy"
    );
    fake.action("set-brightness", json!({"value": 20})).unwrap();
    fake.action("apply-look", json!({"value": "Rainy"}))
        .unwrap();
    assert_eq!(fake.sets(PROP_BRIGHTNESS), [200, 700]);
    let state = fake
        .action("delete-look", json!({"value": {"look": "Rainy"}}))
        .unwrap();
    assert!(row(&state, "looks:", "no-looks")["control"].is_null());
    assert!(
        fake.action("delete-look", json!({"value": {"look": "Rainy"}}))
            .is_err()
    );
}

#[test]
fn win_lock_sets_mask_then_enable() {
    let fake = Fake::k65("lock");
    let state = fake.action("set-win-lock", json!({"value": true})).unwrap();
    assert_eq!(fake.sets(PROP_LOCK_MASK), [1]);
    assert_eq!(fake.sets(PROP_LOCK_ENABLE), [1]);
    assert_eq!(
        row(&state, "keys-lock:", "win-lock")["control"]["value"],
        true
    );
    fake.action("set-lock-mask", json!({"bit": 1, "value": true}))
        .unwrap();
    assert_eq!(fake.sets(PROP_LOCK_MASK), [1, 3]);
    fake.action("set-lock-mask", json!({"bit": 1, "value": true}))
        .unwrap();
    assert_eq!(
        fake.sets(PROP_LOCK_MASK),
        [1, 3],
        "unchanged mask is skipped"
    );
}

#[test]
fn sleep_is_write_only_and_cached() {
    let fake = Fake::k65("sleep");
    let state = fake.action("set-sleep", json!({"value": "15"})).unwrap();
    assert_eq!(fake.sets(PROP_SLEEP_MS), [900_000]);
    assert_eq!(row(&state, "device:", "sleep")["control"]["value"], "15");
    fake.action("set-sleep", json!({"value": "15"})).unwrap();
    assert_eq!(
        fake.sets(PROP_SLEEP_MS),
        [900_000],
        "cached value skips the write"
    );
}

#[test]
fn polling_is_capped_by_the_keyboard_maximum() {
    let fixed = Fake::k65("polling-fixed");
    let error = fixed
        .action("set-polling", json!({"value": "500"}))
        .unwrap_err();
    assert!(error.to_string().contains("fixed"), "{error:#}");
    assert!(fixed.sets(crate::bragi::PROP_POLLING).is_empty());
    let fake = Fake::new("polling", Model::K100Rgb, LinkKind::Usb);
    fake.action("set-polling", json!({"value": "500"})).unwrap();
    assert_eq!(fake.sets(crate::bragi::PROP_POLLING), [3]);
}

#[test]
fn remaps_and_macros_edit_live_config() {
    let fake = Fake::k65("remap");
    fake.state(false);
    fake.clear_log();
    let state = fake
        .action(
            "add-remap",
            json!({"value": {"key": "capslock", "action": "esc", "combo": ""}}),
        )
        .unwrap();
    check_state(&state);
    // The first feature that needs the helper starts it; no manual step.
    assert!(
        fake.systemctl
            .borrow()
            .iter()
            .any(|call| call.starts_with("enable --now"))
    );
    assert!(
        row(&state, "remaps:", "remap-capslock")["title"]
            .as_str()
            .unwrap()
            .contains("Esc")
    );
    let viz = &group(&state, "remaps:")["visualization"];
    let caps = viz["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["id"] == "capslock")
        .unwrap();
    assert_eq!(caps["remap"], "esc");
    // The keyboard was never touched by config edits.
    assert!(fake.sets(PROP_MODE).is_empty());
    fake.action(
        "save-macro",
        json!({"value": {"name": "Hello", "steps": "text:hello, enter"}}),
    )
    .unwrap();
    fake.action(
        "remap-key",
        json!({"value": {"key": "key-1", "target": "macro:Hello"}}),
    )
    .unwrap();
    let error = fake
        .action("delete-macro", json!({"name": "Hello"}))
        .unwrap_err();
    assert!(
        error.to_string().contains("used by remap of 1"),
        "{error:#}"
    );
    fake.action(
        "remap-key",
        json!({"value": {"key": "key-1", "target": "default"}}),
    )
    .unwrap();
    fake.action("delete-macro", json!({"name": "Hello"}))
        .unwrap();
    assert!(
        fake.action("remap-key", json!({"value": {"key": "fn", "target": "a"}}))
            .unwrap_err()
            .to_string()
            .contains("can't be remapped")
    );
    let state = fake
        .action("remove-remap", json!({"key": "capslock"}))
        .unwrap();
    assert!(
        group(&state, "remaps:")["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["id"] != "remap-capslock")
    );
    let state = fake.action("set-dial", json!({"value": "custom"})).unwrap();
    assert_eq!(
        row(&state, "dial:", "dial-mode")["control"]["value"],
        "custom"
    );
    check_control(&row(&state, "dial:", "dial-custom")["control"]);
    fake.action("set-dial", json!({"value": "volume"})).unwrap();
}

#[test]
fn per_key_lighting_needs_a_wired_board() {
    let fake = Fake::k65("wireless-paint");
    let error = fake
        .action(
            "set-key-colors",
            json!({"value": {"keys": ["esc"], "color": "#00FF00"}}),
        )
        .unwrap_err();
    assert!(error.to_string().contains("USB cable"));
    let wired = Fake::new("wired-paint", Model::K65PlusWireless, LinkKind::Usb);
    let state = wired
        .action(
            "set-key-colors",
            json!({"value": {"keys": ["esc", "key-1"], "color": "#00FF00"}}),
        )
        .unwrap();
    check_state(&state);
    assert_eq!(
        row(&state, "live-lighting:", "enabled")["control"]["value"],
        true
    );
    let viz = &group(&state, "lighting:")["visualization"];
    let key = |id: &str| {
        viz["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(key("esc")["color"], "#00FF00");
    assert_eq!(key("f1")["color"], "#FFFFFF");
    assert!(viz["status"].as_str().unwrap().contains("not confirmed"));
    wired.action("clear-key-colors", json!({})).unwrap();
    wired
        .action("set-live-lighting", json!({"value": false}))
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Companion service

#[test]
fn service_unit_text_is_exact() {
    let text = service::unit_text(std::path::Path::new(
        "/opt/hyprdeck plugins/hyprdeck-corsair",
    ))
    .unwrap();
    assert!(text.contains("ExecStart=\"/opt/hyprdeck plugins/hyprdeck-corsair\" daemon\n"));
    assert!(
        text.contains("ExecStopPost=\"/opt/hyprdeck plugins/hyprdeck-corsair\" rescue --all\n")
    );
    assert!(text.contains("Restart=on-failure\n"));
    assert!(text.contains("WantedBy=default.target\n"));
    let plain = service::unit_text(std::path::Path::new("/usr/bin/hyprdeck-corsair")).unwrap();
    assert!(plain.contains("ExecStart=/usr/bin/hyprdeck-corsair daemon\n"));
    let percent = service::unit_text(std::path::Path::new("/a%b/x")).unwrap();
    assert!(percent.contains("ExecStart=/a%%b/x daemon"));
    assert!(service::unit_text(std::path::Path::new("/a\"b")).is_err());
    assert!(service::unit_text(std::path::Path::new("/a\nb")).is_err());
}

#[test]
fn helper_starts_when_a_feature_needs_it_and_stops_when_none_does() {
    let fake = Fake::k65("service");
    // No controls for the helper anywhere: it is managed for the user.
    let text = fake.state(false).to_string();
    for gone in [
        "enable-live-mode",
        "disable-live-mode",
        "restart-live-mode",
        "Live mode",
        "live mode",
    ] {
        assert!(!text.contains(gone), "{gone} is still shown");
    }
    assert!(fake.systemctl.borrow().is_empty());
    // Choosing a dial mode that needs the helper installs and starts it.
    fake.action("set-dial", json!({"value": "scroll"})).unwrap();
    let unit = std::fs::read_to_string(&fake.unit).unwrap();
    assert!(unit.contains(
        "ExecStart=/home/u/.local/share/hyprdeck/plugins/corsair/hyprdeck-corsair daemon"
    ));
    assert_eq!(
        *fake.systemctl.borrow(),
        ["daemon-reload", "enable --now hyprdeck-corsair.service"]
    );
    // A further change while it runs only asks it to reload.
    fake.action("set-dial", json!({"value": "zoom"})).unwrap();
    assert_eq!(fake.systemctl.borrow().len(), 2);
    assert_eq!(fake.reloads.get(), 1);
    // Once no keyboard has a feature that needs the helper, it is stopped.
    let nothing = crate::config::LiveConfig {
        version: crate::config::LIVE_VERSION,
        devices: Default::default(),
    };
    super::actions::sync_helper(&fake, &nothing).unwrap();
    assert_eq!(
        fake.systemctl.borrow().last().unwrap(),
        "disable --now hyprdeck-corsair.service"
    );
}

// ---------------------------------------------------------------------------------------------
// Key tester stream

struct FakeSources {
    daemon: RefCell<Option<Vec<Vec<String>>>>,
    evdev: RefCell<Vec<std::io::Result<std::os::fd::OwnedFd>>>,
    waited: Cell<bool>,
}

impl stream::Sources for FakeSources {
    fn daemon_keys(&self, _keyboard: &Keyboard) -> anyhow::Result<Option<stream::KeyFrames>> {
        Ok(self
            .daemon
            .borrow_mut()
            .take()
            .map(|frames| Box::new(frames.into_iter().map(Ok)) as stream::KeyFrames))
    }
    fn open_evdev(&self, _keyboard: &Keyboard) -> Vec<std::io::Result<std::os::fd::OwnedFd>> {
        std::mem::take(&mut *self.evdev.borrow_mut())
    }
    fn wait_for_reader_exit(&self) {
        self.waited.set(true);
    }
    fn hup_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }
}

fn sources() -> FakeSources {
    FakeSources {
        daemon: RefCell::new(None),
        evdev: RefCell::new(Vec::new()),
        waited: Cell::new(false),
    }
}

fn frames(out: &[u8]) -> Vec<Value> {
    String::from_utf8(out.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn stream_idles_with_null_frame_without_permission() {
    let fake = Fake::k65("stream-idle");
    let sources = sources();
    sources
        .evdev
        .borrow_mut()
        .push(Err(std::io::ErrorKind::PermissionDenied.into()));
    let mut out = Vec::new();
    stream::stream(&fake.keyboards[0], &mut out, &sources).unwrap();
    assert_eq!(
        frames(&out),
        [json!({"status": stream::IDLE_STATUS, "pressed": null})]
    );
    assert!(sources.waited.get());
}

#[test]
fn stream_maps_daemon_frames_to_visual_ids() {
    let fake = Fake::k65("stream-daemon");
    let sources = sources();
    *sources.daemon.borrow_mut() = Some(vec![vec!["a".into(), "1".into()], vec![]]);
    let mut out = Vec::new();
    let error = stream::stream(&fake.keyboards[0], &mut out, &sources).unwrap_err();
    assert!(error.to_string().contains("stopped"));
    let frames = frames(&out);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[1]["pressed"], json!(["a", "key-1"]));
    assert_eq!(frames[2]["pressed"], json!([]));
}

#[test]
fn stream_reads_evdev_events() {
    let fake = Fake::k65("stream-evdev");
    let mut fds = [0; 2];
    // SAFETY: valid two-element array for pipe(2).
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: pipe returned two fresh fds we now own.
    let (reader, writer) = unsafe {
        use std::os::fd::FromRawFd;
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    };
    let event = |type_: u16, code: u16, value: i32| {
        // SAFETY: input_event is plain old data.
        let mut event: libc::input_event = unsafe { std::mem::zeroed() };
        event.type_ = type_;
        event.code = code;
        event.value = value;
        // SAFETY: reading the bytes of a plain struct.
        unsafe {
            std::slice::from_raw_parts(
                (&event as *const libc::input_event).cast::<u8>(),
                std::mem::size_of::<libc::input_event>(),
            )
        }
        .to_vec()
    };
    let mut bytes = Vec::new();
    bytes.extend(event(1, 30, 1)); // KEY_A down
    bytes.extend(event(0, 0, 0));
    bytes.extend(event(1, 30, 2)); // repeat: no new frame
    bytes.extend(event(0, 0, 0));
    bytes.extend(event(1, 30, 0));
    bytes.extend(event(0, 0, 0));
    {
        use std::io::Write as _;
        let mut writer = writer;
        writer.write_all(&bytes).unwrap();
    }
    let sources = sources();
    sources.evdev.borrow_mut().push(Ok(reader));
    let mut out = Vec::new();
    let error = stream::stream(&fake.keyboards[0], &mut out, &sources).unwrap_err();
    assert!(error.to_string().contains("disconnected"));
    let frames = frames(&out);
    let pressed: Vec<Value> = frames.iter().map(|f| f["pressed"].clone()).collect();
    assert_eq!(pressed, [json!([]), json!(["a"]), json!([])]);
}

#[test]
fn stream_exits_cleanly_on_broken_pipe() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let fake = Fake::k65("stream-pipe");
    let sources = sources();
    *sources.daemon.borrow_mut() = Some(vec![vec!["a".into()]]);
    stream::stream(&fake.keyboards[0], &mut Broken, &sources).unwrap();
}

#[test]
fn layout_ids_round_trip_through_visual_ids() {
    for model in [Model::K65PlusWireless, Model::K100Rgb] {
        for physical in [
            crate::devices::PhysicalLayout::Ansi,
            crate::devices::PhysicalLayout::Iso,
        ] {
            for key in crate::layouts::layout(model, physical).keys {
                let viz = labels::viz_id(key.id);
                assert!(host_id(&viz), "{viz}");
                assert_eq!(labels::layout_id(&viz), key.id);
            }
        }
    }
}

#[test]
fn effect_failure_backs_off_automatic_refresh_and_invalidates_failed_mode_restore() {
    let fake = Fake::k65("effect-fail-cadence");
    fake.device
        .borrow_mut()
        .fail_cmd
        .insert(crate::bragi::CMD_OPEN, 3);
    fake.state(true);
    let count = fake.sets(PROP_MODE).len();
    fake.now.set(NOW + 2);
    fake.state(true);
    assert_eq!(fake.sets(PROP_MODE).len(), count);
    fake.device.borrow_mut().fail_cmd.clear();
    fake.now.set(NOW + 61);
    fake.state(true);
    assert!(fake.sets(PROP_MODE).len() > count);
    let stuck = Fake::k65("effect-restore-failure");
    stuck.device.borrow_mut().stuck = true;
    let state = stuck.state(true);
    assert_eq!(stuck.device.borrow().mode, 2);
    assert_ne!(state["products"][0]["status"], "Connected · Hardware mode");
}
