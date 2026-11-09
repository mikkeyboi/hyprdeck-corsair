# Development

This guide describes the integrated implementation. [DESIGN.md](DESIGN.md) is
the build specification; [hardware-evidence-k65plus.md](hardware-evidence-k65plus.md)
is the source of hardware claims. Neither mocked tests nor a successful compile
promotes researched behavior to hardware-verified behavior.

## Environment and repository boundaries

- Linux, current stable Rust with edition 2024 support, `rustfmt`, `clippy`, and
  Python 3. CI/release runners use Ubuntu 22.04, Linux x86-64.
- Dependencies: `anyhow`, `libc`, `serde`, `serde_json`, `evdev 0.13`. No GTK,
  hidapi, async runtime or external Corsair daemon is linked.
- `Cargo.lock` is committed; every build/test gate uses `--locked`.
- The Hyprdeck host is a separate repository. Keyboard visualization requires
  its `keyboard_visualization` feature. Do not edit a user's host checkout to
  integrate this plugin; respect unrelated work there.
- Preserve unrelated/uncommitted changes and inspect the tree before editing.
  Do not paper over warnings with crate/module-wide lint suppression.

## Source map

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | One binary's request/CLI, input stream, companion and rescue entry points |
| `src/devices.rs` | Sysfs discovery, serial/topology identities and per-link capabilities |
| `src/bragi.rs` | Transport trait, HID identity/flock, framing, allowlists and scoped mode changes |
| `src/effects.rs` | Onboard effect encoding/decoding; unknown descriptors retain raw bytes |
| `src/storage.rs` | XDG paths, bounded JSON, atomic locked updates, write counter/cache |
| `src/config.rs` | Versioned live config/looks; macro and action validation |
| `src/ipc.rs` | NDJSON socket types/client and transport proxy |
| `src/layouts/` | Independently authored ANSI/ISO geometry and key-name tables |
| `src/plugin/` | Validate/dispatch requests, state rendering, actions, visualization, stream and user-service management |
| `src/daemon/` | Device workers/ownership, keepalive, frames/dial, input forwarding/remaps/macros and IPC server |
| `src/rescue.rs` | Explicit close-handle/mode-1 recovery |
| `data/` | Base HID and optional input/uinput permission rules |
| `scripts/` | Offline packaging assertions and isolated CLI smoke checks |

The shared generic `Transport` interface allows deterministic fake keyboards.
Plugin `Backend` and companion `Platform` traits isolate HID, evdev/uinput,
command execution and service-manager calls in tests. The companion owns a HID
session while configured live features or pending restoration require it; plugin traffic for an
owned device goes through IPC instead of competing for the lock.

## Safety contract

Before changing protocol code, read the allowlists and their rejection tests.

1. **Never read or write resource `0x0002`.** A read dropped the physical K65 off
   its receiver; key-table writes risk dead keys without a recoverable backup.
   No flash-directory, pairing, firmware-update or arbitrary command facility
   is exposed. Do not add a “raw command” CLI to bypass the allowlists.
2. SET/resource operations that require software mode use scoped mode changes.
   Close handles before restoring mode 1; verify restoration where possible.
   When mode is already 2, the scope does not assume it owns the mode transition.
3. Status 6 means asleep/disconnected: request a keypress, not repeated writes.
   GET timeouts may retry once after 200 ms; SET/resource commands are not
   blindly retried after an ambiguous outcome.
4. Persistent-setting actions compare readback (or write-only sleep cache) and
   skip unchanged values. Budget records are reserved before writes, capped at
   500 per device per UTC day; a failed attempted write can still consume budget.
   Live per-key frames are not persistent effect writes.
5. Create a working virtual input sink before grabbing physical input. Refuse
   held-key or unknown-state grabs; drain pre-grab events and refuse boundary
   races. Set shutdown cancellation before queueing shutdown. Shared output
   ownership reference-counts each physical node, macro job, and dial separately;
   failed releases remain tracked until retried or the virtual device is destroyed.
   Capture each press disposition across reload; expanding the source set waits
   for an idle boundary before reacquiring nodes. SYN_DROPPED skips to the next
   report and reconciles authoritative key state without replaying macros. A failed
   reconciliation ungrabs. Configuration/source generations cancel old macro jobs
   with a nonblocking bounded queue; normal input forwarding never waits on it.
   Fn remains firmware-handled. Treat command actions as trusted user config.
   Dial ownership requires a live IF2 reader and output sink before mode entry;
   reader loss restores/retries. Layout discovery also runs in hardware mode for
   remap-only ISO operation. Key hubs retain latest state without subscribers;
   overflow disconnects, and source changes clear/invalidate/reseed subscriptions.
   The tester rechecks sources at bounded 500 ms polling cadence, subject to the
   bounded IPC timeout, clearing missing-source state before reconnect.
6. Never infer successful persistence from a successful SET or a mock test.
   Receiver K65 brightness/Static red power-cycle persistence and Win-lock
   functional behavior are evidenced; sleep/Win-lock persistence is not. K100
   and K65 USB behavior, especially wired lighting, is hardware-unverified.

