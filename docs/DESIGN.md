# hyprdeck-corsair — design (authoritative build spec)

Status: v0.1 build spec, 2026-11-08. Every protocol fact here is either hardware-verified on the
user's K65 PLUS Wireless (see `docs/hardware-evidence-k65plus.md`, tag **[HW]**) or taken from the
clean-room research (tag **[R]**, unverified on hardware). OpenLinkHub (GPL-3.0), ckb-next and
OpenRGB (GPL-2.0) are protocol references only: **never copy their code or data files**; restate facts.

Repo: `~/source/hyprdeck-corsair` (Rust 2024, MIT, `publish = false`), one binary `hyprdeck-corsair`.
Host: `~/source/hyprdeck` (GTK4/libadwaita). Plugin contract: `~/source/hyprdeck/docs/PLUGINS.md`.
Reference plugin to mirror for style/conventions: `~/source/hyprdeck-razer`.

## 1. Product goals

Feature-rich and intuitive control of the user's Corsair keyboards, with **hardware (onboard) mode as
the default**. Software ("live") mode is used only:
- for a fraction of a second to apply/read a setting, then hardware mode is restored; or
- continuously, only while a user-enabled live feature needs it (custom dial actions on the K65 PLUS,
  per-key lighting on wired boards).

Remaps and macros do **not** use software mode: they run in the opt-in companion on the OS input layer
(evdev grab + uinput). Nothing is ever written to keyboard key tables, flash directories, pairing or
firmware. Writing remaps/macros to onboard memory ("flashing") is out of scope until iCUE's format is
reverse-engineered from a USB capture (see §11).

Supported models (v0.1):

| Model | Link | VID:PID | Frame | Target | Evidence |
|---|---|---|---|---|---|
| K65 PLUS Wireless | SLIPSTREAM receiver | 1b1c:2b07 (IF1 cmd, IF2 events) | 64 | 0x09 (child) | [HW] |
| K65 PLUS Wireless | USB cable | 1b1c:2b10 / 2b11 / 2b23 | 64 | 0x08 | [R] |
| K100 RGB (OPX v1 / MX / OPX v2) | USB | 1b1c:1b7c / 1b7d / 1bc5 | 1024 | 0x08 | [R] |

K100 AIR is recognised in discovery only as "not supported yet" (no controls).

## 2. Bragi protocol facts the code relies on

Transport: hidraw node of USB interface **1** (usage page 0xFF42 usage 1) for commands; interface
**2** (0xFF42 usage 2, IN-only, 64-byte reports) for events. Write `frame_len + 1` bytes:
`[0x00 report id, target, cmd, args…, zero pad]`. Read replies on the same node: `r[0]` = 0x00
(direct/receiver) or 0x01 (child behind receiver), `r[1]` = echoed cmd, `r[2]` = status, `r[3..]` =
value/data. Match replies by `r[1] == cmd`; drain stale input before each write; 500 ms reply timeout.

Status: 0 ok, 1 invalid value, 3 failed/busy (receiver: unsupported), 5 unsupported,
6 **child offline** (keyboard asleep/disconnected), 7 not allowed (e.g. SET or resource I/O in hardware
mode). Map 6 to a friendly "Keyboard is asleep or disconnected — press a key" error.

Commands: `01 prop 00 <value LE>` SET; `02 prop` GET (value LE at r[3..]); `0d h resLo resHi` open
handle; `09 h` probe (size u32 LE at r[5..9]); `08 h` read (data r[3..64]); `06 h <len u32 LE> data`
write first chunk; `07 h data` continuation; `05 01 h` close; `12` ping/keepalive.

**Hardware-mode rule [HW]:** in mode 1 every SET and every resource open/probe/read/write returns 7.
The working pattern is: GET 03 → if 1, SET 03=2 → operation(s) → close handles → SET 03=1 → verify
GET 03 == 1. Entering software mode does not disturb the running onboard effect; FN shortcuts and
typing keep working in software mode [HW].

Properties (GET in any mode; SET only in software mode):

