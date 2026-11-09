# Corsair "Bragi" (V2 peripheral) HID protocol — K65 PLUS Wireless (SLIPSTREAM + wired), K100 notes

Research for the hyprdeck Corsair plugin (Rust, direct hidraw). Protocol facts only; no GPL code reproduced.

## 0. Sources and conventions

| Tag | Root |
|---|---|
| `OLH/` | `<scratch>/src/OpenLinkHub/` (GPL-3.0, reference only) |
| `CKB/` | `.../scratchpad/src/ckb-next/src/daemon/` (GPL-2.0) |
| `ORGB/` | `.../scratchpad/src/OpenRGB/Controllers/CorsairPeripheralV2Controller/` (GPL-2.0) |
| `WEB1` | ckb-next issue #1060 "K65 PLUS Wireless - 2b10" (github.com/ckb-next/ckb-next/issues/1060) |

Evidence labels: **[V]** = VERIFIED-IN-SOURCE (cited file:line), **[C]** = CORROBORATED (two independent sources agree), **[I]** = INFERRED (reasoned, not directly shown; needs a hardware test).

Note: OpenRGB and ckb-next do **not** support the K65 PLUS itself; they corroborate the generic Bragi framing, commands, properties and lighting formats. All K65-PLUS-specific facts come from OLH only (so are at best [V]).

Byte offsets below are written two ways where it matters:
- **wire/report offsets** = the 64-byte HID report (what hidraw returns on read; what goes on the bus).
- **write-buffer offsets** = the 65-byte buffer passed to `write()` on hidraw, which carries a leading report-ID byte `0x00` (interface has no numbered reports). `wbuf[i] == report[i-1]`.

---

## 1. Device identification

### 1.1 USB IDs

