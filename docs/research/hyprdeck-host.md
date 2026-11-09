# Hyprdeck plugin host + hyprdeck-razer reference plugin — research for `hyprdeck-corsair`

Scope: the Hyprdeck plugin host (`~/source/hyprdeck/crates/plugins`, `docs/PLUGINS.md`) and the reference plugin (`~/source/hyprdeck-razer`). The Corsair keyboard HID protocol itself is out of scope here.

Evidence labels:
- **VERIFIED-IN-SOURCE**: read in the cited file and line.
- **CORROBORATED**: two independent sources agree, for example code plus docs, or host plus plugin.
- **INFERRED**: my design reasoning or recommendation.

Path shorthands:
- `H/` = `~/source/hyprdeck/`
- `P/` = `H/crates/plugins/src/`
- `R/` = `~/source/hyprdeck-razer/`

No device nodes were opened, and nothing outside the scratchpad was modified.

---

## 0. Executive summary

1. **The plugin contract.** A plugin is a standalone executable. It is run as `<exe> --request` with cwd set to the plugin directory. It gets one JSON request on stdin and must write exactly one JSON envelope on stdout. The deadline is 15 s; stdout is capped at 1 MiB and stderr at 64 KiB. The whole process group is SIGKILLed afterwards, so persistent daemons are forbidden.
   - Live input is the one long-lived exception. The host spawns `<own exe> <declared args>` while the view is mapped and reads NDJSON from it.
   - Sources: VERIFIED `P/backend.rs:215-258`, `P/process.rs:41-104` and `P/backend.rs:311-385`; CORROBORATED by `H/docs/PLUGINS.md:92,142,195-219`.
2. **Only one visualization kind exists: `controller`.** It has a closed set of 23 input ids and fixed host-owned artwork. `Visualization` is a serde internally-tagged enum (`P/protocol.rs:265-276`), so a state containing an unknown kind such as `"keyboard"` fails deserialization. On an older host that makes the enable handshake fail, so a new kind is a host feature that must ship first (VERIFIED `P/backend.rs:242-244`; `H/docs/PLUGINS.md:520-526`).
3. **Adding a `keyboard` kind cleanly** touches these places:
   - `protocol.rs`: a new enum variant, validation, a frame type and tests.
   - `backend.rs`: make the input stream generic over the frame type.
   - A new `keyboard.rs` view built on a `GtkDrawingArea`, following the `controller.rs` pattern.
   - `page.rs` and `products.rs`: a view enum in place of `ControllerView`, plus kind-change detection.
   - A new "run action" callback, so the multi-select paint control can submit the live selection.
   - `illustration.rs` and the products overview rule.
   - Docs.
4. **Hard limit to design around: 1 MiB per response.** Per-key `control` objects that each carry a large remap choice list blow this limit. For example, 150 keys × a 1,024-option form choice is about 6 MB. Use one shared option list at visualization level (INFERRED; arithmetic in §2.6).
5. **Refresh interval quirk.** The page resets the interval to 2,000 ms before each refresh and then takes `min(2000, state.refresh_interval_ms)`. A plugin can make refresh faster but never slower than 2 s (VERIFIED `P/page.rs:709,838-842`). Every state request re-executes the plugin, which re-opens hidraw. The Corsair `state` path must therefore be cheap and safe to run every ≤2 s.
6. **Razer conventions to mirror:**
   - Language and license: Rust edition 2024, MIT.
   - No GTK and no hidapi. Raw hidraw is accessed through `std::fs::OpenOptions` plus `libc::ioctl` and `libc::flock`.
   - Responses use `serde_json::json!` for controls. Device work is sync inside `spawn_blocking`, with a 13 s internal deadline.
   - A scoped `TAG+="uaccess"` udev rule is shipped as a release asset but never auto-installed.
   - CI runs fmt, clippy, test, a release build, and a headless `--request` jq check. Tag-triggered releases publish `plugin.json`, `<exe>-linux-x86_64` and `.sha256`.

---

## 1. Host build environment

- **Workspace (VERIFIED `H/Cargo.toml`):**
  - `edition = "2024"`, `license = "MIT"`, `resolver = "3"`.
  - `gtk = { package = "gtk4", version = "0.11", features = ["v4_20"] }`
  - `adw = { package = "libadwaita", version = "0.9", features = ["v1_8"] }`
  - `tokio` with `rt-multi-thread, macros, process, time, sync, fs, io-util`; `serde`, `serde_json`, `anyhow`.
- **Locked versions (VERIFIED `H/Cargo.lock`):** gtk4 0.11.5, libadwaita 0.9.2, glib 0.22.10, cairo-rs 0.22.9, tokio 1.54.0, serde_json 1.0.154.
- **System libraries (VERIFIED via `pkg-config` on this machine):** GTK 4.24.1, libadwaita 1.10.0. The crate features require at least GTK 4.20 and libadwaita 1.8.
- **`hd-plugins` crate (VERIFIED `H/crates/plugins/Cargo.toml`):** depends on `hyprdeck-core`, adw, gtk, anyhow, dirs, serde, serde_json, tokio, tracing, `libc = "0.2"` and `semver = "1"`.
- **Host CI (VERIFIED `H/.github/workflows/ci.yml`):** runs in an `archlinux:latest` container, because GTK 4.20+ and libadwaita 1.8+ are needed. Steps: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, plus an AppImage smoke build.

---

## 2. Plugin protocol: every validated field and limit

All of this is VERIFIED in `P/protocol.rs` unless noted, and CORROBORATED by `H/docs/PLUGINS.md`.

### 2.1 Manifest (`plugin.json`): `protocol.rs:12-22`, validated at `:51-86`

| field | rule | lines |
|---|---|---|
| `api_version` | must be exactly `1` (`API_VERSION`, `:9`) | :53-57 |
| `id` | `valid_id`: 1–64 bytes, first byte `[a-z]`, the rest `[a-z0-9_-]` | :24-31, :58-62 |
| `name` | nonempty after trim; ≤16,384 bytes; no NUL (`text()`) | :63, :510-517 |
| `description` | may be empty; ≤16,384 bytes; no NUL | :64 |
| `version` | semver with no prerelease or build suffix | :65-69 |
| `executable` | `filename()`: 1–200 bytes, first byte ASCII alphanumeric, the rest `[A-Za-z0-9._-]`; must not be `plugin.json` | :33-40, :70-73 |
| `update_repo` | exactly `owner/repo`, each part a safe filename | :42-49, :74 |
| `asset` | must equal `<executable>-linux-<std::env::consts::ARCH>` | :75-84 |

- Unknown manifest fields are accepted (`H/docs/PLUGINS.md:65`).
- An update requires the same `id`, `update_repo`, `executable` and `asset`, and a strictly newer version (`:88-111`).
- The executable must be a regular file (not a symlink), have an exec bit, and be ≤128 MiB (VERIFIED `P/backend.rs:28,133-147`).
- The manifest must be ≤1 MiB (`MAX_DOCUMENT`, `protocol.rs:10`; `backend.rs:121-131`).

### 2.2 Request/response envelope