| prop | meaning | width | notes |
|---|---|---|---|
| 0x01 | polling rate code 1=125,2=250,3=500,4=1000,5=2000,6=4000,7=8000 Hz | 1 | K65 max 4 (prop 0x96) [HW read]; SET unverified, device may re-enumerate |
| 0x02 | brightness 0..1000 | 2 | SET persists into hardware mode and across power cycle [HW]; FN+F5/F6 changes it and GET sees it [HW] |
| 0x03 | mode 1 hardware / 2 software | 1 | volatile [R] |
| 0x0E | sleep timeout ms | 4 | K65: SET only, GET returns 7 [HW]; cache last written value |
| 0x0F | battery per-mille | 2 | K65 [HW] |
| 0x10 | charge state 0 not charging,1 charging,2 not charging (battery),3 full | 1 | [HW]/[R] |
| 0x11/0x12 | VID / PID | 2 | child PID 0x2b10 behind receiver [HW] |
| 0x13 | app firmware bytes | 3–4 | K65 `05 1a 9a` (display as hex triple + raw) |
| 0x14 | bootloader | 3 | |
| 0x15 | radio fw / hw rev | 2 | |
| 0x36 | receiver: bitmask of connected children (GET on target 0x08) | 1 | 0x02 = keyboard online [HW] |
| 0x41 | physical layout 1 ANSI,2 ISO,3 ABNT,4 JIS | 1 | [HW] |
| 0x45 | Win-lock enable 0/1 | 1 | [HW] |
| 0x4A | lock mask bit0 Win, bit1 Alt+Tab, bit2 Alt+F4, bit3 Shift+Tab | 1 | [HW] |
| 0x6D | K100 RGB secondary brightness, written with 0x02 | 2 | [R] |
| 0x96 | max polling code | 1 | [HW read] |

Resources:
- **0x6D60** (K65 PLUS): the live onboard effect slot [HW]. FN+1..0 rewrite it; host writes show
  immediately in hardware mode and survive power cycles. Read/write only inside a software-mode dip.
  Write framing (OLH-proven on this unit): open h1 → wait 500 ms → probe → `06 01` + first 61 bytes of
  `[len u16 LE, 0, 0, header…, data…]` → `07 01` + next 61-byte chunks → close. `len` = data length
  (header excluded), data padded with zeros to 93 bytes. Firmware stores a compact copy
  (Rain read back as 4 bytes `7e a0 02 03`, Watercolor as 11 bytes).
- **0x0022** direct per-key frame `12 00 R G B …` (K100 RGB 193 LEDs, K65 wired 123 LEDs) — software
  mode only, needs keepalive; used only by the companion.
- **Forbidden:** reading 0x0002 (dropped the keyboard off the receiver [HW]); writing 0x0002 (key table:
  dead-key risk, no backup possible); 0x0005/0x0006 (pairing); 0x0013 directory; commands 0x0B, 0x15;
  SET 0x3A (K100 AIR Bluetooth switch); any property/command not in the allowlists below.

Effect descriptor (bytes as stored, little-endian id) [HW for Static/Rain/Watercolor, R for others]:

| effect (FN key) | id | header written | data (before zero padding) |
|---|---|---|---|
| Watercolor (FN+1) | 0x0022 | `22 00 03 <speed>` | `00 00 01 ff ff ff ff` |
| Spiral Rainbow (FN+2) | 0xAB87 | `87 ab 00 <speed> 06` | — |
| Rain (FN+3) | 0xA07E | `7e a0 02 <speed> 01` | — |
| Rainbow Wave (FN+4) | 0xB94C | `4c b9 00 <speed> 04` | — |
| Type Lighting – Key (FN+5) | 0xB1F9 | `f9 b1 02 <speed>` | — |
| Type Lighting – Ripple (FN+6) | 0x09A2 | `a2 09 02 <speed>` | — |
| Color Shift (FN+7) | 0xA5FA | `fa a5 02 <speed>` | — |
| Color Pulse (FN+8) | 0xAD4F | `4f ad 02 <speed>` | — |
| Color Wave (FN+9) | 0x7BFF | `ff 7b 02 <speed> 04` | — |
| Static (FN+0) | 0x207E | `7e 20 01` | `00 00 00 01 ff B G R` |

