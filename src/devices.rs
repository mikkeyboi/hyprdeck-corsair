//! Supported models, capabilities and sysfs discovery. See docs/DESIGN.md §1, §5.
//!
//! OWNER: foundation. Shared interface.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const CORSAIR_VID: u16 = 0x1b1c;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Model {
    K65PlusWireless,
    K100Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkKind {
    /// Through the SLIPSTREAM receiver (target 0x09).
    Receiver,
    /// Direct USB cable (target 0x08).
    Usb,
}

/// Physical layout reported by GET 0x41.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalLayout {
    Ansi,
    Iso,
    Abnt,
    Jis,
}

impl PhysicalLayout {
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Ansi),
            2 => Some(Self::Iso),
            3 => Some(Self::Abnt),
            4 => Some(Self::Jis),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub onboard_effects: bool,
    pub brightness: bool,
    pub battery: bool,
    pub sleep_timeout: bool,
    pub win_lock: bool,
    /// Polling rates (Hz) the model can be offered, before capping by GET 0x96.
    pub polling_rates: &'static [u32],
    /// Per-key colours through the companion (resource 0x22) on this link.
    pub live_lighting: bool,
    /// Number of LEDs in the direct frame (0 when not supported).
    pub led_count: usize,
    /// Rotary dial (K65 PLUS) or control wheel (K100) that needs emulation in software mode.
    pub dial: bool,
    pub secondary_brightness: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyboard {
    /// Stable id, e.g. "k65plus-0123456789abcdef".
    pub id: String,
    pub model: Model,
    pub link: LinkKind,
    pub name: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    /// sysfs path of the USB device.
    pub usb_path: PathBuf,
    /// /dev/hidrawN of IF1 (usage page 0xFF42 usage 1).
    pub cmd_node: PathBuf,
    /// /dev/hidrawN of IF2 (events), if present.
    pub event_node: Option<PathBuf>,
    /// /dev/input/eventN nodes that carry the keyboard's keys (IF0 keyboard + consumer).
    pub evdev_nodes: Vec<PathBuf>,
    /// Output report payload length without report id: 64 or 1024.
    pub frame_len: usize,
    /// Bragi target byte: 0x09 behind a receiver, 0x08 direct.
    pub target: u8,
}

/// Env override for the sysfs root used by discovery (tests and smoke runs).
pub const ENV_SYSFS: &str = "HYPRDECK_CORSAIR_SYSFS";
/// Env override for the /dev root used to build node paths (tests and smoke runs).
pub const ENV_DEV: &str = "HYPRDECK_CORSAIR_DEV";

/// Hardware-verified: the K65 PLUS refuses SET polling (status 7); its rate is read-only.
const K65_POLLING: &[u32] = &[];
const K100_POLLING: &[u32] = &[125, 250, 500, 1000, 2000, 4000, 8000];

/// Report descriptor prefixes: usage page 0xFF42, usage 1 (commands) / usage 2 (events).
const CMD_DESCRIPTOR: [u8; 5] = [0x06, 0x42, 0xff, 0x09, 0x01];
const EVENT_DESCRIPTOR: [u8; 5] = [0x06, 0x42, 0xff, 0x09, 0x02];