- **Request** (`:114-120`): `{"api_version":1,"method":"state"|"action","action":<str|null>,"args":{...}}`. The host sets `method = "action"` exactly when an action name is present (`backend.rs:226-231`). `args` is always a JSON object.
- **Action name** (`:519-523`): nonempty, ≤128 bytes, no NUL.
- **Response** (`:122-129`): `{api_version, error?, state?}`.
  - `api_version` must be 1 (`backend.rs:245-249`).
  - A non-null `error` fails the request with `plugin <id>: <error>` (`:250-252`).
  - A null `error` requires a `state`, which is then validated (`:253-256`).
- **Transport limits:**
  - 15 s deadline (`backend.rs:236`).
  - Request ≤1 MiB (`:233`).
  - stdout ≤1 MiB; stderr ≤64 KiB (`process.rs:76`).
  - stderr is logged at debug level only (`backend.rs:239-241`).
  - A nonzero exit is a failure (`process.rs:80-83`).
  - The process group is SIGKILLed on completion, timeout or drop (`process.rs:32-39,58,87-103`).
- **Serialization:** all requests and actions are serialized across GUI and CLI by an in-process mutex plus `flock` on `$XDG_STATE_HOME/hyprdeck/plugins.lock` (`backend.rs:26,63-89,260-268`). The lock is global across all plugins.
- **Activation:** a disabled plugin is refused (`backend.rs:262-265`).

### 2.3 State (`:131-141`, validate `:525-611`)

| field | rule |
|---|---|
| `title` | required, nonempty (`:527`) |
| `description` | default `""`; text cap |
| `groups` | ≤128 (`:533`); group ids nonempty and unique (`:537-538`); group title nonempty; description text cap |
| total rows | ≤1,024 across all groups (`:544-545`); row ids nonempty and unique **within** a group (`:546-549`); row title nonempty; subtitle text cap; `control` validated (`:552-554`) |
| `refresh_interval_ms` | default 2,000 (`:261-263`); must be 250–10,000 (`:529-532`) |
| `products` | default `[]`; ≤64 (`:557`); ids pass `valid_id` and are unique (`:559-563`) |
| `product.kind` | one of `mouse controller dock keyboard keypad headset mousemat accessory peripheral` (`:564-578`) |
| `product.name` | nonempty; `description`, `status` and `connection` are **required keys** (no `serde(default)`, `:143-154`) but may be empty strings |
| `product.battery` | optional or null; `percentage` is null or finite 0–100; `charging` is null or bool; `source` string required (`:156-161`, `:583-591`) |
| `product.sections` | ≤12 (`:592`); ids pass `valid_id` and are unique; title nonempty; every group reference must exist in `state.groups` and is unique within the section (`:594-607`) |
| `group.collapsed` | default false (`:257-258`) |
| `group.visualization` | optional; validated per kind (`:541-543`) |

All strings: ≤16,384 bytes and no NUL (`text()`, `:510-517`). They are always rendered as plain text, never markup. Group titles go through `markup_escape_text` (`page.rs:188-193`), and rows use `use_markup(false)` (`page.rs:194-199`).

### 2.4 Native controls (`Control` enum `:437-504`, validate `:613-745`, rendered `page.rs:1014-1456`)

For every control, `args` defaults to `{}`. On Apply, the host inserts `args.value` with the user's value, overwriting any plugin-supplied `value`.

| kind | fields and validation | widget and behavior (page.rs) |
|---|---|---|
| `button` | `label` (nonempty), `action`, `args`, `destructive` (default false) (`:440-447`, `:616-619`) | `gtk::Button` sends args verbatim. `destructive` adds the `destructive-action` style and a confirm dialog (`:1017-1043`, `:800-806`) |
| `switch` | `value: bool` | `gtk::Switch` plus **Apply**; sends `value: bool` (`:1044-1081`) |
| `number` | `value`, `min`, `max`, `step` all finite; `min ≤ value ≤ max`; `step > 0` (`:629-639`) | `gtk::SpinButton` (digits 0 if step is an integer, else 6) plus Apply. Sends an integer JSON number when there is no fractional part (`:1082-1135`, `number_value` `:136-142`) |
| `choice` | 1–**256** options; option `value` text; `label` nonempty; values unique; current value present (`:642-662`) | `gtk::DropDown` plus Apply; sends the option `value` string (`:1136-1181`) |
| `text` | `value` text cap | `gtk::Entry` (max_length 16,384) plus Apply; sends a string (`:1182-1220`) |
| `color` | `value` must be `#RRGGBB` (`color_value` `:186-190`: length 7, `#` then 6 hex digits, any case) | `gtk::ColorDialogButton` (no alpha) plus Apply; sends uppercase `#RRGGBB` (`:1221-1255`, `color_hex` `:143-150`) |
| `form` | 1–32 fields; unique field ids passing `valid_id`; `label` (Apply text) defaults to "Apply" (`:496-508`, `:672-689`). Field kinds: `number` (finite min/max/step, step > 0, value in range), `switch` (bool), `text`, `color`, `choice` (1–**1,024** options, unique, value present) (`FormField::validate` `:192-246`) | Boxed list of `adw::SpinRow`, `EntryRow`, `ColorDialogButton` in an ActionRow, `ComboRow` (searchable when >16 options), and `SwitchRow`, plus a suggested-action Apply. Sends `value: {field_id: typed}` (`:1300-1454`) |
| `dpi` | min/max finite with 1 ≤ min ≤ max ≤ 65,535; step > 0; x/y null or an integer in range; `storage` is `host` or `device`; 1–8 stages with unique `valid_id` ids, nonempty labels and integer values in range; `active` is null or an existing id (`:690-741`) | `crate::dpi::build`. Sends `{"operation":"apply",...}` or `{"operation":"save",...}` (`:1256-1299`) |

Edit preservation:
- Any widget change marks the row with the CSS class `hd-pending-edit` and sets `view.dirty` (`mark_edit` `:161-164`).
- On refresh, rows with a pending edit or focus keep their widgets when the control is "compatible": same kind, same action and args, same numeric bounds, same choice options and same form schema (`products.rs:140-198`, `:820-847`; legacy path `page.rs:868-949`).

Action feedback:
- `run_action` (`page.rs:775-836`) shows Pending, Running, then Completed or Failed on the row (`ActionFeedback` `:71-111`).
- It disables the whole state box and products during the action, then renders the returned state.
- Actions are never retried.

### 2.5 The `controller` visualization: model, validation, rendering, streaming, selection

**Model** (VERIFIED `protocol.rs:265-318`):
```
Visualization::Controller { name, connection, status, inputs: Vec<ControllerInput>, stream: Option<ControllerStream> }
ControllerInput { id, label, detail, pressed: Option<bool>, value: Option<f64>, x: Option<f64>, y: Option<f64>, control: Option<Control> }
ControllerStream { args: Vec<String> }
```

**Validation** (`:335-356`, `:358-411`):
- `name` is nonempty; `connection` and `status` pass the text cap.
- At most 32 inputs. Ids come from a closed list of 23: `a b x y lb rb lt rt left_stick right_stick dpad_up dpad_down dpad_left dpad_right view menu guide m1..m6`. Uniqueness is enforced with a u32 bitmask.
- `label` is nonempty and `detail` passes the text cap.
- `value` must be a finite number in 0–1, and `x`/`y` finite in −1 to 1.
- `control` uses `Control::validate`.

**Stream args** (`:301-317`):
- 1–16 args, each nonempty, ≤256 bytes, with no control characters; ≤2,048 bytes in total.