Speed byte: 3 slow, 4 medium, 5 fast. Stored layout for decoding: `[id lo, id hi, colour mode, speed,
direction, 0, count, (ff B G R)×count]`, truncated (firmware may store fewer bytes). Colour modes:
0 none/rainbow, 1 custom, 2 random, 3 default/white. Decode unknown ids as "Custom (0xNNNN)" + raw hex.
"Lights off" = brightness 0 (keeps the effect).

Events (IF2) [HW]: `01 02 <bitmap>`: bit n = Bragi key index = HID usage for normal keys
(a=4 … space=44, F6=63), modifiers 105..112 (105 LCtrl,106 LShift,107 LAlt,108 LGui,109 RCtrl,
110 RShift,111 RAlt,112 RGui), Fn 122, dial press 137. `01 05 60 00 <i32 LE>` dial rotation (+1 cw,
−1 ccw). In **hardware mode IF2 is silent** (no notifications for FN changes, dial or battery) → the
host must poll/read back. In software mode the K65 dial no longer reaches the OS; the companion must
emulate it.

Keepalive in software mode: ping `12` every 5 s via receiver, 15 s wired (watchdog ~60 s [R]).

## 3. Safety rules (enforced in code, not just convention)

1. `bragi` has explicit allowlists per model: GET props, SET props (0x01 only with a value ≤ GET 0x96,
   0x02, 0x03 ∈ {1,2}, 0x0E, 0x45 ∈ {0,1}, 0x4A ≤ 0x0F, 0x6D on K100), resources (read: 0x6D60;
   write: 0x6D60, 0x0022), commands {01,02,05,06,07,08,09,0D,12}. Anything else is a programming error
   returned as `Err` before any byte is written. Unit-tested.
2. Every software-mode entry is scoped (`with_software_mode`) and restores mode 1 on success, error and
   panic-unwind; it closes all tracked/uncertain handles before restoring. If the mode was already 2 (companion owns it),
   the scope neither enters nor exits.
3. Read-compare-skip: before any SET, GET the current value and skip identical writes (except write-only
   0x0E, which compares with the cached last-written value). Effect writes skip when the decoded
   current effect equals the requested one.
4. Write budget: per-device counter in `$XDG_STATE_HOME/hyprdeck-corsair/writes.json`; refuse more than
   500 persistent writes per UTC day with a clear error.
5. Never leave a handle open; never keep hidraw open in the plugin beyond one request.
6. Status 6 / timeouts never trigger retries that write; reads may retry once after 200 ms.
7. Rescue (`hyprdeck-corsair rescue [--all|<id>]`): independently close handles 0, 1 and 2, SET 03=1, verify; aggregate failures and exit nonzero if any target fails. Exposed as
   a button and as the companion's `ExecStopPost`.

## 4. Architecture and module ownership

```
src/main.rs          CLI: --request | --input-stream <id> | daemon | rescue [--all|<id>] | state | action <name> [json] | --help | --version
src/bragi.rs         Transport trait, Hidraw transport (open, identity check, flock), framing, Status, allowlists, Bragi<T> ops, with_software_mode, resource read/write
src/devices.rs       Model/LinkKind/Keyboard/Capabilities, sysfs discovery, stable ids
src/effects.rs       Effect model, encode (table above) / decode
src/storage.rs       XDG paths, atomic JSON with flock, write budget, value cache (sleep timeout, last effect read + timestamp)
src/config.rs        LiveConfig schema (live.json) shared by plugin (writer) and daemon (reader); Action, MacroStep, DialConfig, LightingConfig; validation
src/ipc.rs           daemon socket protocol types, client helpers, DaemonTransport (implements Transport with typed atomic RPC)
src/layouts/         key geometry + key-name tables (K65 PLUS ANSI/ISO, K100 RGB ANSI/ISO), evdev keycode names
src/plugin/          host protocol: request dispatch, state rendering, actions, keyboard visualization, --input-stream, service install/enable
src/daemon/          companion: device ownership, keepalive, dial emulation, per-key lighting frames, evdev-grab remaps + macros via uinput, IPC server
```

