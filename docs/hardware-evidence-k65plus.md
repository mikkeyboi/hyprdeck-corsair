# K65 PLUS Wireless — live read-only probe (2026-11-07, OpenLinkHub stopped)

Transport: /dev/hidraw3 (dongle 1b1c:2b07, interface 1, vendor page 0xFF42 usage 1).
Write 65 bytes: [0x00 report id, target, cmd, prop, 0...]. target 0x08 = dongle, 0x09 = paired keyboard.
Response 64 bytes: [r0, cmd echo, status, value LE ...]. r0 = 0x00 for dongle, 0x01 for keyboard (subdevice).
Status: 0 = ok, 3 = unsupported on dongle, 7 = unsupported/not-gettable on keyboard.
Only GET PROPERTY (0x02) was sent. Full sweep 0x00–0xff on both targets.

## Dongle (target 0x08) — supported GET props
| prop | value | meaning |
|---|---|---|
| 0x01 | 04 | polling rate code (4 → likely 1000 Hz) |
| 0x03 | 01 | mode: 1 = hardware (OLH released it) |
| 0x11 | 0x1b1c | VID |
| 0x12 | 0x2b07 | PID (Slipstream receiver for K65 Plus) |
| 0x13 | 01 44 | app firmware → 1.68 (matches OLH "1.68.0") |
| 0x14 | 01 00 | bootloader version |
| 0x36 | 02 | paired/connected subdevice bitmask (slot 1 connected) |

## Keyboard (target 0x09) — supported GET props
| prop | value | meaning |
|---|---|---|
| 0x01 | 04 | polling rate code |
| 0x02 | 0x03e8 = 1000 | brightness, 0–1000 (100 %) |
| 0x03 | 01 | mode: hardware |
| 0x0f | 0x01a4 = 420 | battery, 0–1000 → 42.0 % |
| 0x10 | 01 | battery state (1 = charging? or discharging — verify with cable) |
| 0x11 | 0x1b1c | VID |
| 0x12 | 0x2b10 | PID (K65 PLUS wired PID) |
| 0x13 | 05 1a 9a | app firmware bytes (decode TBD; OLH showed "50.0.0") |
| 0x14 | 00 09 0e | bootloader bytes |
| 0x15 | 01 27 | unknown (maybe hardware rev / layout id) |
| 0x41 | 01 | unknown (layout? 1 = ANSI/US?) |
| 0x45 | 00 | OLH uses 0x45 as a control flag ([0x45,0x00,0x01/0x00]) — see source |
| 0x4a | 00 | OLH "cmdPerformance" (Win-lock / performance flags) |
| 0x96 | 04 | unknown |

Notably NOT gettable on keyboard: 0x0e (OLH sets sleep timeout via set 0x0e — write-only or different id).

# Session 2 (2026-11-08) — write/persistence probes, hardware-verified with the user

- Resource reads/writes and property SETs are rejected in hardware mode (status 7). Pattern that works:
  SET mode 2 (`01 03 00 02`) -> operation -> SET mode 1 (`01 03 00 01`). Mode dips of ~1 s caused no visible problem.
- Brightness `01 02 00 <u16>` written in mode 2 stays after returning to mode 1 AND survives a keyboard power cycle. [V user]
- On-keyboard FN+F6 changes are visible to host GET 0x02 (500 -> 700). FN brightness steps look like +200.
- Effect slot 0x6D60 is the LIVE onboard effect:
  - Host write (OLH framing: open `0d 01 60 6d`, 0.5 s, probe `09 01`, `06 01 <len16 LE> 00 00 <hdr+data>` chunks of 61, `07 01` continuation, close `05 01 01`) of Static red `7e 20 01 | 00 00 00 01 ff 00 00 ff` (data padded to 93, len field 93) shows immediately in hardware mode and survives a power cycle. [V user]
  - Firmware stores a compact form: Watercolor read back as 11 bytes, Rain as 4 bytes `7e a0 02 03`.
  - Pressing FN+3 rewrote 0x6D60 to Rain (`7e a0 02 03` = id 0xA07E, colour mode 2 random, speed 3). So FN effect keys and host writes share the slot; reading it gives the true current effect.
- Win-lock: SET 0x4A=1 then 0x45=1 in mode 2 -> Super key blocked in hardware mode [V user]; both read back. Restored to 0/0.
- Charge state 0x10: 0 not charging, 1 charging, 2 not charging, 3 full (OLH stats.go). Observed 2 on battery at 99%.
- DANGER: reading resource 0x0002 (key table) via `08 01` timed out and the keyboard dropped off the dongle (status 6 on every command, dongle 0x36=0) until a keypress/power cycle. Never read 0x0002 this way. Status 6 = child offline.
- Current device state left: Rain (from FN+3), brightness 700, mode 1, Win-lock off.

## Live-mode (software mode) behaviour, K65 PLUS via dongle [V user + capture]
- Hardware mode: IF2 (hidraw if02, usage page FF42 usage 2, IN-only 64 B) is SILENT for FN+4, FN+F6 and dial turns. No change notifications -> host must read state back.
- Software mode (SET 03=2, keepalive `12` to 0x09 every 5 s, ~130 s, no watchdog revert):
  - Keys still type normally (IF0) and are ALSO reported on IF2 as `01 02 <bitmap>`: bit = HID usage (a=4, 1=30, space=44, F6=63), modifiers 105..112, Fn = 122, dial press = 137.
  - FN shortcuts (FN+F6 brightness, FN+1 effect) still handled by firmware. Onboard lighting effect keeps running (no freeze) when entering software mode.
  - The dial no longer controls volume/mute: rotation arrives as `01 05 60 00 <i32 LE>` (+1 cw, -1 ccw), press as key bit 137. Host must emulate dial actions.
- Restoring mode 1 works after a long software session.