| Device | VID:PID | Evidence |
|---|---|---|
| K65 PLUS WIRELESS — SLIPSTREAM USB receiver (the user's dongle) | `1b1c:2b07` (11015) | [C] `OLH/src/devices/devices.go:929` (`11015 → k65plusWdongle.Init`, interface 1); WEB1 (user report "This device also has the Id: 2b07") |
| K65 PLUS WIRELESS — wired USB (cable) | `1b1c:2b10` (11024) | [C] `OLH/src/devices/devices.go:840`; WEB1 (`ID 1b1c:2b10 Corsair CORSAIR K65 PLUS WIRELESS Keyboard`) |
| K65 PLUS WIRELESS — wired, variant | `1b1c:2b11` (11025) | [V] `OLH/src/devices/devices.go:841` |
| K65 PLUS WIRELESS — wired, variant | `1b1c:2b23` (11043) | [V] `OLH/src/devices/devices.go:842` |
| Same three PIDs are what the dongle reports for the paired keyboard | 11024/11025/11043 | [V] `OLH/src/devices/k65plusWdongle/k65plusWdongle.go:127-193` |
| K100 RGB (optical v1 / MX / optical v2) | `1b1c:1b7c`, `1b1c:1b7d`, `1b1c:1bc5` | [C] `OLH/src/devices/devices.go:848-850` (7036/7037/7109); `ORGB/CorsairPeripheralV2Devices.h:89-91`; `CKB/usb.h:116-118` |
| K100 AIR WIRELESS — wired USB | `1b1c:1bab` (7083) | [V] `OLH/src/devices/devices.go:847` |
| K100 AIR via a generic SLIPSTREAM receiver (dongle PID one of 0x1bdc, 0x1b7f, 0x1ba6, 0x2b00, 0x1b65; V2 = 0x2b1b) — paired PID reported as 7083 | | [V] `OLH/src/devices/devices.go:904-909`, `OLH/src/devices/slipstream/slipstream.go:284-286` |

### 1.2 HID interfaces (both dongle and wired keyboard)

- **Interface 1**, vendor usage page `0xFF42` usage 1, 64-byte IN + OUT, no report IDs: the **command channel** (request → response). [C] OLH opens interface 1 for 11015/11024/… (`OLH/src/devices/devices.go:840-842,929`; table field `InterfaceId` at `devices.go:165-171`, enumeration `devices.go:612-624`); ORGB registers every V2 device on interface 1, usage page 0xFF42 (`ORGB/CorsairPeripheralV2ControllerDetect.cpp:92-139`).
- **Interface 2**, vendor page 0xFF42 usage 2, IN-only: **notifications + software-mode input** (key bitmap, dial, battery, link status). [V] OLH opens `InterfaceNbr == 2` as "listener" for dongle (`k65plusWdongle.go:578-641`) and for wired (`k65plusWU.go:2914-2930`).
- **Interface 0**: standard boot keyboard / consumer / mouse collections — normal typing in hardware mode. [I] (matches user's enumeration; ckb-next comment `CKB/keymap.c:776-779` says in HW mode all wireless events come from the dongle as plain HID).

### 1.3 Bluetooth

- K65 PLUS has 2.4 GHz SLIPSTREAM, Bluetooth and USB modes (Corsair product page, WEB search result). In BT mode the keyboard talks directly to the host BT stack; nothing passes through the SLIPSTREAM dongle, so **BT is not reachable via the dongle's hidraw**. [I]
- OLH has no BT device entry for K65 PLUS (no PID in `devices.go`), so there is no known Bragi-over-BLE path. [V by absence] Whether the vendor collection is exposed over HOGP is unknown. [I]
- The K100 AIR (and probably K65 PLUS) has a **connection-mode property 0x3A** that switches radio: values 1 = SLIPSTREAM, 2/3/8 = BT host profile 1/2/3. [V for K100 AIR only] `OLH/src/devices/k100airW/k100airW.go:2812-2822, 2896-2920`; `cmdBluetoothMode = 01 3a 00 02`, `cmdUsbMode` GET/SET 0x3A=0 at `k100airW.go` var block. Do not write 0x3A on a K65 PLUS without testing (it would drop the dongle link). [I]

---

## 2. Transport framing

### 2.1 OUT (host → device), interface 1

```
write-buffer (65 bytes):  [0]=0x00 (report id) [1]=DEV [2]=CMD [3..64]=args, zero padded
report on the wire (64):  [0]=DEV [1]=CMD [2..63]=args
```

- **DEV** (ckb-next calls it "magic"; OLH calls it "endpoint"; OpenRGB "write_cmd"):
  - `0x08` = the directly attached device (wired keyboard, or the dongle itself). [C] `OLH/src/devices/k65plusWU/k65plusWU.go:2878` (`bufferW[1] = 0x08`); `OLH/.../k65plusWdongle.go:68` (`cmdCommand = 0x08`); `CKB/bragi_proto.h:4` (`BRAGI_MAGIC 0x08`); `ORGB/CorsairPeripheralV2Controller.h:27` (`WRITE_WIRED_ID 8`).
  - `0x08 | n` = sub-device n behind a receiver; the K65 PLUS behind its dongle is always **`0x09`** (n=1). [C] `OLH/.../k65plusWdongle.go:297` (`Endpoint: 0x09`), `k65plusW.go:2398` (`bufferW[1] = d.Endpoint`); `CKB/usb_bragi.c:61-65` (`pkt[0] |= kb->bragi_child_id` when routing through parent); `ORGB/CorsairPeripheralV2Controller.h:28` (`WRITE_WIRELESS_ID 9`), `.cpp:38-47`.
  - Generic multi-device SLIPSTREAM: `0x08 + deviceType` (or 0x09 if only one device). [V] `OLH/src/devices/slipstream/slipstream.go:801-812`.
- Report size: 64 bytes for K65 PLUS (wired and dongle) [V] `k65plusW.go:143-146`, `k65plusWU.go:151-154`, dongle `k65plusWdongle.go:53-54`. Some boards (K100 RGB, K65 MINI, K70 PRO) use **1024-byte "jumbo"** reports [C] `CKB/usb.h:316` (`USES_BRAGI_JUMBO`), `OLH/src/devices/k100/k100.go` var block (`bufferSize = 1024`).

### 2.2 IN (device → host), interface 1 = responses

```
report (64): [0]=DEV echo [1]=CMD echo [2]=STATUS [3..63]=payload
```
- [C] `CKB/bragi_common.h:149-154` (success = `out[1]==in[1] && in[2]==0`); `ORGB/CorsairPeripheralV2Controller.cpp:306-317` (status `read[2]`, value `read[3..6]`); OLH reads values from `[3..]` everywhere (e.g. battery `binary.LittleEndian.Uint16(batteryLevel[3:5])`, `k65plusW.go:668`).
- `[0]` low 3 bits identify the device (0 = receiver/self, 1..7 = child). [C] `CKB/keymap.c:786-797` (`devid = firstbyte & 0x7`); OLH slipstream checks `msg[0] == 0x01/0x02` for children (`slipstream.go:1255`) — i.e. echo may be `0x09` or `0x01`; **match on `byte0 & 0x07`**. [I on exact echo value]
- One request → exactly one response. OLH wired/keyboard does write then blocking read (`k65plusW.go:2393-2418`); OLH dongle **drains stale input non-blocking before every write** then reads with 100 ms timeout (`k65plusWdongle.go:643-697`). ckb-next waits up to 2 s (`CKB/usb_bragi.c:80-81`). ORGB 50 ms (`.h:24`). Recommendation: drain, write, read with ~100–500 ms timeout, verify `byte1 == CMD` and `byte0&7 == DEV&7`, loop/discard otherwise. [I]

### 2.3 Multi-packet writes (block write)

First packet (cmd 0x06), then continuations (cmd 0x07):

```
report: [0]=DEV [1]=0x06 [2]=HANDLE [3..6]=TOTAL_LEN u32 LE [7..63]=first 57 data bytes
report: [0]=DEV [1]=0x07 [2]=HANDLE [3..63]=next 61 data bytes   (repeat)
```
- [C] `CKB/bragi_common.c:60-102` (header at pkt[0..6], data at +7, continuation header 3 bytes); `ORGB/CorsairPeripheralV2Controller.cpp:377-460` (first-packet data at write-buffer offset 8 = report offset 7, continuation at write-buffer 4 = report 3); OLH builds `[len u16 LE][00 00][data…]` and chunks to 61 bytes after the 2-byte command (`k65plusW.go:1923-1946`, `common/common.go:554-580`).
- ORGB waits for the first packet's response and sleeps 2 ms between continuations because some devices (K100) drop back-to-back packets (`ORGB/...Controller.cpp:416-454`, `WRITE_PACE 2`). OLH reads a response after **every** chunk (its `transfer` always reads). ckb-next also `usbrecv`s every continuation (`CKB/bragi_common.c:94`). → The device **does** answer continuation packets on the K65 PLUS path OLH uses. [C for "read after each packet works"]
- ckb-next always puts handle 0 in continuation packets (`CKB/bragi_common.c:92`), OLH uses the right handle (`0x07 0x01` for handle 1, `k65plusW.go:129,139`) — use the real handle. [V]

---

## 3. Command set

| CMD | Name | Request args (report offsets) | Response | Evidence |
|---|---|---|---|---|
| `0x01` | SET property | `[2..3]`=prop id u16 LE (usually `[3]=0x00`), `[4..]`=value LE (1–4 bytes) | status | [C] `CKB/bragi_proto.h:7`, `CKB/bragi_common.c:23-36`; `ORGB/...Controller.h:56`; OLH `cmdSoftwareMode = 01 03 00 02` (`k65plusW.go:123`) |
| `0x02` | GET property | `[2..3]`=prop id | `[3..6]` value LE | [C] `CKB/bragi_common.c:6-19`; `ORGB/...Controller.cpp:291-318` |
| `0x05` | CLOSE handle | `[2]=0x01` (constant), `[3]`=handle | status | [C] `CKB/bragi_common.c:169-177`; OLH `cmdCloseEndpoint = 05 01 01` (`k65plusW.go:137`); ORGB `StopTransaction` (`.cpp:341-358`) |
| `0x06` | WRITE (first block) | `[2]`=handle, `[3..6]` total length u32 LE, `[7..]` data | status | see §2.3 |
| `0x07` | WRITE continue | `[2]`=handle, `[3..]` data | status | see §2.3 |
| `0x08` | READ data | `[2]`=handle | `[3..63]` next 61 bytes of the resource | [C] `CKB/bragi_common.c:124-167`; OLH dongle `cmdRead = 08 00` (`k65plusWdongle.go:64`) |
| `0x09` | PROBE handle (size) | `[2]`=handle | `[5..8]` resource length u32 LE | [V] `CKB/bragi_common.c:109-122`; OLH calls it `cmdWrite = 09 00` (misnomer, `k65plusWdongle.go:65`) and `09 01` after opening the lighting file (`k65plusW.go:703`) |
| `0x0D` | OPEN handle | `[2]`=handle (0..2), `[3..4]`=resource id u16 LE | status (0x03 = already open) | [C] `CKB/bragi_common.c:179-195`; `ORGB/...Controller.cpp:320-339`; OLH `0d 01 60 6d`, `0d 00 22`, `0d 01 02`, `0d 00 24` |
| `0x12` | PING / heartbeat (no args) | — | `[1]=0x12` | [C] `CKB/device_bragi.c:14-36` (50 s poll, "CUE polls every 52 s"); OLH `cmdHeartbeat = 12` (`k65plusW.go:140`, `k65plusWdongle.go:60`), `cmdKeepAlive` (`k65plusWU.go:137`) |
| `0x0B` | unknown ("init protocol" on resource 0x6d65) | `[2..3]` resource | — | [V, meaning I] `OLH/src/devices/k100airW/k100airW.go` var `cmdInitProtocol = 0b 65 6d` (K100 AIR only) |
| `0x15` | unknown ("flush", arg handle) | `[2]`=handle | — | [V, meaning I] `k100airW.go` var `cmdFlush = 15 01`, used after writing an effect file (`k100airW.go:2687-2690`) |

Notes:
- Property ids are effectively **u16 LE at `[2..3]`**: OLH uses `01 13 01` (motion sync) and `01 14 01` (ripple control) on mice, i.e. props 0x0113/0x0114 distinct from 0x13/0x14. [I, from the OLH property survey]
- Handles are small integers 0..2 chosen by the host; resources are 16-bit ids. ckb-next convention: handle 0 = lighting, 1 = generic (quick read/write, never leave open), 2 = second lighting (`CKB/bragi_proto.h:69-72`). OLH K65 PLUS wireless uses handle 1 for both the lighting-effect file and the key-assignment table; wired uses handle 0 for direct lighting and handle 1 for key assignment. [V]

## 4. Status codes (response byte `[2]`)

| Code | Meaning | Evidence |
|---|---|---|
| 0x00 | OK | [C] CKB, ORGB |
| 0x01 | Invalid value / not supported (some K100s return 0x01 when opening resource 0x01) | [C] `ORGB/...Controller.cpp:174-187`; `CKB/device_bragi.c:93-97` |
| 0x03 | Failed / handle state error: returned by OPEN when handle already open, and on writes to a handle that is not open | [C] `ORGB/...Controller.cpp:180-181` ("Failed"); `CKB/bragi_common.c:185-191`, `CKB/device_bragi.c:91-92` |
| 0x05 | Unsupported (property/command not implemented) | [C] `CKB/bragi_proto.h:84`; `ORGB/...Controller.cpp:182-183` |
| 0x06 | Seen on K100 when opening resource 0x01 → use 0x22 instead | [V] `CKB/device_bragi.c:93-98` |

OLH never checks the status byte on the K65 PLUS path (`k65plusW.go:2393-2418`). [V] Our implementation must.

---

## 5. Addressing the dongle vs the keyboard; paired-device discovery

- Dongle: DEV `0x08` on dongle interface 1. Keyboard behind it: DEV `0x09` on the **same** hidraw node (dongle interface 1). Responses for both come back on that node. [C] (§2.1)
- Sub-device connection bitfield: GET property **0x36** on the dongle → bit n set ⇔ child n connected (bits 1..7). K65 PLUS = bit 1 → value `0x02`. [C] `CKB/bragi_proto.h:57`, `CKB/bragi_common.c:244-323`, `CKB/device_bragi.c:269-276`; OLH `setDeviceStatus` 0x00 = all offline, 0x02 = single device online (`k65plusWdongle.go:488-497`).
- Child identity: GET 0x11 (VID) and 0x12 (PID) addressed to DEV 0x09 (`ffff` = no device). [C] `CKB/bragi_common.c:299-313`; `ORGB/...Controller.cpp:38-47` (`GetAddressRetry(0x12)` via 0x09, retries 8× 120 ms because a just-enumerated receiver answers 0 or error).
- Paired-device list resource **0x24** on the dongle (OLH-only): `05 01 24` (close; harmless), `0d 00 24` (open handle 0 → resource 0x24), `09 00` (probe), `08 00` (read), `05 01 00` (close). [V] `k65plusWdongle.go:373-409`. K65-PLUS-dongle record layout (report offsets of the first READ response): `[5]` = device count (>0), `[6..7]` VID LE, `[8..9]` PID LE, `[10]` serial length, `[11..]` serial ASCII. [V] `k65plusWdongle.go:281-308`. The generic SLIPSTREAM parser uses a different record layout (`vid[0..1], ?[2..3], pid[4..5], type[6], idLen[7], id[8..]` from report offset 6) [V] `slipstream.go:791-831` — the two firmwares differ; dump real bytes before trusting either. [I]
- Keyboard serial is only obtainable via resource 0x24 (USB serial string of the dongle is the dongle's). [V/I]

---

## 6. Property table (SET 0x01 / GET 0x02)

| Prop | Meaning | Encoding / range | K65 PLUS use in OLH | Evidence |
|---|---|---|---|---|
| 0x01 | Polling rate | u8: 1=125 Hz(8 ms), 2=250, 3=500, 4=1000, 5=2000, 6=4000, 7=8000 | not exposed for K65 PLUS | [C] `CKB/bragi_proto.h:17,50-54`; `OLH/src/devices/k100/k100.go:230-239,1012-1043` |
| 0x02 | Brightness (fine) | u16 LE **0..1000** (1000 = 100 %) | yes: SET on connect, Fn+F5/F6 and dial ±100, "off" = 0 | [C] `k65plusW.go:126,1180-1202,1907-1921,2140-2156`; `CKB/led_bragi.c:110-111` (1000 = on, 0 = off) |
| 0x03 | Device mode | u8: 1 = hardware, 2 = software; (0x04 = force sleep on some mice; 0x06 = SW mode variant used by K100 AIR, K57, K70 PRO MINI W, Vanguard, MAKR75…) | SW on connect, HW on stop | [C] `CKB/bragi_proto.h:19,47-48`; `ORGB/CorsairPeripheralV2Devices.h:19-23`; `k65plusW.go:123-124`; `k100airW.go` (`cmdSoftwareMode = 01 03 00 06`); `OLH/src/devices/m75W/m75W.go:1089-1097` (0x04 sleep) |
| 0x07 | Angle snapping (mice) | u8 | — | [C] CKB, OLH |
| 0x0D | Sleep enable (K100 AIR writes 1 before 0x0E) | u8 | not used for K65 PLUS | [V] `k100airW.go:1021-1043` |
| 0x0E | Sleep / idle timeout | **u32 LE milliseconds** (OLH offers 5,10,15,30,60 min) | yes (wireless only) | [V] `k65plusW.go:131,946-960`, options `k65plusW.go:207-213` |
| 0x0F | Battery level | u16 LE **per-mille 0..1000** (÷10 = %) | yes (connect + every 10 s) | [C] `k65plusW.go:660-671`; `CKB/device_bragi.c:316-325` |
| 0x10 | Battery / charge status | u8; OLH labels 0 = not charging, 1 = charging, 2 = not charging (discharging), 3 = full | yes (connect) | [V] `OLH/src/stats/stats.go:68-90`; [C exists] `CKB/bragi_proto.h:44` |
| 0x11 | USB VID (read-only) | u16 | — | [C] `CKB/bragi_proto.h:41` |
| 0x12 | USB PID (read-only) | u16 | — | [C] `CKB/bragi_proto.h:42`; ORGB `.cpp:46` |
| 0x13 | Application FW version | `[3]`=major `[4]`=minor `[5..6]`=build u16 LE → "maj.min.build" | yes, keyboard (DEV 0x09) and dongle (DEV 0x08) | [C] `k65plusW.go:681-694`, `k65plusWdongle.go:338-346`; `CKB/device_bragi.c:167-169` |
| 0x14 | Bootloader version | as 0x13 | — | [V] `CKB/bragi_proto.h:22` |
| 0x15 / 0x16 | Radio app / radio bootloader version (wireless) | as 0x13 | — | [V] `CKB/bragi_proto.h:23-24` |
| 0x1E/0x1F/0x21/0x22/0x2F-0x31 | DPI stuff (mice) | — | — | [V] CKB |
| 0x36 | Sub-device connection bitfield (receiver) | u8 bitfield | yes (notification) | [C] §5 |
| 0x3A | Connection mode (radio) | 1 = SLIPSTREAM, 2/3/8 = BT profile 1/2/3 (K100 AIR) | not used for K65 PLUS | [V] `k100airW.go:2812-2822,2896-2920` |
| 0x40 | Inactivity time (s, u16 at `[3..4]`) | — | used by generic SLIPSTREAM mice only | [V] `slipstream.go:1238-1277` |
| 0x41 | Physical layout | 1 = ANSI, 2 = ISO, 3 = ABNT, 4 = JIS | not read for K65 PLUS (user picks US/UK/ISO JSON) | [C] `ORGB/CorsairPeripheralV2Devices.h:33-39`, `ORGB/...Controller.cpp:258-261`; `CKB/bragi_proto.h:32`, `CKB/device_bragi.c:254-261` |
| 0x44 | Brightness (coarse) | 0..3 | — | [V] `CKB/bragi_proto.h:33`, `CKB/led_bragi.c:108-109` |
| 0x45 | "Performance"/lock enable (Win-lock master switch) | u8 1/0 | yes | [V] `k65plusW.go:624-634` (`01 45 00 01|00`) |
| 0x4A | Lock mask (what the lock disables) | u8 bits: 1 = Win key, 2 = Alt+Tab, 4 = Alt+F4, 8 = Shift+Tab | yes (written as 0 when lock off) | [V] `k65plusW.go:591-622`; same in `k100airW.go:555-604`, `k100.go` |
| 0x5F | "light_ctrl_reg" (ORGB does a GET of it after entering SW mode) | ? | — | [V, meaning I] `ORGB/...Controller.h:116`, `.cpp:234-256`, `ORGB/CorsairPeripheralV2SoftwareController.cpp:17-18` |
| 0x6D | Secondary brightness (K100 RGB writes 0x02 and 0x6D) | u16 | — | [V] `k100.go` var `cmdBrightness` map, `k100.go:1908-1922` |
| 0x09/0x0A, 0x39/0x38 | Debounce enable / time (K100 RGB): enable = 1, time = 30 + n (n 1..9) | u8 | — | [V] `k100.go` vars, `k100.go:1925-1982` |
| 0x96 | Max polling rate (untested) | — | — | [V] `CKB/bragi_proto.h:36` |
| 0xC4/0xC5 | Auto-brightness (K100 AIR) | u8 | — | [V] `k100airW.go:721-758` |

Not found in any source for K65 PLUS: battery-saver property, Fn-lock property, dedicated "game mode" property other than 0x45/0x4A, sleep/wake notification. [V by absence]

---

## 7. Resources ("files") opened via 0x0D

| Resource | Purpose | Format | Evidence |
|---|---|---|---|
| 0x0001 | Direct per-key lighting, **planar** `RRR…GGG…BBB…` (N bytes each) | length = 3N | [C] `CKB/bragi_proto.h:60`, `CKB/led_bragi.c:92-101`; `ORGB/CorsairPeripheralV2SoftwareController.cpp:26-46` |
| 0x0010 | Monochrome lighting | N bytes | [V] `CKB/bragi_proto.h:61` |
| 0x0022 | Direct per-key lighting, **RGB triplets** with 2-byte header `12 00` | length = 2 + 3N | [C] `CKB/bragi_proto.h:63`, `CKB/led_bragi.c:128-166` (`pkt[7]=0x12`, len `3*zones+2`); `ORGB/CorsairPeripheralV2HardwareController.cpp:64-88`; OLH wired K65 PLUS `0d 00 22` + `12 00` (`k65plusWU.go:133,136,2216-2242`) |
| 0x002E | Secondary lighting (K100 top bar / extra zone) | header `2b 00` in OLH | [C] `CKB/bragi_proto.h:65`, `CKB/device_bragi.c:107-111`; `k100airW.go` (`cmdActivateLedTopBar = 0d 00 2e`, `dataTypeSetColorTopBar = 2b 00`) |
| 0x0002 | Key-assignment table (what each key does natively in SW mode) | K65 PLUS 123 bytes, K100 138, K100 AIR 135 | [V] `k65plusW.go:136,150,352-396,2359-2391`; `k100.go` (`0d 02 02`, 138); `k100airW.go` (`keyAssignmentLength = 135`) |
| 0x0005 / 0x0006 | Pairing id (8 B) / encryption key (16 B) | | [V] `CKB/bragi_proto.h:66-67`, `CKB/device_bragi.c:202-232,333-348` |
| 0x0013 | Effect index/list (K100 AIR writes `69 6c 01 00 08 00 65 6d`) | | [V, meaning I] `k100airW.go:760-799` |
| 0x0024 | Receiver's paired-device list | see §5 | [V] `k65plusWdongle.go:59,281-308` |
| **0x6D60** | **K65 PLUS onboard (hardware) lighting effect** | effect record (§9.2) | [V] `k65plusW.go:125` (`0d 01 60 6d`) |
| 0x6D65 | K100 AIR onboard lighting effect | effect record + LED list | [V] `k100airW.go` (`cmdActivateLed = 65 6d`), `k100airW.go:2640-2690` |

---

## 8. Initialization / sequencing

### 8.1 What OLH does — K65 PLUS via SLIPSTREAM (dongle 0x2b07)

Dongle init, `k65plusWdongle.Init` (`k65plusWdongle.go:73-109`):
1. Open dongle interface 1. Read manufacturer/product/serial strings (USB descriptors).
2. `[08] 02 13` → dongle FW. (`:338-346`)
3. `[08] 01 03 00 02` → dongle to **software mode**. (`:356-362`)
4. Read resource 0x24 (paired list) via `[08]`: `05 01 24`, `0d 00 24`, `09 00 24`, `08 00 24` (+ extra `08` reads), `05 01`. (`:373-409`) → keyboard PID/serial, DEV = 0x09.
5. Create `k65plusW` object sharing the same HID handle (`:125-198`).
6. Start 10 s monitor (§10) (`:500-541`).
7. Open interface 2 listener, loop with 100 ms read timeout (`:563-641`).
8. `[09] 12` heartbeat to keyboard; if it answers, `Connect()` the keyboard. (`:411-443`)

Keyboard `Connect()` (`k65plusW.go:459-477`), all with DEV 0x09:
1. `checkDeviceOnline`: up to 10 × `[09] 12`, 1 s apart (`:436-456`).
2. `01 03 00 02` software mode (`:673-678`).
3. `02 0f` battery (`:660-671`).
4. `02 13` firmware (`:681-694`).
5. compute key count from layout; `initLeds`: `0d 01 60 6d` (open handle 1 → resource 0x6D60), **sleep 500 ms**, `09 01` (probe) (`:696-709`).
6. `setDeviceColor`: `initLeds` again, then block-write the chosen hardware effect to handle 1 (§9.2) (`:1621-1904`). Note OLH returns **without closing handle 1** in every effect branch (close at `:1900` is only reached when no branch matches). [V]
7. `01 02 00 <u16 brightness>` (`:1907-1921`).
8. `01 0e 00 <u32 ms>` sleep (`:946-960`).
9. `setupPerformance`: `01 4a 00 <mask>`, `01 45 00 <0|1>`, then `setupKeyAssignment` → `0d 01 02` (open handle 1 → resource 0x02), `06 01 <len32> <123 bytes…>` + `07 01 …`, `05 01 01` (close handle 1) (`:591-640`, `:352-396`, `:2359-2391`).
10. `02 10` charge status (`:244-252`).

### 8.2 What OLH does — K65 PLUS wired (0x2b10/11/23), DEV 0x08 (`k65plusWU.go:185-265`)
serial → `01 03 00 02` SW mode → `02 0f` battery → `0d 00 22` open handle 0 on direct-lighting resource 0x22 + sleep 500 ms (never closed) → `02 13` FW → 20 s keepalive timer → first color frame → performance/lock + key assignment → interface-2 listener → `01 02 00 <brightness>` → 20 ms frame loop for animated software effects. No sleep-timer write on wired. [V]

### 8.3 ckb-next generic Bragi init (for comparison) (`CKB/device_bragi.c:147-288`)
GET 0x03; if HW → SET 0x03=2; GET 0x13/0x14/0x15/0x16, 0x01, 0x96, 0x02 (fine) else 0x44 (coarse); wireless: read pairing id (open h1 res 0x05, probe, read, close); **SET 0x03=1 back to HW**; GET 0x41 (keyboards); when activated: SET 0x03=2, start 50 s ping thread, open handle 0 on resource 0x01 (fallback 0x22 + handle 2 on 0x2E). Dongle: same common start, SET 0x03=2 on the dongle, GET 0x36 and probe children. [V]

### 8.4 Recommended sequence for hyprdeck (Rust) [I, synthesized]
1. Find dongle (1b1c:2b07) interface 1 and 2 hidraw nodes via sysfs (usage page 0xFF42, usage 1 vs 2) — or wired 2b10/2b11/2b23.
2. Drain interface 1. `[08] 12` ping; `[08] 02 13` dongle FW; `[08] 02 36` → bit1 set? else keyboard asleep/off — wait for notification.
3. `[09] 02 12` (expect 0x2b10/2b11/2b23), `[09] 02 13` keyboard FW, `[09] 02 0f`, `[09] 02 10`.
4. Only if we want software features: `[08] 01 03 00 02` then `[09] 01 03 00 02`; **always** write the key-assignment table (default = all keys native) immediately after entering SW mode so typing keeps working (§11.3); start 10–20 s ping.
5. Always close handles you opened; before OPEN, CLOSE the handle first (cheap, avoids status 0x03).
6. On exit/suspend: close handles, `[09] 01 03 00 01`, `[08] 01 03 00 01`.

---

## 9. Lighting

### 9.1 Wired: per-key direct frames (resource 0x22, handle 0) — K65 PLUS
- Open once: `0d 00 22 00` (handle 0, resource 0x0022); OLH sleeps 500 ms after. [V] `k65plusWU.go:133,654-660`
- Frame payload = `12 00` + **371-byte RGB-triplet buffer** (123 LEDs × 3 = 369, +2 spare) → TOTAL_LEN = 373. [V] `k65plusWU.go:156,2216-2242` (length written as `len(buf)+2`). [C] format with `CKB/led_bragi.c:128-166` and `ORGB/CorsairPeripheralV2HardwareController.cpp:64-88`.
- Packets: `06 00 <75 01 00 00> 12 00 <55 bytes>` then 6 × `07 00 <61 bytes>` (64-byte reports). [V/derived]
- Byte order inside each triplet: **R, G, B**. Offset of LED *i* = 3·i within the 371-byte buffer (i.e. report data offset 2 + 3i after the `12 00`). [C] `k65plusWU.go:1863-1876` (`buf[pi]=Red, +1=Green, +2=Blue`); ckb-next/ORGB triplets.
- OLH zeroes LED index 1 (bytes 3..5) on every frame and excludes offset 3 from the OpenRGB map. [V] `k65plusWU.go:2218-2221`, `:2756`. LED 1 is one of the three spacebar LEDs (0, 1, 44) — keep it 0 [I: possibly a phantom/duplicate LED].
- Frame rate: OLH renders animated effects every 20 ms (50 fps). [V] `k65plusWU.go:2080-2083`, queue worker `:2149-2188`.
- Brightness scaling is separate: property 0x02 (0..1000). [V]

### 9.2 Wireless (via dongle): onboard effect programming (resource 0x6D60, handle 1) — K65 PLUS
OLH does **not** stream per-key frames over SLIPSTREAM to the K65 PLUS; it rewrites the keyboard's onboard lighting effect. Corroborating hint: WEB1 reporter: "limited onboard memory in wireless mode … defaults to preset lighting effects when not plugged in". [V] for OLH behaviour; [I] that per-key streaming is unavailable wirelessly (untested).

Sequence: `0d 01 60 6d` (open h1 → res 0x6D60), sleep 500 ms, `09 01` (probe), `06 01 <len32> <record…>`, `07 01 <…>`… [V] `k65plusW.go:696-709,1923-1946`

Effect record (bytes after the 4-byte length field): [V] layout from OLH branches; [C] field meaning against ORGB's (commented-out) HW-mode writer `ORGB/...Controller.cpp:487-508` and mode enum `ORGB/...Controller.h:64-86`.

```
off 0-1  effect id u16 LE
off 2    colour mode: 0 none, 1 specific colours, 2 random, 3 (unknown, used by watercolor)
off 3    speed: 0x03 slow, 0x04 medium, 0x05 fast
off 4    direction / variant byte
off 5    0
off 6    number of colours n
off 7..  n × [brightness(0xff), B, G, R]          <- note BGR order
rest     zero padding (OLH total data 93 bytes for 3-byte headers, 89 for 4/5-byte headers)
```

| OLH name | Effect id (bytes) | ORGB name | Header bytes OLH writes | Evidence |
|---|---|---|---|---|
| keyboard (static colour) | 0x207E (`7e 20`) | STATIC | `7e 20 01`, then `00 00 00 01 ff B G R` | `k65plusW.go:1662-1674` |
| off | 0x207E | STATIC | static with colour 0,0,0 | `:1648-1660`, also RgbOff `:1632-1644` |
| rain | 0xA07E | RAIN | `7e a0 02 spd 01` | `:1676-1698` |
| tlk (type-lighting key) | 0xB1F9 | REACTIVE | `f9 b1 02 spd` | `:1700-1723` |
| tlr (type-lighting ripple) | 0x09A2 | RIPPLE | `a2 09 02 spd` | `:1725-1748` |
| spiralrainbow | 0xAB87 | SPIRAL | `87 ab 00 spd 06` | `:1750-1773` |
| colorpulse | 0xAD4F | FLASHING | `4f ad 02 spd` | `:1775-1796` |
| colorshift | 0xA5FA | BREATHING | `fa a5 02 spd` | `:1798-1819` |
| colorwave | 0x7BFF | SPECTRUM | `ff 7b 02 spd 04` | `:1821-1844` |
| rainbowwave | 0xB94C | RAINBOW | `4c b9 00 spd 04` | `:1846-1869` |
| watercolor | 0x0022 | WATERCOLOR | `22 00 03 spd`, data `[2]=01, [3..6]=ff ff ff ff` | `:1871-1897` |
| (K100 AIR only) visor | 0x90C0 | VISOR | — | `ORGB/...Controller.h:77`, `k100airW.go` rgbModes |

All 11 ids [C] (OLH bytes == ORGB enum values). The TOTAL_LEN OLH writes is `len(data)` excluding the 3–5 header bytes (93 or 89) — inconsistent with ckb-next/ORGB which count everything; the firmware evidently tolerates it [V `k65plusW.go:1927`]; we should send the true total. [I]

K100 AIR variant appends `[8] = LED count` and `[9..]` = list of LED indices the effect covers, then `05 01 01` close and `15 01` flush. [V] `k100airW.go:1754-1790,2640-2690`. OLH's K65 PLUS record has no LED list (zeros). [V]

### 9.3 Planar alternative (resource 0x01) — not used by OLH for K65 PLUS
`RRR…GGG…BBB…` N each, handle 0. [C] CKB/ORGB. Whether K65 PLUS accepts 0x01 is unknown; ORGB's probe logic: open 0x22, if status > 0 use 0x01 (`ORGB/...Controller.cpp:114-136`). [I]

### 9.4 LED index map (K65 PLUS) — `packetIndex` scheme
- `OLH/database/keyboard/k65plus.json` (US; also `k65plus-uk.json`, `k65plus-iso.json`): each key has `packetIndex: [byteOffset…]` into the 371-byte triplet buffer, always a multiple of 3; **LED index = packetIndex / 3**. [V] e.g. ESC `[123]` → LED 41 (`k65plus.json` row 0 key 1), Space `[0, 3, 132]` → LEDs 0, 1, 44 (`k65plus.json:1378`).
- The LED index equals the **HID keyboard usage id** for normal keys and the Bragi key-bitmap index (§11.1); modifiers are remapped to 105..112; Fn = 122. [C] OLH JSON vs `CKB/keymap.c:328-470` (`keymap_bragi`: index 41 = esc, 122 = fn, 105 lctrl … 112 rwin).
- Also per key: `keyData = [bitmapIndex, nativeValue, remappedValue]` and `keyHash` = decimal of `1 << bitmapIndex`. [V] (dump of k65plus.json)

Full US map (key → LED/bitmap index):

```
Row0: Esc41 F1 58 F2 59 F3 60 F4 61 F5 62 F6 63 F7 64 F8 65 F9 66 F10 67 F11 68 F12 69 Del76
Row1: `53 1 30 2 31 3 32 4 33 5 34 6 35 7 36 8 37 9 38 0 39 -45 =46 Bksp42 Home74
Row2: Tab43 Q20 W26 E8 R21 T23 Y28 U24 I12 O18 P19 [47 ]48 \49 PgUp75
Row3: Caps57 A4 S22 D7 F9 G10 H11 J13 K14 L15 ;51 '52 Enter40 PgDn78
Row4: LShift106 Z29 X27 C6 V25 B5 N17 M16 ,54 .55 /56 RShift110 Up82
Row5: LCtrl105 Win108 LAlt107 Space{0,1,44} RAlt111 Fn122 RCtrl109 Left80 Down81 Right79
ISO/UK extras: #(non-US) 50, ISO \| 100
```
Total coloured LEDs = 83 keys → 85 LED slots (space uses 3); buffer covers indices 0..122 (`LEDChannels: 123`, `k65plusW.go:182`). [V]

---

## 10. Keepalive and timeout

- OLH dongle monitor every **10 s**: `[08] 12`, then for the keyboard `[09] 12` and `[09] 02 0f` (battery). [V] `k65plusWdongle.go:56,500-541`
- OLH wired every **20 s**: `[08] 12` + battery GET. [V] `k65plusWU.go:149,872-904`
- ckb-next: ping every 50 s ("CUE polls devices every 52 seconds"). [V] `CKB/device_bragi.c:14-36`
- OpenRGB: K55 RGB PRO must receive a packet within 1 minute or it reverts to its onboard rainbow; sends a keepalive at most every 30 s. [V] `ORGB/RGBController_CorsairV2Hardware.cpp:63-70,238-251`, `.h:34-35`
- ⇒ Without host traffic for ~60 s, a Bragi device drops out of software mode back to hardware (onboard) behaviour. [C] (two sources, other models) / [I] for the K65 PLUS specifically. Recommendation: ping every 10–20 s while in SW mode.

---

## 11. Interface-2 reports (notifications and SW-mode input)

Report layout (64 bytes, report offsets): `[0]` = device id (low 3 bits; 0 dongle, 1 child), `[1]` = report kind:

| `[1]` | Kind | Layout | Evidence |
|---|---|---|---|
| 0x01 | Property-change notification | `[2]` = prop id, `[3]` = 0, `[4..]` = value LE | [C] `CKB/bragi_proto.h:76,81`, `CKB/bragi_notification.c:22-38`; OLH `k65plusWdongle.go:620-630` |
| 0x02 | SW-mode key bitmap | `[2..21]` = 160-bit LE bitmap (bit i ⇒ key index i pressed) | [C] `CKB/keymap.c:909-912` (`corsair_kbcopy(keys, buffer+2)`); OLH `k65plusW.go:2049-2063,2426-2440` |
| 0x05 | Dial rotation | `[2]` = 0x60 (mask), `[4..7]` = int32 LE delta (OLH reads `[4]` as 0x01 = CW, 0xFF = CCW) | [C] `CKB/keymap.c:913-925`; `k65plusW.go:2441-2521` |
| 0x09 | (mouse tilt, generic SLIPSTREAM) | | [V] `slipstream.go:1340-1370` |

Notifications seen:
- **Battery**: `[1]=01 [2]=0f [4..5]` u16 per-mille. [V] `k65plusWdongle.go:620-626`; wired `k65plusWU.go:2955-2961` (note OLH bug: `||` instead of `&&`).
- **Charge status**: `[2]=10 [4]` status (0..3 as §6). [V] generic `slipstream.go:1433-1437` (keyed by `[0]`).
- **Link status (receiver)**: `[1]=01 [2]=36 [4]` bitfield; 0x00 = keyboard gone (off / sleeping / out of range), 0x02 = keyboard (child 1) connected. Generic SLIPSTREAM also carries child PID at `[10..11]`. [C] `k65plusWdongle.go:628-630,488-497`; `CKB/bragi_notification.c:22-31`; `slipstream.go:1443-1447`.
- No distinct sleep/wake notification exists in any source; keyboard sleep shows up as 0x36 → 0x00 and wake as 0x36 → 0x02. [I]

### 11.1 Key bitmap index (K65 PLUS)
`bit i` at `report[2 + i/8] & (1 << (i%8))`.
- Normal keys: i = HID usage (A=4 … Right=79, Del=76, Home=74, PgUp=75, PgDn=78, Up=82 …).
- Modifiers byte `report[15]` (bits 104..111): LCtrl 0x02, LShift 0x04, LAlt 0x08, LWin 0x10, RCtrl 0x20, RShift 0x40, RAlt 0x80 (bit 0 = 104 = vol-down on ckb map). [C] `k65plus.json` `modifierPosition: 15` (`:14`) and `modifierPacketValue`s; `CKB/keymap.c` keymap_bragi 105..112.
- **Fn**: bit 122 → `report[17] & 0x04`. [C] `k65plusW.go:2428`, `k65plusWU.go:2964`; `CKB/keymap.c` (fn = 122, used at `:658`).
- **Dial press**: bit 137 → `report[19] & 0x02`. [C] OLH `raw[17]==0x02` with `raw = report[2:22]` (`k65plusW.go:2051,2065`); ckb-next `ctrlwheelb` = 137 (`CKB/keymap.c` keymap_bragi).
- Other indices on other boards: 102 mute, 103 vol+, 104 vol−, 113 light, 114 lock, 123 stop, 124 play, 125 next, 126 prev, 128 profile switch, 131-136 G1-G6, 116/130 dial CW/CCW (ckb synthesized). [V] `CKB/keymap.c:328-470`

### 11.2 What SW mode means for key input
- Standard HID keyboard reports on interface 0 **continue** in SW mode for keys whose key-assignment entry says "native"; the vendor bitmap is sent **in addition** for every key. [C] OLH does nothing for `key.Default` keys and relies on native HID (`k65plusW.go:2177-2180`); ckb-next comment "NKRO … does NOT get disabled when the keyboard is in software mode … Corsair input packets are sent along with NKRO ones" (`CKB/keymap.c:552-557`).
- **Fn-layer functions are NOT performed by the firmware in SW mode**; the host must implement them from the bitmap (Fn bit + key bit): OLH maps Fn+F7..F12 to Prev/Play/Next/Mute/Vol−/Vol+, Fn+F5/F6 to brightness −/+100, Fn+L to toggle the Win-lock (0x45/0x4A). [C] `k65plus.json` (F7..F12 `mediaKey` + `fnActionCommand` 5,6,7,3,2,1; F5 `actionType 12`, F6 `actionType 11`; L `isLock`), `k65plusW.go:2134-2174`, action codes `OLH/src/inputmanager/inputmanager.go:112-118`; ckb-next applies the same "awful hack" for K60/K70 CORE Fn media keys in SW mode (`CKB/keymap.c:652-700`). F1/F2 carry `fnActionCommand` 124/125 (screen brightness down/up, `inputmanager.go:235-236`) but OLH never acts on them. [V]
- The **dial** stops emitting native volume events in SW mode; the host must translate type-0x05 reports. [C] OLH emulates volume etc. (`k65plusW.go:2441-2521`); ORGB destructor comment: "Some devices (e.g. K70 Core RGB) stop generating onboard input events (e.g. the volume knob) while left in software render mode, and the mode persists until the next USB enumeration if it is never reverted" (`ORGB/...Controller.cpp:139-152`).
- Receiver: "In HW mode, all wireless events come from the dongle device (0). In SW mode, each (remappable) event comes from the appropriate child device." [V] `CKB/keymap.c:776-779`.

### 11.3 Key-assignment table (resource 0x02, 123 bytes, index = bitmap index)
- Built by OLH: fill all 123 bytes with `0x01`; for each key `buf[keyData[0]] = keyData[1]` if the key keeps its default, else `keyData[2]`; for keys bound to a modifier chord `buf = modifierShift + 2` (Shift 25→0x1B, Alt 41→0x2B, Ctrl 49→0x33); `retainOriginal` keeps `keyData[1]`. [V] `k65plusW.go:352-408`
- Values in the K65 PLUS JSON: native `0x39` / remapped `0x38` (most keys); `0x3B`/`0x3A` (F1-F4, F7-F12, O, P, [, ], ., /); `0x3F`/`0x3E` (modifiers, Fn). [V] dump of `k65plus.json`
- Interpretation: **bit 0 = emit native HID usage**; clearing it suppresses the key at firmware level so only the bitmap reports it. Bits 1..2 seem to mark Fn-layer / modifier class. [I] Writing this table with all-native values is the safe default after entering SW mode.
- Written by: `0d 01 02 00` → `06 01 7b 00 00 00 <57 B>` → `07 01 <61 B>` → `07 01 <5 B>` → `05 01 01`. [V] `k65plusW.go:2359-2391`

---

## 12. Multimedia dial (K65 PLUS)

- Hardware mode: acts as a volume knob natively (consumer volume up/down, press = mute) [I, product behaviour; not in source].
- Software mode: rotation → interface-2 `[1]=0x05`, `[2]=0x60`, `[4..7]` int32 (±1 per detent); press → bitmap bit 137. [C] §11.
- Not reconfigurable in firmware; OLH implements 7 host-side functions: 1 volume (press = mute), 2 keyboard brightness ±100 (press = toggle 0/1000), 3 scroll (press = Ctrl+End), 4 zoom (press = zoom reset), 5 screen brightness (press = nothing), 6 media next/prev (press = play/pause), 7 horizontal scroll (press = reset). [V] `k65plusW.go:186-194,2065-2101,2441-2521`. (The K100 RGB adds dial-ring LEDs 182..189 coloured per function, `k100.go:1482-1509`, `:2401-2403`.)

---

## 13. Shutdown / restore hardware mode (OLH)

- Wireless keyboard `StopInternal` (`k65plusW.go:290-326`): stop animation timers → `0d 01 60 6d` + 500 ms + `09 01` → **rewrite onboard effect to Watercolor** (`22 00 03 04`, data `[2]=01`, `[3..6]=ff ff ff ff`) → `05 01 01` → `[09] 01 03 00 01` (HW mode). [V]
- Dongle `Stop` (`k65plusWdongle.go:226-257`): stop 10 s monitor → keyboard `StopInternal` → `[08] 01 03 00 01` → close HID. [V]
- Wired `Stop` (`k65plusWU.go:311-343`): stop queue/timers → `[08] 01 03 00 01` → close. (Direct-lighting handle 0 is not closed; switching to HW mode makes the firmware resume its onboard effect.) [V]/[I]
- Global `devices.Stop()` calls each device's `Stop` (`OLH/src/devices/devices.go:189-201`).
- Unplug ("dirty") path just stops timers, no I/O (`k65plusW.go:329-349`, `k65plusWdongle.go:200-224`). [V]

## 14. OLH resume / sleep / reconnect behaviour (K65 PLUS)

- **Keyboard sleeps / turns off / out of range**: dongle sends `01 36 .. 00` → `SetConnected(false)`: stop animation, mark offline, sleep 1 s. No I/O. [V] `k65plusWdongle.go:445-454,488-492`, `k65plusW.go:424-434`
- **Keyboard wakes / reconnects**: `01 36 .. 02` → wait **5 s** (`connectDelay`) → `Connect()` (full re-init §8.1: ping up to 10×1 s, SW mode, battery, FW, effect, brightness, sleep timer, lock + key table, charge). [V] `k65plusWdongle.go:70,474-485,493-495`. (Re-init is required because the keyboard comes back in HW mode. [I])
- Keepalive failures (`[09] 12`) are only logged; they do not mark the keyboard offline. [V] `k65plusWdongle.go:514-521`
- **Host suspend**: OLH listens for logind `PrepareForSleep(true)` → `devices.Stop()` (→ restore HW mode as §13); on resume it waits `ResumeDelay` and then **exits the process (code 1) so systemd restarts it** ("If you're reading this and thinking a resume should work, good luck"). [V] `OLH/src/monitor/monitor.go:75-110`
- **USB hotplug**: netlink uevent "add" → sleep 7 s (+2 s for some PIDs) → re-init; "remove" → StopDirty. [V] `monitor.go:162-240`

---

## 15. OLH feature checklist for the K65 PLUS family (device methods → protocol)

W = wireless/dongle package `k65plusW`, U = wired `k65plusWU`. Host-only = no device I/O.

| Feature / method | W | U | Protocol operation |
|---|---|---|---|
| Connect / Init | ✓ | ✓ | §8: SW mode 0x03=2, GET 0x0F/0x10/0x13, open lighting handle, brightness, (W) sleep 0x0E, lock 0x4A/0x45, key table res 0x02 |
| Stop / StopInternal / StopDirty | ✓ | ✓ | §13 (W rewrites watercolor + HW mode; U HW mode) |
| SetConnected / ModifyBatteryLevel | ✓ | – | driven by 0x36 / battery notifications |
| UpdateRgbProfile (choose effect) | ✓ 11 HW effects | ✓ 19 software effects | W: effect record → res 0x6D60 (§9.2). U: 20 ms frames → res 0x22 (§9.1) |
| UpdateRgbProfileData / ProcessNew/DeleteGradientColor | ✓ | ✓ | re-run setDeviceColor (W: only speed used; U: full) |
| UpdateDeviceColor | ✓ (whole-board static colour only, keyOption 2) | ✓ per-key / per-row / all / selection (keyOption 0-3) | W: STATIC effect BGR; U: triplet frame |
| ControlDeviceRgb (RGB off schedule) | ✓ | ✓ | W: STATIC 0,0,0; U: all-zero frame |
| ChangeDeviceBrightness (4 presets) | ✓ | ✓ | W: SET 0x02 = 1000 (mode 4 → 0); U: software scaling of frames |
| Fn+F5/F6, dial-mode 2 brightness | ✓ | ✓ | SET 0x02 ±100 in 0..1000 |
| UpdateSleepTimer (5/10/15/30/60 min) | ✓ | – | SET 0x0E u32 ms |
| ProcessGet/SetKeyboardPerformance (Win, Alt+Tab, Alt+F4, Shift+Tab) | ✓ | ✓ | SET 0x4A mask, SET 0x45 enable; Fn+L toggles 0x45 |
| UpdateDeviceKeyAssignment / ProcessGetKeyAssignmentTypes/Modifiers / ProcessGetKeyboardKey | ✓ | ✓ | res 0x02 table + host-side action on bitmap (types: media, keyboard, sniper, mouse, macro, profile change/switch, global profile) |
| TriggerKeyAssignment (macros, remaps, Fn media) | ✓ | ✓ (backendListener) | consume iface-2 `[1]=02` bitmap |
| UpdateControlDial (7 modes) | ✓ | ✓ | consume iface-2 `[1]=05` + bit 137 |
| FN colour-off overlay | – | ✓ | re-render frame when bit 122 changes |
| ChangeKeyboardLayout (US/UK/ISO JSON) | ✓ | ✓ | host-only (then re-sends lighting + key table) |
| Save/Change/Delete device & keyboard profiles, UpdateDeviceLabel, profile rotate | ✓ | ✓ | host-only + re-send colour/perf/key table |
| Battery level / charge status display | ✓ | ✓ | GET 0x0F, 0x10; notifications |
| Firmware display (keyboard + dongle) | ✓ | ✓ | GET 0x13 (DEV 0x09 and 0x08) |
| ProcessSetRgbCluster / ProcessSetOpenRgbIntegration (OpenRGB SDK server) | – | ✓ | external frames → res 0x22 |
| CPU/GPU temperature effects | – (polled but unused) | ✓ | software frames |

Source: method list `grep '^func'` of `k65plusW.go` and `k65plusWU.go` (§ line refs above). [V]

---

## 16. K100 notes (RGB vs AIR)

| | K100 RGB (wired only) | K100 AIR WIRELESS |
|---|---|---|
| PIDs | 0x1B7C / 0x1B7D / 0x1BC5 | wired 0x1BAB; wireless via SLIPSTREAM, child DEV 0x09 |
| Report size | **1024-byte jumbo** (write 1025 incl. report id; first chunk 1017 data, continuations 1021) [C] `k100.go` vars, `CKB/usb.h:316` | 64 bytes [V] `k100airW.go` vars |
| SW mode value | 0x03 = 2 [V] | 0x03 = **6** [V] `k100airW.go` (`01 03 00 06`) |
| Lighting | Direct per-key: open **handle 1** → res 0x22 (`0d 01 22`), `12 00` + 193 triplets (581 B incl. 2 spare); OLH continuation uses `07 00`; ORGB pins K100 to 0x22 and paces 2 ms; ckb-next falls back from 0x01 (returns 0x01/0x06) to 0x22 + res 0x2E on handle 2 [C] `k100.go`, `ORGB/...Controller.cpp:114-124`, `CKB/device_bragi.c:93-134`; 193 LEDs `CKB/led_bragi.c:44-46` | Onboard effects like K65 PLUS wireless: open h1 → res 0x6D65, probe, write record **with LED list**, close, `15 01` flush; init writes res 0x13 and `0b 65 6d`; top bar via handle 0 res 0x2E header `2b 00` [V] `k100airW.go:609-646,760-799,2640-2710` |
| Extras | brightness also prop 0x6D; polling 0x01 (1..7 up to 8 kHz); debounce 0x09/0x0A + 0x39/0x38; dial ring LEDs 182-189; lock LED 114 (offset 342) [V] `k100.go` | sleep enable 0x0D + 0x0E; auto-brightness 0xC4/0xC5; connection mode 0x3A (Fn+BT1/2/3/SLIPSTREAM handled by host in SW mode) [V] |
| Key table | res 0x02 on handle 2 (`0d 02 02`, `06 02`), 138 B | res 0x02 on handle 1, 135 B |
| Layout JSON | `OLH/database/keyboard/k100*.json` | `k100air*.json` |

Distinguish at runtime by PID (wired) or by GET 0x12 on DEV 0x09 (via receiver). [C]

---

## 17. OLH quirks to avoid (and open questions)

1. No status-byte checks anywhere on the K65 PLUS path; handle 1 left open after effect writes, then re-opened for the key table (likely status 0x03, ignored). Close before open. [V] `k65plusW.go:1621-1904,2359-2391`
2. Keyboard (`k65plusW.transfer`) and dongle (`k65plusWdongle.transfer`) share one HID handle but use **different mutexes**, so the 10 s monitor can interleave with keyboard commands and steal responses. Use one I/O queue per hidraw node. [V] `k65plusW.go:2393-2396`, `k65plusWdongle.go:643-646`
3. Wired listener battery test uses `data[1] == 0x01 || data[2] == 0x0f`; the `data[19]==0x02` mute branch is unreachable. [V] `k65plusWU.go:2955,2980-2983`
4. Effect-record TOTAL_LEN excludes the header bytes (§9.2). [V]
5. Two incompatible parsers for resource 0x24 (K65 dongle vs generic SLIPSTREAM). [V] §5

Open questions to settle with read-only probes first (GETs are side-effect free; do them only once the user OKs touching hidraw):
- Does the K65 PLUS accept direct per-key frames (res 0x22 or 0x01) over SLIPSTREAM, or only effect records? [I]
- Response echo byte `[0]` value for child (0x09 vs 0x01). [I]
- GET 0x41 (layout), 0x01 (polling), 0x15/0x16 (radio FW) on K65 PLUS. [I]
- Exact semantics of key-table bits 1..2. [I]
- SW-mode timeout duration on K65 PLUS. [I]