**Frames** (`:320-333`):
- `MAX_INPUT_FRAME` is 64 KiB per line.
- A frame is `{inputs, status}` with the same validation as the visualization inputs.

**Rendering** (VERIFIED `P/controller.rs`):
- One `const SLOTS: [Slot; 23]` table (`:89-252`) drives painting, pointer hit-testing and the accessible selector. Each slot has an id, label, glyph, `Shape::Circle|Rect` in an 800×450 design space (`:13-14`) and a `Kind`.
- The canvas is a `gtk::DrawingArea` with `content_height(360)` and an accessible label (`:329-337`).
- It is drawn with cairo, scaled by `fit()` (`:1028-1037`), and repaints only on change (`:692-698`).
- It has a front/rear `ToggleButton` pair (`:310-327`) and a telemetry line (`:739-786`).
- The inspector (`:365-384`) shows the title, readings, detail and an `adw::PreferencesGroup` holding at most one control `ActionRow`.

**Selection:**
- Click: `GestureClick` → `hit()` → `select()` (`:449-459`, `:858-886`).
- Hover: a motion controller sets the pointer cursor and a tooltip (`:460-484`).
- Keyboard: a searchable `gtk::DropDown` of all 23 labels with an `_Control:` mnemonic (`:352-362`). Changing it calls `select(index, reveal=true)`, which also flips the front/rear view (`:717-738`).
- Per-view last selection is kept separately for front and rear (`:701-716`).

**Per-input control:**
- `refresh_inspector` (`:787-857`) renders the selected input's `control` by building an `adw::ActionRow` and calling the host's `on_control` callback. That callback is `page.control()`, so the controls are identical to row controls (`page.rs:972-980`; `products.rs:676-683`).
- The row is rebuilt only when the control actually changes.
- An in-progress edit is preserved when the new control is `compatible_control` and the row has `hd-pending-edit` or holds focus (`:818-839`).

**Live refresh:**
- `update()` (`:517-544`) updates text and merges inputs by id (`update_inputs` `:620-699`). Samples are copied only when there is no stream (`samples = !live`). Metadata and controls always update.
- If the `stream` args change, the stream restarts and samples clear (`:529-534`).
- Page-level paths call `controller.update()` even when the state is unchanged (`page.rs:882-890`; `products.rs:742-747`).

**Streaming lifecycle:**
- The view's `map` starts the stream; `unmap` and `destroy` stop it (`:485-506`).
- `start_stream` (`:577-618`) calls `backend::input_stream`, which returns a lease plus a bounded `mpsc(8)` receiver (`backend.rs:283-300`). Frames are consumed with `glib::spawn_future_local`; generation counters drop stale tasks.
- Any error marks the stream failed, clears readings and shows the error. It retries only after a re-map or a state update (`:535-537`).
- `disconnect()` pauses the stream on backend failure (`:561-567`).

**Host side of the stream** (`backend.rs:311-385`):
- The plugin must be enabled. The lock is held only while spawning (`:318-336`).
- The command is the installed executable with the literal args, cwd set to the plugin dir, stdin null, its own process group, and kill-on-drop.
- stdout is read line by line with a 64 KiB bound (`:403-430`); each line is parsed and validated.
- At most 16 KiB of stderr is retained and attached to the error (`:432-448`).
- On cancel or EOF the whole group is SIGKILLed and reaped (`:360-365`).

### 2.6 Products and artwork

- **Gallery and device pages:** `products.rs` shows a gallery `FlowBox` of up to two cards per line (`:216-224`).
  - Each card has a `illustration::product(kind, false)` image (250×150), name, description, a connection chip and a battery chip (`:328-367`).
  - A device page has a back button, chips, a title, and a `StackSwitcher` with one tab per section (`:369-412`).
  - Escape or Alt+Left returns to the gallery (`:441-467`).
  - A removed product loses its controls, and its open page shows "no longer available" (`:242-264`).
- **Overview artwork:**
  - The overview section prepends a large 480×260 illustration for every kind except `controller` (`:557-561`).
  - Controller overview puts visualization groups first (`:605-616`).
  - When a controller visualization's `name` equals the product name, its header is hidden (`:626-629`).
- **Keyboard artwork** (VERIFIED `illustration.rs:182-215`):
  - A generic 336×124 rounded chassis with an accent bar and a 4-row keycap grid in a 480×260 space.
  - Accessible label "Keyboard illustration" (`:16`).
  - A comment mentions "Chroma underglow", which is Razer wording; it is cosmetic only.
  - There is no per-model art. `kind: "keyboard"` already validates and renders today.
- **Deep links** (VERIFIED `H/src/app.rs:383-389,429-433` and `page.rs:377-390`): `hyprdeck --page plugin:<id>:<product>:<section>` routes to a product tab. This is useful for development.

### 2.7 Refresh interval: actual behavior

- The product page loop refreshes when mapped and not busy, then sleeps `refresh_interval_ms` (`page.rs:391-410`).
- `refresh()` first sets the interval to 2,000 (`:709`). `render()` then sets `min(current, state.refresh_interval_ms)` (`:838-842`).
- Net effect: the effective interval is `min(2000, plugin value)`. A plugin cannot slow refresh below every 2 s, even though validation accepts up to 10,000. This is VERIFIED; it partly contradicts `H/docs/PLUGINS.md:148`.
- Each refresh also runs `backend::list` first (`:711`).
- Refresh is skipped while an action is busy, and stale results are dropped by `action_generation` (`:712-714`, `:747-749`).

**Implication for Corsair (INFERRED):** `state` runs at least every 2 s while the page is visible, and each run is a fresh process that opens hidraw. Keep it to a small number of cheap reads. Cache static data (firmware, layout) keyed by USB identity, as Razer's `cache.rs` does. Expect `flock` contention with action requests and the stream process.

### 2.8 Test conventions (host)

- **Unit tests** live in the same file as the code under test (`#[cfg(test)] mod tests`):
  - Protocol validation tests: `protocol.rs:747-978`. They cover traversal and manifest checks, update identity, controller ambiguity, stream bounds, frame validation, product refs and battery, color and form fields, and DPI.
  - Backend tests (`backend.rs:648-871`): input line bounds, button edges and EOF, exchange rollback, symlink refusal, handshake rollback, API and error envelopes, and disabled updates never executing.
  - Process tests (`process.rs:106-135`): timeout and unbounded stdout.
  - Release tests (`releases.rs:341+`).
- **Subprocess fixtures** are `/bin/sh` scripts written by a `fixture()` helper (`backend.rs:741-765`) and serialized by `process::FIXTURE_LOCK` (`process.rs:11-12`), which avoids `ETXTBSY`.
- **No GTK unit tests exist.** `controller.rs`, `page.rs` and `products.rs` have no tests. Pure logic, such as geometry, hit tests and selection merges, should therefore live in GTK-free functions with their own tests (INFERRED).
- **Preview examples:** feature crates have `crates/<x>/examples/<x>_preview.rs`, about 7 lines each, calling `hyprdeck_core::ui::preview(app_id, <crate>::pages())` (VERIFIED `H/crates/input/examples/input_preview.rs:1-7`; `H/crates/core/src/ui.rs:142-178`).
  - `crates/plugins/examples/` exists but is empty. There is no plugins preview.
  - `ui::preview` takes `PageInfo` with a non-capturing `fn(&Ctx) -> Widget` (`ui.rs:29-38`). A plugin product-page preview therefore needs either a fixture plugin installed under isolated `XDG_*` dirs plus `hyprdeck --page plugin:<id>`, or a small `#[doc(hidden)] pub` fixture hook in `hd-plugins`.
  - `H/docs/PLUGINS.md:587` prescribes the isolated-XDG end-to-end method.