The implementation uses shared generic Transport, Backend and Platform boundaries for offline mocks.
Transport adds absolute-deadline and atomic-resource proxy hooks; the companion worker executes whole
typed operations, never lends resource handles to request threads.

Dependencies: `anyhow`, `libc`, `serde` (derive), `serde_json`, `evdev = "0.13"` (evdev + uinput).
No tokio, no hidapi, no GTK. Sync I/O with `poll(2)` deadlines and threads.

## 5. Device discovery and identity

Walk `/sys/class/hidraw/*`: canonicalize `device`, read interface `bInterfaceNumber` and the parent USB
device's `idVendor`, `idProduct`, `serial`, `busnum/devpath`. Match the model table. Command node = IF1
whose `report_descriptor` starts with usage page 0xFF42 usage 1 (`06 42 ff 09 01`); event node = IF2
(`06 42 ff 09 02`). Evdev nodes: `/sys/class/input/event*/device` under the same USB device (for the
receiver, the keyboard interface is IF0 with a `-event-kbd` by-id link). Stable id:
`k65plus-<serial>` / `k100-<serial>` (lowercase serial; fall back to `usb-<busnum>-<devpath>`).
Through a receiver, check GET 0x36 on target 0x08: bit 1 clear → product shows "Keyboard asleep or off"
with only cached/diagnostic info (no keyboard I/O).

## 6. Plugin (stateless, `--request`)

Request handling mirrors hyprdeck-razer: validate api_version/method/action/args before any I/O,
10 s cooperative work plus 3 s cleanup reserve with worker join, envelope `{"api_version":1,"error":…,"state":…}`, compact JSON.

Transport selection per keyboard: if the daemon socket answers `hello` and reports the device as
`owned`, use `DaemonTransport`; otherwise `Hidraw` with `flock(LOCK_EX|LOCK_NB)` retried for 750 ms
(busy → error group "Keyboard busy — refresh").

**State cost budget:** a `state` request must only GET properties (≈ 12 packets, < 150 ms). The effect
readback needs a software-mode dip: attempt it at most every 60 s per device (persist successful
data and a separate last-attempt timestamp on failure). After lighting actions cache the value
written. Failure to record cadence skips automatic mode-changing readback; use the explicit
"Re-read from keyboard" action. Show "as of HH:MM:SS" in the effect row subtitle.

Product (`kind: "keyboard"`), id = device id, `battery` from 0x0F/0x10 when wireless, `status` e.g.
"Connected · Hardware mode" / "Connected · Live mode (companion)" / "Asleep — press a key", `connection`
"SLIPSTREAM 2.4 GHz receiver" / "USB".

Sections (in order) and controls:
1. **overview** — keyboard visualization (when the host advertises `"keyboard_visualization"` in
   `request.host_features`), else omitted; info rows: battery & charge, mode/owner, layout, firmware
   (keyboard + receiver), polling rate.
