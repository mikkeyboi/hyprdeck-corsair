# hyprdeck-corsair

A standalone [Hyprdeck](https://github.com/mikkeyboi/hyprdeck) plugin for Corsair
K65 PLUS Wireless and K100 RGB keyboards. One Rust binary provides the plugin,
key tester, recovery command and an **opt-in** live-mode companion. No
OpenLinkHub, iCUE, GTK library or root daemon is required.

**Hardware/onboard mode is the default.** Brightness, onboard effects and
Win-lock use brief software-mode dips; the companion is only needed for custom
dial actions, wired per-key lighting, and OS-level remaps/macros. Installing or
enabling the Hyprdeck plugin does not automatically install the companion or
udev rules.

## Support and evidence

| Keyboard/link | USB VID:PID | Implementation and verification |
| --- | --- | --- |
| K65 PLUS Wireless, SLIPSTREAM receiver | `1b1c:2b07` | Protocol and selected operations hardware-verified; see limits below |
| K65 PLUS Wireless, USB cable | `1b1c:2b10`, `2b11`, `2b23` | Implemented from protocol research; **hardware-unverified**, including wired lighting |
| K100 RGB, USB (OPX v1 / MX / OPX v2) | `1b1c:1b7c`, `1b7d`, `1bc5` | Implemented from protocol research; **hardware-unverified**, including brightness and per-key lighting |

K100 AIR is recognized as unsupported and receives no controls. Bluetooth,
firmware updates, pairing, onboard remap/macro flashing and iCUE profile import
are not supported.

The [K65 hardware evidence](docs/hardware-evidence-k65plus.md) establishes:

- Brightness and **Static red** effect persistence across a keyboard power cycle
  on the receiver-linked K65. It does not prove every effect's persistence.
- FN brightness changes can be read back; FN effect keys and host effect writes
  share the live onboard effect slot `0x6D60`.
- Win-lock's functional blocking of Super in hardware mode, **not** its
  power-cycle persistence. Sleep timeout persistence is also **not verified**;
  its displayed value is the last value set by this plugin, not device readback.
- Software-mode key/dial reports and restoration to hardware mode after a long
  session. This does not amount to hardware validation of the whole companion.

Polling-rate writes and other lock-mask combinations remain experimental.
Read the [design](docs/DESIGN.md) as a build specification, not a blanket claim
that all intended features have been tested on hardware.

## Quick start: build and check without hardware

Requirements: Linux, a current stable Rust toolchain supporting edition 2024,
Python 3 and a Hyprdeck installation for the graphical interface. Release CI
builds on Ubuntu 22.04 for Linux x86-64; builds are not fully static.

From a checkout:

```sh
cargo build --locked --release
./target/release/hyprdeck-corsair --version
python3 -I scripts/check-packaging.py
python3 -I scripts/smoke.py target/release/hyprdeck-corsair
```

`smoke.py` creates empty fake sysfs/dev trees and isolated XDG directories. It
never runs the daemon, rescue, service commands or actions on a real keyboard.
Set `TMPDIR` to an existing private scratch directory first if required by your
workspace. The script uses a short `/proc/self/fd` alias for its private runtime
directory so deeply nested scratch paths work with Linux Unix sockets.

## Install in Hyprdeck

After the offline checks, stage the **manifest and executable together**:

```sh
mkdir -p dist/local
install -m 0755 target/release/hyprdeck-corsair dist/local/hyprdeck-corsair
install -m 0644 plugin.json dist/local/plugin.json
hyprdeck plugins install ./dist/local
hyprdeck plugins enable corsair
hyprdeck --page plugin:corsair
```

The host copies those two files into
`${XDG_DATA_HOME:-$HOME/.local/share}/hyprdeck/plugins/corsair`. A release ships
`hyprdeck-corsair-linux-x86_64`, `plugin.json`, a binary SHA-256 checksum and both
udev rules. When staging a downloaded release locally, name the executable
`hyprdeck-corsair` as the manifest requires, and verify its checksum first.

Older API-v1 hosts get ordinary controls. Keyboard drawing, paint/remap selection
and the visual key tester require a host that advertises
`keyboard_visualization`; the plugin never sends an unknown visualization to an
older host. This repository does not change or install the host.

## Device permissions

The rules are supplied as files and release assets; neither Hyprdeck nor this
plugin installs them. The following commands are **manual administrator steps**,
not part of building or offline testing.

### Required: vendor HID access

Install the base rule from the repository root:

```sh
sudo install -m 0644 data/70-hyprdeck-corsair.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

Then unplug/replug the cable or receiver. On a systemd/logind system, `uaccess`
grants the active local seat user access to the supported devices' vendor HID
interfaces 01/02. The rule imports USB identity before matching the interface;
it does not change device groups/modes or detach kernel keyboard drivers.
Headless or remote sessions may not receive a seat ACL.

### Optional: remaps, macros, custom dial and key tester

**Privacy/security trade-off:** granting access to these input nodes lets any
process running as the seat user read keys typed on those keyboards. Access to
`/dev/uinput` lets that user inject keyboard/mouse events system-wide, not just
for Corsair devices. Do not install this rule if you do not want that capability.
Existing unrelated input rules may already grant it.

```sh
sudo install -m 0644 data/71-hyprdeck-corsair-input.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

Replug the keyboard/receiver; ensure your system supplies the `uinput` device.
The rule also covers the companion's virtual input devices. Base lighting and
power controls need only the HID rule; OS input features need the optional rule.

## Controls and configuration

In Hyprdeck:

- **Lighting:** K65's ten onboard effect choices, speed where applicable, Static
  color, brightness, lights-off/full presets and saved looks. K100 hardware mode
  offers brightness, not an editor for iCUE's onboard effects.
- **Keys & Macros:** Win-lock and lock-mask toggles; remaps and saved macros run
  through the companion's evdev/uinput layer. They do not alter the keyboard's
  onboard key tables and do not follow the keyboard to another computer.
- **Dial:** default Volume remains onboard; Scroll, Zoom, Brightness, Media,
  Workspace and Custom require the companion. Workspace uses `hyprctl`.
- **Per-key lighting:** wired links only, through the companion; K100 has keys,
  LightEdge, logo and wheel-ring zones. All wired lighting is hardware-unverified.
- **Power & performance:** write-only sleep setting on K65, cached as “last set”,
  and polling-rate choices capped by the device's reported maximum.

Macro steps are comma-separated, for example:

```text
ctrl+shift+t, delay:50, text:hello world, media:play_pause
```

Other steps include `down:shift` and `up:shift`. At most 64 steps, with delays at
most 5000 ms each. Text injection assumes a US layout; Fn shortcuts remain
firmware-handled and are not remappable. Custom command actions execute argv
without a shell, as your user: treat configuration as trusted executable input.

| File | Purpose |
| --- | --- |
| `$XDG_CONFIG_HOME/hyprdeck-corsair/live.json` | Versioned per-device remaps, macros, dial and wired lighting |
| `$XDG_CONFIG_HOME/hyprdeck-corsair/looks.json` | Versioned saved looks |
| `$XDG_STATE_HOME/hyprdeck-corsair/cache.json` | Effect bytes/timestamps and last-written values |
| `$XDG_STATE_HOME/hyprdeck-corsair/writes.json` | Per-device persistent-write budget, 500 writes per UTC day |
| `$XDG_RUNTIME_DIR/hyprdeck-corsair/daemon.sock` | Private companion IPC socket, mode 0600 |

Config/state defaults are `~/.config` and `~/.local/state`; runtime falls back to
`/run/user/<uid>`. Device ids use serials, with a USB topology fallback when no
serial is available. A link change may therefore select a different config id.
The plugin creates config/state files when needed, not at install time. Use the
UI to edit config; malformed files are reported, not silently overwritten.

## Opt-in companion and recovery

“Enable live mode” writes
`$XDG_CONFIG_HOME/systemd/user/hyprdeck-corsair.service`, reloads the user service
manager and enables/starts it. The unit uses the installed executable, restarts
on failure and runs `rescue --all` after stopping. “Disable live mode” stops and
disables it but retains configuration and the unit. “Restart” updates the unit's
executable path before restarting. Disabling the **plugin** in Hyprdeck is not
the same as disabling this separate service.

Only configured dial/per-key-lighting features need sustained software mode.
Remaps/macros need an input grab, not software mode. Live-mode owners hold the
HID lock and proxy plugin requests. By default, no device is owned or grabbed.

**Restoration is best-effort, not guaranteed on every path.** Scoped operations
attempt to close handles and restore/verify mode 1 on normal return, errors and
panic unwind; an already-mode-2 device is left to its owner. Plugin work uses
a shared 10-second deadline and a 3-second cleanup reserve. Normal timeout
cancels and joins its worker before returning; the host killing the process
can still prevent cleanup. Device timeouts/disconnects,
SIGKILL and the companion's bounded shutdown can also leave mode uncertain. Do
not rely on a watchdog: the receiver-linked K65 stayed in software mode during
a roughly 130-second keepalive session without reverting.

Plain refresh reads properties, but an expired/missing K65 effect cache also
causes a brief software-mode dip and updates local state. Successful effect
readback is cached for 60 seconds; “Re-read from keyboard” forces it. If readback
fails, a separately recorded last-attempt time prevents another automatic
mode dip for 60 seconds; the UI may show a stale value with an error. Live
lighting/remaps are labelled applied only after the companion acknowledges the
same saved configuration revision and successful frame/grab status.

If the keyboard is asleep/offline (status 6), press a key and refresh. If mode or
lighting looks stuck, use Diagnostics → Rescue, or deliberately run:

```sh
hyprdeck-corsair rescue --all
# Or: hyprdeck-corsair rescue <device-id>
```

This is a **real hardware operation**, closing handles 1/2 and attempting mode-1
restoration with readback. It can fail on an unresponsive device; stop conflicting
controllers and wake/reconnect the keyboard before retrying. Never run it as an
offline smoke test. Do not run another Corsair controller concurrently.

## CLI, development and license

`--help` lists `--request`, `state`, `action <name> [json]`, `--input-stream <id>`,
`daemon` and `rescue`. `state` can access hardware and perform an effect readback
dip; it is not a guaranteed GET-only diagnostic. `--request` consumes one JSON
request and emits one API-v1 envelope. Protocol errors are carried in `error`
with `state: null`; CLI `state`/`action` also return a nonzero exit code on error.

See [development and safety checks](docs/DEVELOPMENT.md) for architecture,
contribution rules, exact gates, protocol examples and release packaging.
[MIT](LICENSE). Protocol research is documentation only: do not copy code or
data files from GPL reference projects.

### Safety reconciliation checkpoint

The current offline-tested safety changes are not a hardware-readiness claim.
Lossless grab/source handoff races, bounded daemon shutdown completion, and
remap-only layout refresh on unchanged-path wake remain follow-up work; see
[Development limitations](docs/DEVELOPMENT.md#remaining-validation-and-boundary-work).
No service/rule installation or hardware test is part of the reconciliation gates.