### Timeout and cleanup limitations

`Bragi::with_software_mode` arms its guard before entry. An ambiguous entry
failure still attempts restoration; a definite device rejection disarms it.
Normal operation errors are combined with restoration errors. Panic-unwind Drop
cleanup is best-effort and cannot report an additional error to the caller.

Plugin dispatch gives every request one monotonic **10 s work deadline** and a
**3 s cleanup reserve**, within the host's 15 s process allowance. The request
thread is cancelled cooperatively and **joined**, never detached on a normal
timeout. HID lock/reply/resource loops, IPC request/reply reads and JSON update
locks check the shared deadline. Software-mode guards use the original cleanup
deadline rather than allocating another reserve per packet. An external SIGKILL
or a kernel/filesystem call that does not return still cannot be made safe by
Rust guards; restoration errors are surfaced, not converted into success.

Companion shutdown waits up to **1900 ms**, joining only finished workers.
Unfinished workers may not complete restoration before process exit; the unit's
`ExecStopPost` rescue is a fallback attempt, not proof of success. Offline or
unresponsive hardware can reject cleanup. Do not rely on the researched wired
watchdog timing or advertise a receiver watchdog guarantee.

### Remaining validation and boundary work

This safety reconciliation is a compiling/tested checkpoint, not permission to
install or use against hardware. The offline regressions do not establish a
lossless EVIOCGRAB handoff: a press can race after the final state check, and
post-grab race refusal currently ungrabs without replaying the discarded boundary
events. Source-set teardown waits on the forwarder's cached held state, not an
atomic kernel idle/stop handshake. Those boundaries need a dedicated lossless
transition design and additional race-injection tests before readiness claims.

The shared shutdown flag interrupts held-key waiting, but the daemon's 1900 ms
shutdown wait can still expire before a HID/resource/restore operation completes.
Normal plugin request timeouts now join; that is distinct from daemon exit.
Remap-only layout is discovered independently in hardware mode and on a new
worker/reconnect, but a successful remap-only probe is not periodically refreshed
on wake with unchanged paths. These are explicitly unresolved in the numbered
safety outcomes; no hardware readiness is asserted.

Refresh is not purely GET-only: each keyboard gets property reads and K65 effect
readback when its 60-second cache is stale/missing. That readback uses a scoped
software-mode dip and local cache writes. A separate persisted last-attempt timestamp applies the same 60 s automatic
backoff after failed reads. Explicit Re-read bypasses that backoff. If recording
the cadence fails, automatic mode-changing readback is skipped.

## Exact local gates