2. **lighting** ("Lighting") — Effect `choice` (10 onboard effects, applies immediately); Speed `choice`
   (Slow/Medium/Fast; only for effects with speed); Colour `color` (Static only); Brightness `number`
   0–100 % step 5 (Apply); quick buttons "Lights off" (brightness 0) and "Full"; "Re-read from keyboard"
   button; **Saved looks**: `form` (name text) "Save current look", `choice` "Apply look" of saved
   looks, "Delete look" choice-form. Looks stored in `$XDG_CONFIG_HOME/hyprdeck-corsair/looks.json`
   (versioned, per device). K100 RGB: brightness only in hardware mode (onboard effects are iCUE's);
   per-key colours in live mode (see 3).
3. **live-lighting** ("Per-key lighting", wired boards only) — switch "Use per-key lighting (live mode)";
   base colour; zone colours (K100: keys, LightEdge, logo, wheel ring); per-key colours via the
   visualization `paint` action; "Clear per-key colours". Requires the companion; when it is not
   enabled, show an explanatory row with an "Enable live mode" button.
4. **keys** ("Keys & Macros") — Win-lock `switch` (0x45) and four `switch`es for the mask bits (0x4A);
   Remaps (companion): `form` {key choice (layout keys), action choice (key names, media keys,
   "Disabled", saved macros), optional combo text "ctrl+shift+t"} → "Add remap"; list rows of current
   remaps each with a "Remove" button; Macros: `form` {name, steps text in the mini-language below} →
   "Save macro", rows with "Delete"; dial (K65): `choice` "Dial does" = Volume (onboard, no live mode),
   Scroll, Zoom, Brightness, Media (next/prev, press play/pause), Workspace (left/right via
   `hyprctl dispatch workspace e±1`), Custom (cw/ccw/press actions via form). Non-default dial modes
   require live mode. All live features show whether the companion is running.
5. **power** ("Power & performance") — Sleep after `choice` (1, 5, 10, 15, 30, 60 min; shows
   "last set from hyprdeck" since GET is unsupported); Polling rate `choice` limited by 0x96
   (marked "experimental" until hardware-verified).
6. **live-mode** ("Live mode companion") — status (not installed / installed-stopped / running, owned
   devices, current mode per device); buttons "Enable live mode" (writes
   `~/.config/systemd/user/hyprdeck-corsair.service`, `systemctl --user daemon-reload`,
   `enable --now`), "Disable live mode" (`disable --now`, keeps config), "Restart"; note about the
   opt-in udev input rule (needed for remaps/macros/key tester) with the exact install commands.
7. **diagnostics** (collapsed) — hidraw/evdev nodes, PIDs, raw GET dump, write budget used today,
   "Rescue keyboard (restore hardware mode)" button (`destructive: false`), "Copy protocol log" is not
   needed.

Macro mini-language (also accepted by remap combo field): steps separated by `,`; each step is
`ctrl+shift+t` (tap combo), `down:shift`, `up:shift`, `text:hello world` (US layout only),
`delay:50` (ms, ≤ 5000), `media:play_pause`. Validation errors name the failing step. Max 64 steps.

Actions (names stable, args always include `device`): `set-effect {effect}`, `set-speed {speed}`,
`set-color {value}`, `set-brightness {value 0..100}`, `brightness-preset {value}`, `reread-effect`,
`save-look {name}`, `apply-look {value}`, `delete-look {value}`, `set-win-lock {value bool}`,
`set-lock-mask {bit, value bool}`, `set-sleep {value minutes}`, `set-polling {value hz}`,
`add-remap {key, action, combo?}`, `remove-remap {key}`, `save-macro {name, steps}`,
`delete-macro {name}`, `set-dial {mode, …}`, `set-live-lighting {value bool}`, `set-base-color`,
`set-zone-color {zone, value}`, `set-key-colors {value:{keys,color}}` (visualization paint),
`remap-key {value:{key,target}}` (visualization remap), `clear-key-colors`, `enable-live-mode`,
`disable-live-mode`, `restart-live-mode`, `rescue`. Each action returns the full new state.
Config-changing live actions write `live.json` atomically and send `reload` to the daemon if running.

**Keyboard visualization** (requires host feature): `kind: "keyboard"`, layout from `layouts`, key
colours = Static colour when the onboard effect is Static, live per-key colours when live lighting is
on, else `null` with status "Onboard effect: Rain (slow)". `remap` present when live remaps are
possible (input rule installed) with shared options; `paint` present only for live-lighting-capable
links. `stream.args = ["--input-stream", <device id>]`.

`--input-stream <id>` (key tester): NDJSON `{"status":"…","pressed":[key ids]}`. Source order:
daemon `keys` subscription if the daemon owns/grabs the device; else read-only, non-exclusive evdev of
the keyboard's event node; if neither is accessible, emit one frame with `pressed: null` and status
"Install the input rule to see live key presses" and keep running idle until killed. Never log keys.

## 7. Companion daemon (`hyprdeck-corsair daemon`, systemd --user)

Single process, sync, threads per device. Loads `live.json`; watches for `reload` over IPC and for
device hot-plug (poll `/sys/class/hidraw` every 2 s). Per device, compute required features:
- **dial** (K65, mode ≠ Volume) → needs software mode.
- **lighting** (wired board with live lighting enabled) → needs software mode.
- **remaps/macros** (any remap or macro bound) → needs evdev grab only (no software mode).

If a device needs software mode: open hidraw IF1 with exclusive flock (held while owned), SET 03=2,
keepalive (5 s receiver / 15 s wired), open IF2 events, emulate the dial via uinput (Volume emulation
too, so all modes work), write per-key frames on change (and on reconnect) using resource 0x22 (open
h1 on 0x22 once, write frames, keep handle; close on release). Serve plugin `xfer` requests through the
same transport (allowlisted). On child offline (status 6 / receiver 0x36 bit clear) pause and retry
every 2 s; on wake re-apply mode, frames. When no longer needed: close handles, SET 03=1, release lock.
On SIGTERM/SIGINT: release everything (mode 1, ungrab, destroy uinput devices) within 2 s.

Remaps/macros: open the keyboard evdev node(s), `EVIOCGRAB`, create a uinput keyboard
(`hyprdeck-corsair <model>`) with all keys the source supports + consumer keys; forward every event,
transforming remapped keys; macros run on a worker thread (never block forwarding); forward LED state
(caps/num lock) from the virtual device back to the physical device. If the grab or uinput fails, do
not grab (keys must never go dead). Fn layer is firmware-handled and not remappable in v0.1.

IPC (`$XDG_RUNTIME_DIR/hyprdeck-corsair/daemon.sock`, mode 0600, NDJSON, one request per line):
`{"op":"hello"}` → `{"ok":true,"version":"…","devices":[{"id","owned":bool,"mode":1|2,"grabbed":bool,"features":["dial","lighting","remap"]}]}`;
`{"op":"xfer","device":id,"payload":"<hex>"}` → `{"ok":true,"reply":"<hex>"}` (property-only compatibility, only when owned);
`{"op":"rpc","device":id,"timeout_ms":2000,"operation":{"kind":"read-resource","resource":28000}}`
→ whole resource data as hex. Other operations: `xfer` with byte-array `payload`, and
`write-resource` with `resource`, `declared_len`, `bytes`. No raw OPEN/READ/PROBE/WRITE/CLOSE
is accepted through IPC. Queued work has an absolute expiry before I/O, then bounded cleanup;
owner-side compare-skip, polling-cap and daily-write budget are authoritative (proxy callers do not
count again). Only K65 effect resource 0x6D60 is available through atomic resource RPC;
0x0002 and direct-lighting handle commandeering remain forbidden.
`hello` also reports optional/defaulted `generation`, `config_revision` (FNV64 deterministic
content correlation, not authentication), `frame_applied`, `remaps_active`, and diagnostics.
Mode 0 means uncertain: failed restoration retains the recovery owner, retries even after live
configuration is disabled, and never claims mode 1 until verified. A terminal descriptor is
reopened and verified, while keepalive also checks for mode drift. UI application acknowledgement
requires the same saved revision plus successful ownership/frame/grab/feature status; legacy or
missing acknowledgements show configured preview. Reload load/apply is serialized. Connection
admission uses RAII and a total monotonic request-line deadline, including slow-byte clients;
`{"op":"reload"}` → `{"ok":true}`; `{"op":"keys","device":id}` → stream of `{"pressed":[ids]}` lines
until the client disconnects. Errors: `{"ok":false,"error":"…"}`.

Input output is centrally reference-counted per physical node, macro job and dial. Failed releases
remain uncertain until cleanup/recreation. A physical press keeps its disposition across reload;
source-set expansion waits for idle before reacquisition. SYN_DROPPED suppresses events to the
next report and reconciles EVIOCGKEY without replaying macros; failed reconciliation ungrabs.
Grab waits observe the mailbox shutdown flag set before Shutdown is queued. A held-key deadline
refuses acquisition; buffered delivered events are drained and detected boundary races refuse
acquisition. Lossless handling of a press exactly on the EVIOCGRAB/ungrab boundary is not yet
proved; the checkpoint limitations in DEVELOPMENT remain blockers.
IF2 must be open before K65 software ownership; reader loss restores and retries. ISO layout is
read in hardware mode even for remap-only operation. Macro queues are bounded/nonblocking and
cancelled by configuration/source generation. Hubs retain latest state without subscribers,
disconnect overflowed subscribers, and invalidate/reseed source handoffs. Testers poll source
selection on a bounded cadence and clear missing-source highlights while reconnecting.

Plugin requests share 10 s work + 3 s cleanup under the host's 15 s allowance. Cooperative
cancellation is followed by JOIN; normal timeout never detaches a software-mode guard. Real HID,
resource pacing, IPC line reads, and storage locks observe absolute deadlines. Filesystem/kernel
stalls and external SIGKILL remain outside this guarantee. Cleanup failure is diagnostic, not
success. Companion shutdown still has a bounded 1900 ms join and ExecStopPost rescue fallback;
offline hardware may reject restoration (see DEVELOPMENT).

Performance: idle CPU ≈ 0 (blocking poll, no busy loops); keepalive timer only while owned; frames only
on change; uinput forwarding with no allocation per event.

## 8. Permissions (udev)

`data/70-hyprdeck-corsair.rules` (needed for everything): `TAG+="uaccess"` on hidraw nodes of
1b1c:{2b07,2b10,2b11,2b23,1b7c,1b7d,1bc5} interfaces 01 and 02.
`data/71-hyprdeck-corsair-input.rules` (opt-in, for remaps/macros/dial emulation/key tester): uaccess on
`/dev/uinput` (`KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"`)
and on `event*` nodes of those Corsair devices and of the companion's virtual devices (match
`ATTRS{name}=="hyprdeck-corsair*"`). README explains the privacy trade-off (any process of the seat user
can read those keyboards and inject input). Rules are release assets, never auto-installed.