impl Model {
    pub fn capabilities(self, link: LinkKind) -> Capabilities {
        match (self, link) {
            (Model::K65PlusWireless, LinkKind::Receiver) => Capabilities {
                onboard_effects: true,
                brightness: true,
                battery: true,
                sleep_timeout: true,
                win_lock: true,
                polling_rates: K65_POLLING,
                live_lighting: false,
                led_count: 0,
                dial: true,
                secondary_brightness: false,
            },
            (Model::K65PlusWireless, LinkKind::Usb) => Capabilities {
                onboard_effects: true,
                brightness: true,
                battery: true,
                sleep_timeout: true,
                win_lock: true,
                polling_rates: K65_POLLING,
                live_lighting: true,
                led_count: 123,
                dial: true,
                secondary_brightness: false,
            },
            (Model::K100Rgb, _) => Capabilities {
                onboard_effects: false,
                brightness: true,
                battery: false,
                sleep_timeout: false,
                win_lock: true,
                polling_rates: K100_POLLING,
                live_lighting: true,
                led_count: 193,
                dial: true,
                secondary_brightness: true,
            },
        }
    }
    pub fn display_name(self) -> &'static str {
        match self {
            Model::K65PlusWireless => "Corsair K65 PLUS Wireless",
            Model::K100Rgb => "Corsair K100 RGB",
        }
    }
    /// Command-channel frame length (payload without report id): 64 or 1024 ("jumbo").
    pub fn frame_len(self) -> usize {
        match self {
            Model::K65PlusWireless => 64,
            Model::K100Rgb => 1024,
        }
    }
    /// Prefix of the stable device id.
    pub fn id_prefix(self) -> &'static str {
        match self {
            Model::K65PlusWireless => "k65plus",
            Model::K100Rgb => "k100",
        }
    }
}

/// Identify a (vid, pid) pair; returns model and link kind for supported devices.
pub fn identify(vid: u16, pid: u16) -> Option<(Model, LinkKind)> {
    if vid != CORSAIR_VID {
        return None;
    }
    match pid {
        0x2b07 => Some((Model::K65PlusWireless, LinkKind::Receiver)),
        0x2b10 | 0x2b11 | 0x2b23 => Some((Model::K65PlusWireless, LinkKind::Usb)),
        0x1b7c | 0x1b7d | 0x1bc5 => Some((Model::K100Rgb, LinkKind::Usb)),
        _ => None,
    }
}

/// Corsair devices we recognise but do not support yet.
fn unsupported_name(pid: u16) -> Option<&'static str> {
    match pid {
        0x1bab => Some("Corsair K100 AIR Wireless (not supported yet)"),
        0x1ba6 => Some("Corsair SLIPSTREAM receiver for K100 AIR (not supported yet)"),
        _ => None,
    }
}

fn roots() -> (PathBuf, PathBuf) {
    let sys = std::env::var_os(ENV_SYSFS).map_or_else(|| PathBuf::from("/sys"), PathBuf::from);
    let dev = std::env::var_os(ENV_DEV).map_or_else(|| PathBuf::from("/dev"), PathBuf::from);
    (sys, dev)
}

/// Discover connected supported keyboards from the live sysfs tree
/// (honours `HYPRDECK_CORSAIR_SYSFS` / `HYPRDECK_CORSAIR_DEV`).
pub fn discover() -> Vec<Keyboard> {
    let (sys, dev) = roots();
    discover_in(&sys, &dev)
}

fn read_text(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn read_hex16(path: &Path) -> Option<u16> {
    u16::from_str_radix(&read_text(path)?, 16).ok()
}

/// Sorted directory entries (name, path); empty when the directory is missing.
fn entries(dir: &Path, prefix: &str) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.starts_with(prefix).then(|| (name, entry.path()))
        })
        .collect();
    out.sort_by_key(|(name, _)| natural_key(name));
    out
}

/// "hidraw10" sorts after "hidraw9".
fn natural_key(name: &str) -> (String, u64) {
    let digits = name.len() - name.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (stem, number) = name.split_at(name.len() - digits);
    (stem.to_string(), number.parse().unwrap_or(0))
}

/// Walk up from `start` (inclusive) until `stop`, returning the first dir containing `marker`.
fn ancestor_with(start: &Path, stop: &Path, marker: &str) -> Option<PathBuf> {
    start
        .ancestors()
        .take_while(|dir| dir.starts_with(stop) && *dir != stop)
        .find(|dir| dir.join(marker).exists())
        .map(Path::to_path_buf)
}