Run from the repository root:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
python3 -I scripts/check-packaging.py
python3 -I scripts/smoke.py target/release/hyprdeck-corsair
```

Set `TMPDIR` to an existing private scratch directory before running tests/smoke
when the workspace requires it. Rust filesystem tests use that directory. Unix
socket fixtures keep a directory fd open and address the socket through
`/proc/self/fd/<fd>/daemon.sock`, preventing Linux's roughly 108-byte socket-path
limit from breaking deeply nested scratchpads; the actual files remain inside
the scratch directory. The smoke subprocess inherits only its private runtime
directory fd for the same reason. This test helper is not a production socket
fallback and does not change the documented XDG socket location.

All unit tests must execute assertions with implemented key-name/layout modules.
Do not wrap staging tests in `catch_unwind` to silently skip missing modules.
The panic-unwind mode-restoration test intentionally catches a panic and asserts
that it occurred and cleanup succeeded; keep that test.

Coverage includes protocol framing/allowlists, mode restoration on return/error/
unwind, effect bytes, fake sysfs discovery, config/macro parsing and limits,
geometry/key-name invariants, host-envelope/control validation and capability
gating, IPC round trips, and fake companion dial/grab/frame/shutdown behavior.
No hardware access is needed for these tests. Record the actual count/output of
each verification run instead of hard-coding a count as a maintained promise.

### Safe executable smoke

`scripts/smoke.py` makes **new empty** sysfs and dev roots and separate config,
data, state, cache and runtime directories. It sets:

```text
HYPRDECK_CORSAIR_SYSFS = <private temporary directory>/sys
HYPRDECK_CORSAIR_DEV   = <private temporary directory>/dev
XDG_CONFIG_HOME       = <private temporary directory>/config
XDG_DATA_HOME         = <private temporary directory>/data
XDG_STATE_HOME        = <private temporary directory>/state
XDG_CACHE_HOME        = <private temporary directory>/cache
XDG_RUNTIME_DIR       = short fd alias into <private temporary directory>/run
```

It checks 12 cases: help/version, state without/with keyboard capability, bad
API version, unknown action, valid action with a missing device, malformed JSON,
oversize request, human CLI state/error status and unknown CLI command. It
asserts no files/sockets were written and sys/dev stayed empty. Creating the
private runtime subdirectory is expected.

**Never run** `daemon`, `rescue`, a real device action, or the key stream against
real hardware as a headless integration check. An empty fake-root daemon smoke
and mock-socket executable test may run with isolated XDG and device overrides. Do not open `/dev/hidraw*`, `/dev/input/*` or
`/dev/uinput`, install services, or execute `sudo`, `systemctl` or `udevadm` while
running offline gates. A fake sysfs fixture must not point at a real device
node. Hardware tests require a separate deliberate session with the device
owner; do not run sweeping resource probes.

## Host protocol and configuration

One request on stdin to `<exe> --request`; one compact envelope on stdout.
Diagnostics belong on stderr. Input is capped at 1 MiB and the response stays
under the host's 1 MiB cap (large visualizations are omitted first).

```json
{"api_version":1,"method":"state","action":null,"args":{},"host_features":["keyboard_visualization"]}
```

```json
{"api_version":1,"method":"action","action":"set-brightness","args":{"device":"k65plus-example","value":50}}
```

The second example is a protocol shape, **not an offline command to run against
a keyboard**. The error envelope is
`{"api_version":1,"error":"...","state":null}`. `--request` normally exits 0
even for protocol errors; inspect `error`. Human CLI `state`/`action` exits 1 on
an error and bad CLI usage exits 2. Unknown actions/invalid args are rejected
before backend I/O. Older hosts without the feature get no keyboard
visualization; layouts and live streams must remain capability-gated.

See [README.md](../README.md#controls-and-configuration) for XDG locations and
macro syntax. `live.json` and `looks.json` carry version 1 and per-device maps.
The UI writes changes atomically and asks a running companion to reload; malformed
config must not overwrite good config. Saved macros only run when bound to a
remap/dial action. Protocol action names/argument types are parsed in
`src/plugin/actions.rs`; extend tests whenever those types or render controls
change. IPC uses `hello`, serialized `reload`, property-only legacy `xfer`, typed atomic
`rpc`, and `keys`. Resource RPCs are whole transactions (no borrowed packet
handles). A queued RPC expires before sending any command; the owner performs
read-compare-skip, polling-cap checks, and persistent-write accounting. The plugin
counts only direct writes, never proxy writes a second time. `hello` adds optional
`generation`, `config_revision`, `frame_applied`, and `remaps_active`; mode 0 means
uncertain. Old/missing fields or a stale revision never acknowledge current UI
preview as applied. The FNV64 revision is content correlation, not authentication.
Socket modes and allowlists are independent of host UI validation.

## Permissions, CI and release review

The base rule has 14 `uaccess` grants: seven supported PIDs × command/event
interfaces. It first imports `usb_id` for supported Corsair hidraw devices when
`ID_USB_INTERFACE_NUM` is missing; a hidraw event need not inherit identity from
an input event. USB VID/PID ancestor matches remain on the same parent device;
do not combine them with a mismatched-parent `ATTRS{bInterfaceNumber}` shortcut.

The optional rule grants uinput access, seven physical-device input patterns and
the companion virtual-device pattern. It exposes user-level key reading and
system-wide input injection, so remains a separate opt-in file. Rules sort
before systemd's late seat-ACL application. Actual logind/udev matching and
hotplug permissions need manual validation on a real Linux seat; the offline
script checks textual scope and identity, **not** actual ACL application.

`ci.yml` runs packaging assertions, all four Cargo gates and the isolated smoke
on pushes to main and pull requests with read-only repository permissions.
`release.yml` runs the same gates for `v*` tags or a manually selected existing
tag, validates Cargo/plugin/tag/asset identity, then assembles:

```text
hyprdeck-corsair-linux-x86_64
hyprdeck-corsair-linux-x86_64.sha256
plugin.json
70-hyprdeck-corsair.rules
71-hyprdeck-corsair-input.rules
```

The checksum covers the binary under its published name. `plugin.json` continues
to declare the installed executable `hyprdeck-corsair`, distinct from the asset
name. The release job can write contents; it refuses to replace an existing
GitHub release. Running local gates does **not** publish a release. Release
builds use the runner's dynamic libc, not a musl/static target; validate the
published binary on a clean supported distribution before claiming portability.

Release checklist: align Cargo/plugin versions and `v<version>`, rerun gates,
review hardware evidence and udev privacy notices, verify host compatibility and
asset/checksum names, and test the resulting binary offline. Tag/push/publish
steps are maintainer operations only when explicitly requested.

## Local GUI iteration and contributions

Follow [README installation](../README.md#install-in-hyprdeck) only after offline
checks. Local host installation copies only the manifest/executable: embed any
runtime resources instead of assuming files beside them get installed. When
replacing an existing installation, first stop/disable the separately enabled
companion in its UI, disable the host plugin and deliberately replace **only**
the Corsair plugin's installed directory. Do not remove saved configuration or
modify another plugin. Re-enable the plugin, then the companion if wanted.

For contributions: keep patches scoped, run the exact gates, add assertions for
new behavior and report what is mocked versus hardware-tested. Independently
restate protocol facts; never copy GPL project code or data tables. See
[research](research/bragi-k65plus.md) and [MIT license](../LICENSE).