## 9. Host change: `keyboard` visualization (in `~/source/hyprdeck`)

Implement research design `§3` (see `docs/research/hyprdeck-host.md` copy in this repo) with these
decisions: new `Visualization::Keyboard`, `KeyboardFrame`, generic input stream, `KeyboardView` on a
`gtk::DrawingArea` (selection, paint row, single-key remap row, live pressed keys), `VisualView` enum in
page/products, kind-change rebuild, docs in PLUGINS.md, tests. The host adds
`"host_features":["keyboard_visualization"]` to every request (older plugins ignore unknown fields).
Work on branch `feat/keyboard-visualization` from `main` in a separate worktree; never touch the user's
current checkout.

## 10. Testing

Unit tests (no hardware): framing, allowlist rejection, software-mode guard restores on error (mock
transport), status mapping, effect encode/decode against the [HW] byte strings, discovery against a
fake sysfs tree in a temp dir, config/macro parsing, layout invariants (unique ids, geometry in bounds,
≤ 512 keys), state JSON passes the host validation rules (ids, choice option limits, 1 MiB), dispatch
rejects bad requests before I/O, IPC round-trip with a mock daemon. CI: fmt, clippy -D warnings,
test, release build, jq smoke checks (Razer-style). Hardware tests are run by the lead with the user.

## 11. Out of scope / follow-ups

- Onboard (flash) remaps/macros/profiles: needs a USB capture of iCUE "Device Memory Mode" writes on
  Windows; then a separate research task.
- K100 AIR, firmware updates, iCUE profile import, software animated effects (reactive typing etc.).