- **Required gates** (`H/docs/PLUGINS.md:580-585`, CI): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`, `cargo build --release --locked`.

---

## 3. Adding a validated `keyboard` visualization kind (INFERRED design, grounded in cited code)

### 3.1 Wire schema proposal

```json
"visualization": {
  "kind": "keyboard",
  "name": "K65 PLUS Wireless",
  "connection": "SLIPSTREAM 2.4 GHz receiver",
  "status": "Per-key colors from host preset (no device readback)",
  "layout": {"width": 15.0, "height": 5.25},
  "keys": [
    {"id":"esc","label":"Esc","detail":"","x":0.0,"y":0.0,"w":1.0,"h":1.0,"color":"#FF2200","pressed":null,"remap":"esc"},
    {"id":"enter","label":"Enter","x":12.75,"y":2.0,"w":2.25,"h":1.0,"color":null}
  ],
  "paint":  {"label":"Apply color to selected keys","action":"set-key-colors","args":{"device":"usb:1-8"}},
  "remap":  {"label":"Assign selected key","action":"remap-key","args":{"device":"usb:1-8"},
             "options":[{"value":"default","label":"Default"},{"value":"f13","label":"F13"}]},
  "stream": {"args":["--input-stream","usb:1-8"]}
}
```

**Validation rules** (mirroring the existing style in `protocol.rs:335-411`):

- **Text fields:** `name` nonempty; `connection` and `status` pass `text()`.
- **`layout`:** `width` and `height` finite, 1–64 layout units. Units are key units ("u"); fractional values allowed.
- **`keys`:**
  - 1–512 entries. K100 RGB has roughly 150 keys plus macro keys, and LED-only elements such as a light bar may be added later.
  - `id` passes `valid_id` and is unique (HashSet). `label` is nonempty and at most 32 bytes (a drawing cap). `detail` passes `text()`.
  - Geometry: `x, y ≥ 0`; `w, h > 0`; all finite; `x + w ≤ width`; `y + h ≤ height`.
  - `color` is null or passes `color_value`. Null means unknown or no readback and is drawn hatched, like the controller's "no value" trigger (`controller.rs:1187-1200`).
  - `pressed` is null or a bool. `control` is optional and uses `Control::validate`.
  - `remap` is null or must be one of `remap.options[].value`.
- **`paint`:** optional; `label` nonempty; `action` passes `action_name`. When submitted, the host inserts `args.value = {"keys":[selected ids in layout order], "color":"#RRGGBB"}`.
- **`remap`:** optional; `label` nonempty; `action_name`; 1–1,024 unique options (the same rule as form choices, `:223-241`). When submitted, the host inserts `args.value = {"key":"<id>","target":"<option value>"}`.
- **`stream`:** reuse the `ControllerStream` type, renamed or aliased to `InputStream`, with the same validation (`:301-317`).
- **`KeyboardFrame` (stream NDJSON):** `{"status":"Live keys","pressed":["a","left_shift"]}`.
  - `pressed` is null, meaning unavailable, or an array of at most 512 unique `valid_id` strings.
  - Semantics: every declared key not listed is released. This keeps frames small; 150 full key objects would be about 10 KB per frame.
  - Unknown ids are ignored by the view, as the controller does at `controller.rs:621-626`.

**Why shared `remap.options` rather than a per-key `control` holding a big choice:** each 1,024-option form choice is about 1,024 × ~40 B ≈ 40 KB of JSON. Repeated for 150 keys that is about 6 MB, far over the 1 MiB stdout cap (`protocol.rs:10`, `backend.rs:236`). Razer already models onboard reassignment the same way: one form with a `button` choice and a `target` choice, not per-input controls (VERIFIED `R/src/wolverine_mapping.rs:273-293`). Razer's `ControllerInput.control` is always `None` (`R/src/input.rs:271,309,359,379`). Keep the generic per-key `control` for small cases.

### 3.2 Files, functions and tests to change

| File | Change |
|---|---|
| `P/protocol.rs` | Add `Visualization::Keyboard { name, connection, status, layout: KeyboardLayout, keys: Vec<KeyboardKey>, #[serde(default)] paint: Option<SelectionAction>, #[serde(default)] remap: Option<RemapAction>, #[serde(default, skip_serializing_if)] stream: Option<ControllerStream> }`. Extend `Visualization::validate` (`:335-356`); this match is exhaustive, so the compiler forces it. Add `validate_keyboard_keys` and `pub(crate) struct KeyboardFrame { status, pressed: Option<Vec<String>> }` with `validate()`. Tests modelled on `:795-892`: duplicate ids, invalid id, NaN, negative or out-of-layout rects, `w = 0`, bad color, `remap` value absent from options, more than 512 keys, legacy JSON without `paint`/`remap`/`stream` round-trips, frame with duplicate or invalid ids, and frame larger than 64 KiB rejected by the line reader. |
| `P/backend.rs` | `input_stream`, `run_input_stream`, `read_input_frames` and their tests are typed to `ControllerFrame` (`:22,283-300,311-401`). Make them generic: `trait Frame: DeserializeOwned + Send + 'static { fn validate(&self) -> Result<()>; }`, with `input_stream<F: Frame>(id, stream) -> (InputStream, mpsc::Receiver<Result<F, String>>)`. Add a keyboard-frame parsing test alongside `:671-695`. Keep the lock-only-while-spawning behavior (`:318-336`). |
| `P/keyboard.rs` (new) | `KeyboardView { widget, inner }` with the same public surface as `ControllerView`: `new(plugin_id, &Visualization, on_control, on_action)`, `update(&Visualization)`, `show_header(bool)`, `disconnect()`. Its internals are listed in §3.3. |
| `P/page.rs` | `Rendered.controllers: Vec<Option<ControllerView>>` (`:63-69`) becomes `Vec<Option<VisualView>>`, where `enum VisualView { Controller(ControllerView), Keyboard(KeyboardView) }` delegates `widget`, `update`, `disconnect` and `show_header`. Update all call sites (`:732-735`, `:764-768`, `:883-889`, `:902-908`, `:971-984`). **Kind-change bug-proofing:** `same_shape` compares only `visualization.is_some()` (`:896`); also compare `std::mem::discriminant` so that switching from controller to keyboard rebuilds the view. Add an `on_action` closure that wraps `run_action` (`:775-836`) so the view can submit `paint`/`remap` with the live selection. |
| `P/products.rs` | `GroupView.controller: Option<ControllerView>` (`:42-50`) becomes `Option<VisualView>`. Change `new` (`:676-683`) and the add/remove/update logic (`:763-779`), comparing the discriminant before calling `update`. Generalize `ControlRenderer` (`:13`) into a small `Host { render_control, run_action }`. Generalize the overview-illustration rule at `:559` from `product.kind != "controller"` to "no group in this section has a visualization", and the `prioritize_diagram` rule at `:605` to any visualization. Make the duplicate-header check at `:626-629` match `Keyboard { name, .. }` as well. |
| `P/lib.rs` | `mod keyboard;` (`:1-10` list). |
| `P/illustration.rs` | Optional. Keep `keyboard()` as card art. Optionally add a `tkl`/`full` hint, but the product schema has no sub-kind, so a new field would be needed. Change the "Chroma" comment to neutral wording. |
| `H/docs/PLUGINS.md` | New "Keyboard visualization" section after `:146-225`: schema, limits, paint and remap value shapes, frame semantics, and a privacy note on key streams. Add a note that older hosts reject unknown kinds. |
| `H/crates/plugins/examples/` | Optional `keyboard_preview.rs` if a `#[doc(hidden)] pub fn preview_visualization(json: &str) -> gtk::Widget` hook is added; otherwise document the isolated-XDG fixture plugin method. |

### 3.3 Rendering approach: GtkDrawingArea (recommended) versus a grid of buttons

**Recommended: a `gtk::DrawingArea` with cairo, following `controller.rs`.** One geometry table (`Vec<KeyRect>` built from the validated `keys`) drives painting, hit-testing and keyboard navigation. This is the same pattern as `SLOTS` (`controller.rs:89`).

Strengths:
- Free-form rectangles handle ISO Enter, tall numpad keys, the K100 macro column and an LED bar.
- Per-key colors are just fill colors, with no CSS provider per key.
- Live key-press frames cost one `queue_draw` each.
- Scaling: compute `scale = min((W-16)/layout.width, (H-12)/layout.height)`, as in `fit()` (`:1028-1037`). Use `gtk::AspectFrame(ratio = width/height)` or a computed `content_height` so the canvas keeps the keyboard's aspect ratio.

Drawing per key:
1. A rounded rectangle filled with the key color, or a neutral fill with hatching when the color is null.
2. A 1.5 px border.
3. The label, rendered as plain cairo text, never Pango markup. Text color is chosen by luminance contrast against the fill. Ellipsize by measuring `text_extents`.
4. Pressed: an inner accent glow (the controller uses GREEN, `:15`).
5. Selected: an accent outline; focused: a dashed ring.

**Alternative: a grid of `gtk::ToggleButton`s in a custom layout.** Strengths: native per-key focus and accessibility. Weaknesses:
- 100–200 widgets.
- Arbitrary colors need dynamic CSS or a custom draw for every button.
- Non-grid shapes need a custom `LayoutManager`, because `gtk::Fixed` does not scale.
- Stream-rate updates would restyle many widgets.

Not recommended.

**Keyboard accessibility for the DrawingArea:**
- Make the area focusable (`set_focusable(true)`, `set_focus_on_click(true)`) with an `EventControllerKey`:
  - Arrow keys move focus to the nearest key center in that direction (a geometric search over the rect table).
  - Space toggles the focused key in the selection. Shift+Arrow extends the selection. Ctrl+A selects all. Escape clears.
  - Enter selects only the focused key and moves focus to the inspector.
- Mirror the controller's accessible fallback: a searchable `gtk::DropDown` "_Key:" of all labels with a mnemonic (`controller.rs:352-362`).
- Add **Select all**, **Clear selection** and **Add key to selection** buttons.
- Show a selectable "Selected: Esc, F1, F2 (3 keys)" label.
- Update `gtk::accessible::Property::Description` on the area whenever the selection changes.
- Tooltips on hover, as at `:460-484`.
- A fuller option is to implement GTK 4.10+ virtual accessible children (`gtk::subclass::accessible::AccessibleImpl`, available with the `v4_20` feature) so screen readers see every key. Treat that as a follow-up, not a requirement.

**Multi-select and paint:**
- Pointer: click selects one key; Ctrl+click toggles; Shift+click selects the rectangle between the anchor and the key; a `GestureDrag` rubber-band selects every key whose rect intersects the dragged rectangle.
- The paint row is an `adw::ActionRow` owned by the view. It contains a `ColorDialogButton` (`page.rs:151-160` pattern), an "N keys selected" subtitle and an Apply button.
- Apply calls `on_action(&row, paint.action, paint.args + {"value": {"keys":[...], "color":"#RRGGBB"}})`, which reaches `run_action` so native feedback appears on that row (`page.rs:784-835`).
- The picker's pending color is view state and is never reset by refresh.

**Per-key control (remap):**
- When exactly one key is selected, the inspector shows a `ComboRow` built from `remap.options`, searchable when there are more than 16 options (as at `page.rs:1393-1397`). It is preselected to that key's `remap` value, with Apply calling `on_action(..., {"value":{"key":id,"target":v}})`.
- If a key carries a generic `control`, render it through `on_control`, exactly as `controller.rs:840-856` does.
- With more than one key selected, hide the per-key control and show "Select a single key to remap".

**How live refresh preserves selection (INFERRED, following `controller.rs:620-699` and `:807-839`):**
- Store the selection as `RefCell<BTreeSet<String>>` of key **ids**, not indices, plus `anchor` and `focus` as `Option<String>`.
- `update(&Visualization)`:
  1. If the ids or rects changed, rebuild the geometry table, then intersect the selection, anchor, focus and hover with the new ids.
  2. Copy colors, labels, details and `pressed` by id, and only `queue_draw` when something changed.
  3. Re-render the inspector row only when the selected key's control or remap value changed in an incompatible way, skipping it when the row has `hd-pending-edit` or focus (`compatible_control`, `products.rs:140-198`).
  4. Never touch the paint picker's color.
- When the stream is live, ignore `pressed` from state and take it only from frames, as the controller does with `samples = !live` at `controller.rs:538-542`.
- After an action, the page renders the returned state through the same `update` path. The view is not rebuilt, because the group id and kind are unchanged, so the selection survives.
- Stream lifecycle is copied verbatim: start on `map`, stop on `unmap`/`destroy`, a generation counter, and `disconnect()` on backend failure (`controller.rs:485-506`, `:561-618`).

**Privacy note (INFERRED):** a keyboard key-press stream is functionally a keylogger feed. Limit it to the mapped view, which the host already enforces (`H/docs/PLUGINS.md:213-218`). The plugin should open evdev read-only and non-exclusively, as Razer does (`R/src/input.rs:142-149`), never write keys to stderr, and say this in the README.

### 3.4 Compatibility and rollout (VERIFIED basis; plan INFERRED)

- An older host that receives `"kind":"keyboard"` fails `serde_json::from_slice::<Response>` at `backend.rs:242-244`. On enable, that rolls back an update (`backend.rs:631-641`; `H/docs/PLUGINS.md:520-526`). API v1 has no feature negotiation.
- Plan: publish the host first, then the plugin release.
- Optional host addition: an extra request field such as `"host_features":["keyboard_visualization"]`. Plugins ignore unknown fields, and the Razer `Request` struct has no `deny_unknown_fields` (`R/src/main.rs:16-24`). With it, the Corsair plugin could emit rows-only state on older hosts.

---

## 4. hyprdeck-razer reference plugin (VERIFIED in `R/`)

### 4.1 Packaging

- **`Cargo.toml`:**
  - `name = "hyprdeck-razer"`, version 0.6.0, `edition = "2024"`, `license = "MIT"`, `publish = false`.
  - Dependencies: `anyhow 1`, `evdev 0.13`, `libc 0.2`, `reqwest 0.12` (rustls-tls only), `futures-util`, `roxmltree`, `serde` (derive), `serde_json`, `tokio` (rt-multi-thread, macros, time, process), `zbus 5` (tokio).
  - Release profile: `lto = "thin"`, `codegen-units = 1`, `strip = true`.
  - No GTK, hidapi, libusb or OpenSSL linkage (CORROBORATED `R/docs/DEVELOPMENT.md:74`).
- **`plugin.json`:** id `razer`, executable `hyprdeck-razer`, `update_repo: mikkeyboi/hyprdeck-razer`, `asset: hyprdeck-razer-linux-x86_64`.
- **`LICENSE`:** MIT, © 2026 mikkeyboi.
- **`.gitignore`:** `/target/`, `/dist/`.

### 4.2 `src/main.rs`: request dispatch

**Serialization types** (`:16-148`):
- The plugin keeps its own minimal mirror of the host types: `Request`, `State`, `Product`, `Battery`, `ProductSection`, `Group`, `Visualization::Controller` (with `stream: Option<Value>`), `ControllerInput` and `Row`.
- Controls are untyped `serde_json::Value` built with `json!`.
- Builders: `Row::info(id, title, subtitle)`, `.controlled(json)`, `Group::new(id, title, desc)` and `.collapsed()`. The helper `string_arg` is at `:149-153`.

**`state()`** (`:155-178`):
- Enumerates USB devices, then joins the OpenRazer and firmware snapshots.
- `products::assemble` builds groups and products, and firmware groups are added to Diagnostics.
- `refresh_interval_ms: 2000`.

**`dispatch()`** (`:180-325`):
- Rejects `api_version != 1`, non-object `args`, `state` with an action, and unknown methods before touching any hardware.
- Each action is a `match` arm. Blocking device I/O runs under `tokio::task::spawn_blocking` (`:203-235`, `:242-251`, `:269-272`).
- After an action it rebuilds the full state with `state()`. The action's optional result `Group` is attached:
  - `measure` results go into the owning product's `tools` section with id `cached-<id>`.
  - Other results go into `diagnostics`, collapsed (`:277-318`).

**CLI** (`main` `:369-459`):
- `--input-stream <usb-id>` takes exactly two args, runs the stream, and exits 1 with stderr on error (`:372-385`).
- `firmware-update <id> --confirm` is a standalone long operation (`:386-399`).
- `--help` and `--version` (`:400-412`).
- `--request` reads stdin capped at 1 MiB + 1 byte (`:413-425`). Human subcommands (`state`, `measure`, `firmware-check`, `usb-reset`, `rumble`, `action <name> [json]`) go through `cli_request` (`:327-367`).
- A 13 s `tokio::time::timeout` leaves headroom under the host's 15 s (`:429-439`).
- Envelope: `{"api_version":1,"error":null,"state":...}` or `{"api_version":1,"error":"<{:#}>","state":null}` (`:441-444`). Compact JSON in request mode, pretty in CLI mode (`:446-450`). Exit 0 with an error envelope in request mode; exit 1 for CLI errors (`:456-458`).

**Tests** (`:461-488`): invalid protocol or action is rejected before hardware.

### 4.3 Products, groups, sections and controls

- **`R/src/products.rs`:**
  - `section()` (`:8-14`) and `map_daemon_kind` (`:19-35`) set `product.kind`.
  - `product_sets` (`:53-107`) groups USB devices into physical products, for example the Basilisk mouse + receiver + dock.
  - `assemble` (`:204+`) builds sections in a fixed order (`:533-548`): `overview`, then `controls` ("Buttons"), `macros` ("Keys & Macros"), `sensitivity`, `lighting`, `power`, `buttons` ("Buttons & Macros"), `tools`, `firmware`, and finally `diagnostics` ("Diagnostics").
  - Product status is "USB connected", and `battery` comes from native or OpenRazer queries (`:549-558`).
- **Group ids are namespaced per device**, for example `wolverine-polling:{device.id}` (`R/src/wolverine.rs:259-263`). They stay stable across refreshes because they derive from the USB port id `usb:<port>` (`R/src/usb.rs:47-49`).
- **Error groups:** a failed read becomes an informational group, "Native HID read failed; no hardware configuration was changed" (`wolverine.rs:349-361`). One bad query never fails the whole state (`:362-405`).
- **Example controls:**
  - Form with choice fields for polling (`wolverine.rs:269-275`).
  - Form with button and target choices for mapping (`wolverine_mapping.rs:273-279`).
  - Form with button choice and shortcut text (`:286-292`).
  - Destructive USB-reset button (`usb.rs:230-241`).
  - Controller visualization with stream args `["--input-stream", self.id]` (`usb.rs:211-228`).

### 4.4 Caching and persistent host-side settings

- **`R/src/cache.rs`** (diagnostic result cache):
  - Location: `$XDG_STATE_HOME/hyprdeck-razer/` (fallback `~/.local/state`) (`:17-23`). File name `<kind>-<hex(id)>.json` (`:24-27`).
  - Stores `{identity, unix_seconds, group, summary}` (`:9-16`).
  - The identity guard is `port:pid:inode:ctime:ctime_nsec:serial` of the USB sysfs node (`:28-47`), so re-plugging invalidates the cache.
  - Writes are atomic: `create_new` with mode 0600, a `.tmp` file, then rename (`:48-68`).
  - `load` rejects non-regular files over 1 MiB and identity mismatches. It relabels results as "Last explicit result … not a live measurement" (`:69-93`).
- **Host preferences** (`R/src/dpi_presets.rs`):
  - Location: `$XDG_CONFIG_HOME/hyprdeck-razer/dpi.json` (fallback `~/.config`) (`:47-54`).
  - Versioned `{"version":1,"devices":{SERIAL:{...}}}`. Malformed files or wrong versions are reported and never overwritten (`:83-100`).
  - `save` (`:205-245`): `flock(LOCK_EX)` on `dpi.lock` (opened `O_NOFOLLOW`, mode 0600), then read, modify, write a temp file, `sync_all`, rename, and fsync the directory.
  - Tests use temp dirs (`:335+`).
  - CORROBORATED by `R/docs/DEVELOPMENT.md:127-133`.
- **Corsair applicability (INFERRED):** use the same pattern for per-key color presets and remap profiles, at `$XDG_CONFIG_HOME/hyprdeck-corsair/{lighting,remap}.json` keyed by a serial or a stable identity. This matters if the keyboard cannot read per-key colors back: the plugin must not invent readback, so the visualization `status` should say "host preset".

### 4.5 Wolverine HID access: raw hidraw with `libc`, no hidapi

All VERIFIED in `R/src/wolverine.rs`.

- **Constants** (`:31-39`): report id 10, 91-byte report, `HIDIOCSFEATURE = 0xc05b4806`, `HIDIOCGFEATURE = 0xc05b4807`, `HIDIOCGRAWINFO = 0x80084803`, a 750 ms command timeout and a 4 s session timeout.
- **`Session::open`** (`:64-128`):
  1. Walk `/sys/class/hidraw/*`, canonicalize `<entry>/device`, and keep entries under the USB device's sysfs path whose parent interface has `bInterfaceNumber == "01"`.
  2. Open `/dev/<hidrawN>` read+write with `O_CLOEXEC | O_NONBLOCK`. On failure, the error tells the user to install the udev rule and not to run as root (`:85-90`).
  3. Check identity with `HIDIOCGRAWINFO` (`#[repr(C)] RawInfo { bustype: u32, vendor: i16, product: i16 }` `:41-47`): bus 3 (USB) and the exact VID:PID (`:91-100`).
  4. Take an advisory `flock(LOCK_EX|LOCK_NB)` on the hidraw fd, retrying every 10 ms up to 750 ms, to exclude concurrent plugin processes; otherwise fail with "busy … refresh" (`:101-118`). It never grabs evdev or unbinds drivers.
- **Command** (`:130-176`): a transaction id 1..31, then `HIDIOCSFEATURE`, then polling `HIDIOCGFEATURE` every 10 ms until a matching completed reply with a valid checksum and status arrives. No resend. Pure framing helpers (`checksum`, `request`, `completed_reply`, `:179-219`) are unit-tested without hardware (`:432-475`).
- **Write discipline** (`set_polling` `:278-302`): check that the active profile still matches, write only on difference, verify readback, and re-check the profile afterwards. The docs restate this (`R/docs/controller-linux-support.md`, "Native V4 PC HID controls").
- **Corsair (INFERRED):** the K65 PLUS vendor interface uses 64-byte interrupt IN/OUT reports, not feature reports. Use the same open, identity and flock skeleton with plain `write(2)` of 65 bytes (report id 0 prefix) and `poll(2)` + `read(2)` with deadlines. Add `HIDIOCGRDESCSIZE`/`HIDIOCGRDESC` to assert the 0xFF42 usage page before writing. Select the interface by `bInterfaceNumber` (01) under VID `1b1c`. The exact framing belongs to the protocol research.

### 4.6 Input-stream subprocess

- **Producer** (`R/src/input.rs`):
  - `stream(id)` (`:502-516`) wraps `stream_to` with a 16 KiB `BufWriter`. On error it emits a final frame with every input unknown and status `Input disconnected / unavailable: …`, so no stale "pressed" reading remains, then returns the error and the process exits 1.
  - `stream_to` (`:518+`) finds the device and opens the evdev node read-only and non-exclusively (`open` `:142-149`). It reads the initial key and axis state via ioctls, emits a first frame, then loops on `poll(2)`.
  - Axis frames are coalesced to about 60 Hz (`:533`); button edges are immediate. `SYN_DROPPED` is handled by resynchronizing.
  - `emit_frame` (`:481-498`) writes one JSON line and flushes. A `BrokenPipe` means the host closed the stream and is a clean exit.
- **Host side:** see §2.5.
- **Wiring:** advertised from state via `stream: {"args":["--input-stream","usb:<port>"]}` (`usb.rs:224`); `main.rs:372-385` dispatches it.

### 4.7 Error handling style

- `anyhow::Result` with `.context(...)` and `.with_context(...)` that carry user-actionable remediation, for example udev instructions and "refresh before applying again".
- `bail!` and `ensure!` validate every argument before I/O (`main.rs:180-201`).
- Partial failures degrade to info groups (§4.3), and responses are never fabricated. In the code's own words, the plugin never fabricates a setting or substitutes a default rate (`wolverine.rs:250-252`).

### 4.8 Tests

- Counts per file (grep of `#[test]` / `#[tokio::test]`):
  - `input.rs` 9, `battery_notifications.rs` 9, `dpi_presets.rs` 4, `openrazer_controls.rs` 4, `wolverine.rs` 3, `openrazer.rs` 3
  - `products.rs`, `firmware.rs`, `main.rs`, `wolverine_mapping.rs`, `wolverine_keyboard.rs` 2 each
  - `usb.rs` 1
- All run without hardware: pure framing and validation, temp-dir file I/O, and dispatch rejection.
- CI also runs headless protocol checks on the release binary (§4.9).

### 4.9 CI and release workflows

- **`.github/workflows/ci.yml`:**
  - Triggers: push to main and pull requests. Runner `ubuntu-22.04`, `dtolnay/rust-toolchain@stable` (rustfmt, clippy), `Swatinem/rust-cache@v2`.
  - Steps: `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo build --locked --release`.
  - Then two jq checks: a valid state request must yield `api_version == 1 and error == null and (.state.groups | type == "array")`, and `api_version: 2` must yield an error string with a null state.
- **`.github/workflows/release.yml`:**
  - Triggered by a `v*` tag or by `workflow_dispatch` with an input tag. `permissions: contents: write`. Env `ASSET: hyprdeck-razer-linux-x86_64`.
  - Validates that the tag equals `v` + the Cargo version, that `jq` sees the correct manifest identity (`id`, `version`, `executable`, `update_repo`, `asset`), and that the tag ref exists.
  - Runs the same gates and builds.
  - Assembles `dist/`:
    - `install -m 0755 target/release/hyprdeck-razer dist/$ASSET`
    - `install -m 0644 plugin.json`
    - `install -m 0644 data/70-hyprdeck-wolverine.rules`
    - `sha256sum "$ASSET" > "$ASSET.sha256"`
    - a smoke `--request` check
  - Uploads the artifact, refuses to overwrite an existing release, and runs `gh release create "$RELEASE_TAG" dist/* --verify-tag --generate-notes`.
  - Release assets: `plugin.json`, `<exe>-linux-x86_64`, `<exe>-linux-x86_64.sha256` and the udev rule. This matches the host's expected asset set (`H/docs/PLUGINS.md:504-546`).

### 4.10 README and docs

- **`README.md`:** capabilities per product, screenshots, Install (`hyprdeck plugins install mikkeyboi/hyprdeck-razer`, `enable razer`, `--page plugin:razer`, `:41-53`), "Important limits", and Development links.
- **`docs/DEVELOPMENT.md`:** build gates and local install (`:72-93`), CLI and subprocess API (`:95-106`), storage formats, capability boundaries, firmware and releases (`:163-176`), and primary references.
- **`docs/controller-linux-support.md`:** hardware evidence, permissions and protocol provenance.
- The house style is an explicit capability and limits statement. It never claims unsupported writes and records the evidence for each protocol fact.

### 4.11 udev rule shipping

- **`R/data/70-hyprdeck-wolverine.rules`:** three `ACTION=="add|change"` lines grant `TAG+="uaccess"` (active-seat ACL, not a group or mode change):
  - hidraw scoped by `ATTRS{idVendor}`/`ATTRS{idProduct}` and `ENV{ID_USB_INTERFACE_NUM}=="01"`
  - the same for interface `"00"`
  - `SUBSYSTEM=="input", KERNEL=="event*"` for the device
- **Manual install** (`R/docs/controller-linux-support.md`, "Native V4 PC HID controls"):
  - `sudo install -m 0644 data/70-hyprdeck-wolverine.rules /etc/udev/rules.d/`
  - `sudo udevadm control --reload-rules`
  - then reconnect the device.
- The host never installs rules (`H/docs/PLUGINS.md:516-518`).
- Observed on this machine: `/etc/udev/rules.d` (or `/usr/lib/udev/rules.d`) contains `70-hyprdeck-wolverine.rules`, `60-steam-input.rules` and `60-steam-vr.rules`. No file named `*openlink*` or `*corsair*` was found there (VERIFIED by `ls | grep`). Whatever access OpenLinkHub currently has therefore comes from elsewhere, such as its own install path or its service user; that is outside this scope.

### 4.12 Local development install and replacement

- **Build and install** (CORROBORATED `R/docs/DEVELOPMENT.md:76-87` and `H/docs/PLUGINS.md:488-500`):
  ```sh
  cargo build --locked --release
  mkdir -p dist/local
  install -m 0755 target/release/hyprdeck-corsair dist/local/hyprdeck-corsair
  install -m 0644 plugin.json dist/local/plugin.json
  printf '%s' '{"api_version":1,"method":"state","action":null,"args":{}}' | dist/local/hyprdeck-corsair --request
  hyprdeck plugins install ./dist/local
  hyprdeck plugins enable corsair
  hyprdeck --page plugin:corsair
  ```
- **What gets copied:** local install copies **only** the manifest and executable (`backend.rs:528-543`), so resources must be embedded with `include_str!`/`include_bytes!` (`H/docs/PLUGINS.md:61`).
- **Replacing a local development build:** a reinstall is refused when the plugin is already installed (`backend.rs:545-551`). Install also removes the id from `enabled` (`:552-555`). The sequence is:
  ```sh
  hyprdeck plugins disable corsair
  rm -rf -- "${XDG_DATA_HOME:-$HOME/.local/share}/hyprdeck/plugins/corsair"
  hyprdeck plugins install ./dist/local && hyprdeck plugins enable corsair
  ```
  Remove only that plugin's own directory (`H/docs/PLUGINS.md:61`).
- **Current machine state:** `~/.local/share/hyprdeck/plugins/razer` is installed, and `~/.config/hyprdeck/plugins.toml` contains `enabled = ["razer"]` (VERIFIED, read-only).
- **Isolated end-to-end testing:** set `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `XDG_STATE_HOME` to temporary directories (`H/docs/PLUGINS.md:587`).

---

## 5. Recommended skeleton: `~/source/hyprdeck-corsair` (INFERRED, mirroring Razer)

```
hyprdeck-corsair/
  Cargo.toml          # name hyprdeck-corsair, edition 2024, license MIT, publish=false;
                      # deps: anyhow, libc, serde(derive), serde_json; optional evdev 0.13 (key stream);
                      # no tokio needed (all I/O sync + poll with deadlines) unless zbus/reqwest are added;
                      # [profile.release] lto="thin", codegen-units=1, strip=true
  plugin.json         # {"api_version":1,"id":"corsair","name":"Corsair keyboards",
                      #  "description":"K65 PLUS Wireless and K100 lighting, keys and status",
                      #  "version":"0.1.0","executable":"hyprdeck-corsair",
                      #  "update_repo":"mikkeyboi/hyprdeck-corsair","asset":"hyprdeck-corsair-linux-x86_64"}
  LICENSE             # MIT (clean-room: describe protocol facts, never copy GPL-3.0 OpenLinkHub code)
  README.md           # capabilities / important limits / install / dev link (Razer style)
  docs/DEVELOPMENT.md # gates, local install+replace, CLI, storage formats, protocol provenance table
  docs/protocol.md    # evidence-labelled protocol notes (VERIFIED/CORROBORATED/INFERRED)
  data/70-hyprdeck-corsair.rules
  .github/workflows/ci.yml       # copy of Razer ci.yml + jq protocol checks
  .github/workflows/release.yml  # copy; id "corsair", ASSET hyprdeck-corsair-linux-x86_64, attach rules
  src/main.rs         # Request/State/Product/Group/Row/Visualization mirror types; dispatch();
                      # --request | --input-stream <id> | state | action <name> [json] | --help | --version;
                      # overall Instant deadline 13 s; envelope; tests: reject before hardware
  src/hidraw.rs       # sysfs enumeration (/sys/class/hidraw → device → interface bInterfaceNumber,
                      # /sys/bus/usb/devices idVendor=1b1c), open O_RDWR|O_CLOEXEC|O_NONBLOCK,
                      # HIDIOCGRAWINFO identity, HIDIOCGRDESC usage-page check, flock(LOCK_EX|LOCK_NB) retry,
                      # write/poll/read with deadlines; GTK-free unit tests for parsing
  src/protocol.rs     # Corsair framing encode/decode + property/command ids (pure fns, fixture-byte tests)
  src/devices/mod.rs  # model table: PIDs (2b07 SLIPSTREAM dongle, K65 PLUS wired PID, K100 RGB / K100 AIR PIDs),
                      # capabilities per model; no capability without verified protocol evidence
  src/devices/k65_plus.rs, src/devices/k100.rs
  src/layout.rs       # embedded per-model key rects (layout units), ids, labels, LED index map;
                      # validate against the same limits the host enforces (≤512 keys, valid_id, bounds)
  src/products.rs     # one product per physical keyboard (dongle+keyboard = one product, kind "keyboard");
                      # sections: overview (keyboard viz + status/battery), lighting, keys ("Keys & Macros"),
                      # power, firmware, diagnostics (collapsed raw HID/interface info)
  src/lighting.rs     # actions: set-key-colors (args.value {keys,color}), lighting form (effect/brightness)
  src/remap.rs        # remap-key action; shared option list
  src/settings.rs     # $XDG_CONFIG_HOME/hyprdeck-corsair/*.json, versioned, flock + atomic rename (dpi_presets pattern)
  src/cache.rs        # $XDG_STATE_HOME/hyprdeck-corsair/, identity-guarded (Razer cache.rs pattern)
  src/input.rs        # --input-stream: read-only non-exclusive evdev of the keyboard, NDJSON frames
                      # {"status","pressed":[ids]}; final "unavailable" frame on error; BrokenPipe = clean exit
```

**udev rule draft** (INFERRED; interface numbers must be confirmed by the protocol research):
```
# Corsair SLIPSTREAM dongle 1b1c:2b07: vendor configuration interface only (input1, usage page 0xFF42).
ACTION=="add|change", SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1b1c", ATTRS{idProduct}=="2b07", ENV{ID_USB_INTERFACE_NUM}=="01", TAG+="uaccess"
# (add wired K65 PLUS / K100 PIDs and their vendor interface numbers once verified)
```

**Design constraints carried over from the host (INFERRED from the VERIFIED facts above):**
- **No daemons.** A request process is killed after its response (`process.rs:87-89`). If the keyboard needs a continuously attached host to stay in software-lighting mode, which is how OpenLinkHub works as a service, the plugin can only:
  - (a) write onboard or hardware lighting, or
  - (b) keep software mode alive only while the `--input-stream` process runs, that is only while the view is mapped. The stream is not killed after 15 s (`backend.rs:311-385`).

  The protocol research must decide which of these is possible.
- **Concurrency.** `state` runs at least every 2 s (§2.7). The stream process may hold the device at the same time. Request and action processes are serialized against each other by the host lock, but **not** against the stream process. Use Razer's `flock` on the vendor hidraw fd with a short retry. Keep the stream on evdev or the IN-only interface so it never contends with input1 writes.
- **Response size.** Keep every response under 1 MiB, with shared remap options (§3.1). Labels must be plain text. Every action re-validates device identity and arguments.
- **Fallback state.** Always emit useful rows next to the visualization, because older hosts ignore or reject it (`H/docs/PLUGINS.md:148-150`). With no keyboard connected, CI's headless state check must still return `error: null` and a groups array, so report "no device" as a group rather than an error.
- **Version order.** Ship the host's `keyboard` kind before any plugin release that emits it.