/// Lowercase, id-safe (`[a-z0-9_-]`) version of `text`.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Stable id: `<prefix>-<serial>` or `<prefix>-usb-<busnum>-<devpath>`; at most 64 bytes.
fn stable_id(model: Model, serial: &str, usb: &Path) -> String {
    let tail = if serial.is_empty() {
        let bus = read_text(&usb.join("busnum")).unwrap_or_else(|| "0".into());
        let devpath = read_text(&usb.join("devpath"))
            .or_else(|| {
                usb.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "0".into());
        format!("usb-{bus}-{devpath}")
    } else {
        serial.to_string()
    };
    let mut id = format!("{}-{}", model.id_prefix(), sanitize(&tail));
    id.truncate(64);
    id
}

#[derive(Default)]
struct UsbCandidate {
    vid: u16,
    pid: u16,
    cmd: Option<PathBuf>,
    event: Option<PathBuf>,
}

fn descriptor_kind(hid_dir: &Path, interface: Option<&Path>) -> Option<u8> {
    match fs::read(hid_dir.join("report_descriptor")) {
        Ok(bytes) if bytes.starts_with(&CMD_DESCRIPTOR) => Some(1),
        Ok(bytes) if bytes.starts_with(&EVENT_DESCRIPTOR) => Some(2),
        Ok(_) => None,
        // Unreadable descriptor: fall back to the documented interface numbers.
        Err(_) => match interface
            .and_then(|dir| read_text(&dir.join("bInterfaceNumber")))
            .as_deref()
        {
            Some("01") | Some("1") => Some(1),
            Some("02") | Some("2") => Some(2),
            _ => None,
        },
    }
}

/// Discovery against an arbitrary sysfs root and dev root (for tests with a fake tree).
pub fn discover_in(sys_root: &Path, dev_root: &Path) -> Vec<Keyboard> {
    let Ok(sys_canonical) = sys_root.canonicalize() else {
        return Vec::new();
    };
    let mut devices: BTreeMap<PathBuf, UsbCandidate> = BTreeMap::new();
    for (name, path) in entries(&sys_root.join("class/hidraw"), "hidraw") {
        let Ok(hid_dir) = path.join("device").canonicalize() else {
            continue;
        };
        let Some(usb) = ancestor_with(&hid_dir, &sys_canonical, "idVendor") else {
            continue;
        };
        let (Some(vid), Some(pid)) = (
            read_hex16(&usb.join("idVendor")),
            read_hex16(&usb.join("idProduct")),
        ) else {
            continue;
        };
        if identify(vid, pid).is_none() {
            continue;
        }
        let interface = ancestor_with(&hid_dir, &usb, "bInterfaceNumber");
        let node = dev_root.join(&name);
        let candidate = devices.entry(usb.clone()).or_default();
        candidate.vid = vid;
        candidate.pid = pid;
        match descriptor_kind(&hid_dir, interface.as_deref()) {
            Some(1) if candidate.cmd.is_none() => candidate.cmd = Some(node),
            Some(2) if candidate.event.is_none() => candidate.event = Some(node),
            _ => {}
        }
    }
    let inputs: Vec<(String, PathBuf)> = entries(&sys_root.join("class/input"), "event")
        .into_iter()
        .filter_map(|(name, path)| {
            let device = path.join("device");
            let canonical = device.canonicalize().ok()?;
            has_keys(&device).then_some((name, canonical))
        })
        .collect();
    let mut keyboards: Vec<Keyboard> = devices
        .into_iter()
        .filter_map(|(usb, candidate)| {
            let (model, link) = identify(candidate.vid, candidate.pid)?;
            let cmd_node = candidate.cmd?;
            let serial = read_text(&usb.join("serial"))
                .unwrap_or_default()
                .to_lowercase();
            let evdev_nodes = inputs
                .iter()
                .filter(|(_, canonical)| canonical.starts_with(&usb))
                .map(|(name, _)| dev_root.join("input").join(name))
                .collect();
            Some(Keyboard {
                id: stable_id(model, &serial, &usb),
                model,
                link,
                name: model.display_name(),
                vid: candidate.vid,
                pid: candidate.pid,
                serial,
                usb_path: usb,
                cmd_node,
                event_node: candidate.event,
                evdev_nodes,
                frame_len: model.frame_len(),
                target: if link == LinkKind::Receiver {
                    0x09
                } else {
                    0x08
                },
            })
        })
        .collect();
    keyboards.sort_by(|a, b| a.id.cmp(&b.id));
    keyboards.dedup_by(|a, b| a.id == b.id);
    keyboards
}

/// Whether an input device reports EV_KEY (missing capability file: assume yes).
fn has_keys(input_device: &Path) -> bool {
    match read_text(&input_device.join("capabilities/ev")) {
        Some(text) => {
            let last = text.split_whitespace().last().unwrap_or("0");
            u64::from_str_radix(last, 16).is_ok_and(|bits| bits & (1 << 1) != 0)
        }
        None => true,
    }
}

/// Find one keyboard by stable id.
pub fn find(id: &str) -> Result<Keyboard> {
    find_in(discover(), id)
}

fn find_in(keyboards: Vec<Keyboard>, id: &str) -> Result<Keyboard> {
    match keyboards.into_iter().find(|keyboard| keyboard.id == id) {
        Some(keyboard) => Ok(keyboard),
        None => bail!("Keyboard {id} is not connected — plug it in or refresh"),
    }
}

/// Recognised but unsupported Corsair devices (e.g. K100 AIR) for an informational row.
pub fn unsupported_corsair_devices() -> Vec<(u16, String)> {
    unsupported_in(&roots().0)
}

/// Unsupported-device scan against an arbitrary sysfs root.
pub fn unsupported_in(sys_root: &Path) -> Vec<(u16, String)> {
    entries(&sys_root.join("bus/usb/devices"), "")
        .into_iter()
        .filter_map(|(_, path)| {
            if read_hex16(&path.join("idVendor"))? != CORSAIR_VID {
                return None;
            }
            let pid = read_hex16(&path.join("idProduct"))?;
            Some((pid, unsupported_name(pid)?.to_string()))
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    pub(crate) struct TempDir(pub PathBuf, fs::File);

    impl TempDir {
        pub fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyprdeck-corsair-{tag}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            let directory = fs::File::open(&path).unwrap();
            Self(path, directory)
        }

        /// A short alias into this directory, keeping Unix socket paths below SUN_LEN even
        /// when TMPDIR is a deeply nested scratchpad. The directory fd stays live with Self.
        pub fn short_path(&self, name: &str) -> PathBuf {
            use std::os::fd::AsRawFd;
            PathBuf::from(format!("/proc/self/fd/{}", self.1.as_raw_fd())).join(name)
        }
    }

    #[test]
    fn socket_fixture_supports_long_temp_paths() {
        let tmp = TempDir::new(&"long".repeat(32));
        let path = tmp.short_path("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(tmp.0.join("daemon.sock").exists());
        let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let _accepted = listener.accept().unwrap();
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: impl AsRef<[u8]>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// Builds a fake sysfs USB device with HID interfaces and input nodes.
    struct FakeUsb<'a> {
        sys: &'a Path,
        usb: PathBuf,
        port: String,
    }

    impl<'a> FakeUsb<'a> {
        fn new(sys: &'a Path, port: &str, vid: &str, pid: &str, serial: Option<&str>) -> Self {
            let usb = sys.join("devices/pci0000:00/0000:00:14.0/usb1").join(port);
            write(&usb.join("idVendor"), format!("{vid}\n"));
            write(&usb.join("idProduct"), format!("{pid}\n"));
            write(&usb.join("busnum"), "1\n");
            write(
                &usb.join("devpath"),
                format!("{}\n", port.trim_start_matches("1-")),
            );
            if let Some(serial) = serial {
                write(&usb.join("serial"), format!("{serial}\n"));
            }
            let link = sys.join("bus/usb/devices").join(port);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&usb, link).unwrap();
            Self {
                sys,
                usb,
                port: port.to_string(),
            }
        }
        fn interface(&self, number: u8) -> PathBuf {
            let dir = self.usb.join(format!("{}:1.{number}", self.port));
            write(&dir.join("bInterfaceNumber"), format!("{number:02}\n"));
            dir
        }
        fn hidraw(&self, number: u8, hidraw: &str, descriptor: &[u8]) {
            let hid = self
                .interface(number)
                .join(format!("0003:1B1C:2B07.000{number}"));
            write(&hid.join("report_descriptor"), descriptor);
            fs::create_dir_all(hid.join("hidraw").join(hidraw)).unwrap();
            let class = self.sys.join("class/hidraw").join(hidraw);
            fs::create_dir_all(&class).unwrap();
            symlink(&hid, class.join("device")).unwrap();
        }
        fn event(&self, number: u8, event: &str, ev_bits: &str) {
            let input = self
                .interface(number)
                .join(format!("0003:1B1C:2B07.010{number}/input/input{number}"));
            write(&input.join("capabilities/ev"), format!("{ev_bits}\n"));
            let class = self.sys.join("class/input").join(event);
            fs::create_dir_all(&class).unwrap();
            symlink(&input, class.join("device")).unwrap();
        }
    }

    const CMD: &[u8] = &[0x06, 0x42, 0xff, 0x09, 0x01, 0xa1, 0x01];
    const EVT: &[u8] = &[0x06, 0x42, 0xff, 0x09, 0x02, 0xa1, 0x01];
    const KBD: &[u8] = &[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01];

    #[test]
    fn identifies_supported_models() {
        assert_eq!(
            identify(0x1b1c, 0x2b07),
            Some((Model::K65PlusWireless, LinkKind::Receiver))
        );
        for pid in [0x2b10, 0x2b11, 0x2b23] {
            assert_eq!(
                identify(0x1b1c, pid),
                Some((Model::K65PlusWireless, LinkKind::Usb))
            );
        }
        for pid in [0x1b7c, 0x1b7d, 0x1bc5] {
            assert_eq!(identify(0x1b1c, pid), Some((Model::K100Rgb, LinkKind::Usb)));
        }
        assert_eq!(identify(0x1b1c, 0x1bab), None);
        assert_eq!(identify(0x1532, 0x2b07), None);
    }

    #[test]
    fn capabilities_follow_the_model_table() {
        let receiver = Model::K65PlusWireless.capabilities(LinkKind::Receiver);
        assert!(receiver.onboard_effects && receiver.brightness && receiver.battery);
        assert!(receiver.sleep_timeout && receiver.win_lock && receiver.dial);
        assert!(!receiver.live_lighting && !receiver.secondary_brightness);
        assert!(receiver.polling_rates.is_empty());
        let wired = Model::K65PlusWireless.capabilities(LinkKind::Usb);
        assert!(wired.live_lighting && wired.battery && wired.onboard_effects);
        assert_eq!(wired.led_count, 123);
        let k100 = Model::K100Rgb.capabilities(LinkKind::Usb);
        assert!(
            k100.brightness
                && k100.secondary_brightness
                && k100.win_lock
                && k100.live_lighting
                && k100.dial
        );
        assert!(!k100.onboard_effects && !k100.battery && !k100.sleep_timeout);
        assert_eq!(k100.led_count, 193);
        assert_eq!(k100.polling_rates.last(), Some(&8000));
        assert_eq!(Model::K100Rgb.frame_len(), 1024);
        assert_eq!(Model::K65PlusWireless.frame_len(), 64);
    }

    #[test]
    fn discovers_receiver_with_command_event_and_evdev_nodes() {
        let tmp = TempDir::new("discover");
        let sys = tmp.0.join("sys");
        let usb = FakeUsb::new(&sys, "1-2", "1b1c", "2b07", Some("0123456789ABCDEF"));
        usb.hidraw(0, "hidraw2", KBD);
        usb.hidraw(1, "hidraw3", CMD);
        usb.hidraw(2, "hidraw4", EVT);
        usb.event(0, "event7", "120013");
        usb.event(2, "event9", "10"); // no EV_KEY
        // An unrelated device.
        let other = FakeUsb::new(&sys, "1-3", "046d", "c52b", Some("x"));
        other.hidraw(1, "hidraw5", CMD);
        let keyboards = discover_in(&sys, Path::new("/dev"));
        assert_eq!(keyboards.len(), 1);
        let keyboard = &keyboards[0];
        assert_eq!(keyboard.id, "k65plus-0123456789abcdef");
        assert_eq!(keyboard.model, Model::K65PlusWireless);
        assert_eq!(keyboard.link, LinkKind::Receiver);
        assert_eq!(keyboard.target, 0x09);
        assert_eq!(keyboard.frame_len, 64);
        assert_eq!(keyboard.pid, 0x2b07);
        assert_eq!(keyboard.cmd_node, PathBuf::from("/dev/hidraw3"));
        assert_eq!(keyboard.event_node, Some(PathBuf::from("/dev/hidraw4")));
        assert_eq!(
            keyboard.evdev_nodes,
            vec![PathBuf::from("/dev/input/event7")]
        );
        assert_eq!(
            keyboard.usb_path,
            sys.join("devices/pci0000:00/0000:00:14.0/usb1/1-2")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            find_in(keyboards.clone(), "k65plus-0123456789abcdef").unwrap(),
            keyboards[0]
        );
        assert!(find_in(keyboards, "k100-nope").is_err());
    }

    #[test]
    fn discovery_requires_the_vendor_descriptor_and_falls_back_to_bus_path_ids() {
        let tmp = TempDir::new("discover-k100");
        let sys = tmp.0.join("sys");
        let usb = FakeUsb::new(&sys, "1-4.1", "1b1c", "1b7c", None);
        usb.hidraw(1, "hidraw10", CMD);
        let wrong = FakeUsb::new(&sys, "1-5", "1b1c", "2b10", Some("ABC"));
        wrong.hidraw(1, "hidraw11", KBD); // IF1 without the 0xFF42 usage-1 descriptor
        let keyboards = discover_in(&sys, Path::new("/fake-dev"));
        assert_eq!(keyboards.len(), 1);
        let k100 = &keyboards[0];
        assert_eq!(k100.id, "k100-usb-1-4-1");
        assert_eq!(k100.model, Model::K100Rgb);
        assert_eq!(k100.link, LinkKind::Usb);
        assert_eq!(k100.target, 0x08);
        assert_eq!(k100.frame_len, 1024);
        assert_eq!(k100.cmd_node, PathBuf::from("/fake-dev/hidraw10"));
        assert_eq!(k100.event_node, None);
    }

    #[test]
    fn empty_or_missing_sysfs_finds_nothing() {
        let tmp = TempDir::new("discover-empty");
        assert!(discover_in(&tmp.0, Path::new("/dev")).is_empty());
        assert!(discover_in(&tmp.0.join("missing"), Path::new("/dev")).is_empty());
        assert!(unsupported_in(&tmp.0).is_empty());
    }

    #[test]
    fn reports_unsupported_corsair_devices() {
        let tmp = TempDir::new("unsupported");
        let sys = tmp.0.join("sys");
        FakeUsb::new(&sys, "1-6", "1b1c", "1bab", Some("s"));
        FakeUsb::new(&sys, "1-7", "1b1c", "2b07", Some("t"));
        let found = unsupported_in(&sys);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 0x1bab);
        assert!(found[0].1.contains("K100 AIR"));
    }

    #[test]
    fn ids_are_host_safe() {
        assert_eq!(sanitize("AB:cd/1"), "ab-cd-1");
        let tmp = TempDir::new("ids");
        let id = stable_id(Model::K100Rgb, &"x".repeat(100), &tmp.0);
        assert_eq!(id.len(), 64);
        assert!(id.starts_with("k100-"));
    }
}
