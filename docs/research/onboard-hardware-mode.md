# Onboard / hardware mode vs software mode — Corsair K65 PLUS Wireless and K100 (RGB + AIR)

Research date: 2026-11-07. Scope: what persists on the keyboard with no host software running, and what needs a live host process. This decides the hyprdeck plugin architecture: requests are short-lived, with a 15 s deadline and no daemons (hyprdeck `docs/PLUGINS.md:142`).

## Sources and how claims are tagged

| Abbrev | Source | Revision |
|---|---|---|
| OLH | OpenLinkHub (GPL-3.0), `scratchpad/src/OpenLinkHub` | `6fc84cbacd068d1adbef94fdd621e766391c0c97` (2026-11-07) |
| ORGB | OpenRGB `Controllers/CorsairPeripheralV2Controller` (GPL-2.0+), sparse clone at `scratchpad/src/OpenRGB` | `029758e2260208c6de06eefc7106cd5c462c2310` (2026-11-07) |
| CKB | ckb-next (GPL-2.0), `scratchpad/src/ckb-next` | `e22eaf2fba7f93764b4c6d4413bb83eeca75d10a` (2026-10-21) |
| MAN-K65 | Corsair K65 PLUS WIRELESS online manual, corsair.com/us/en/explorer/gamer/keyboards/k65-plus-wireless/ | fetched 2026-11-07 |
| FORUM-K65 | forum.corsair.com topic 188415, "K65 PLUS - NO PER KEY RGB in Wireless Mode" (includes a quoted Corsair support reply, Sep 2024) | |
| FORUM-K100AIR | forum.corsair.com topic 192543 (K100 Air device memory) | |
| CKB#523 | github.com/ckb-next/ckb-next/issues/523 | |
| RAVEN | github.com/Ravenslofty/corsair-re README (legacy, pre-Bragi protocol RE) | |
| BITTECH | bit-tech K100 RGB review (8 MB onboard storage, 200 profiles) | |

Every claim carries one of three tags:
- **[V]** VERIFIED-IN-SOURCE, with a file:line citation.
- **[C]** CORROBORATED by two independent sources.
- **[I]** INFERRED. This is my reasoning and must be checked on hardware.

No GPL code is reproduced here. Protocol facts are described in prose and byte tables.

---

## 0. Executive answer

1. **Proven to persist with no host software (firmware features):**
   - K65 PLUS W has 4 onboard profiles, cycled with FN+Z. Their factory lighting is 1 = white Watercolor, 2 = red Spiral Rainbow, 3 = green Rain, 4 = blue Rainbow Wave.
   - FN+1…FN+0 select 10 built-in hardware effects: Watercolor, Spiral Rainbow, Rain, Rainbow Wave, Type-Lighting Key, Type-Lighting Ripple, Color Shift, Color Pulse, Color Wave, and Static (8 colours that cycle).
   - Other FN functions: FN+F5/F6 brightness, FN+L Win Lock, FN+M on-the-fly macro recording, and factory reset (hold FN + backlight button for 5 s). (MAN-K65) **[V-doc]**
   - K100 RGB: onboard storage of 8 MB for up to 200 hardware profiles with layered lighting, programmable only through iCUE "Device Memory Mode". (BITTECH + FORUM-K100AIR) **[C]**
2. **No open-source project can program onboard profiles, hardware key maps or hardware macros on any Bragi keyboard.**
   - ORGB's hardware-effect writer is commented out and `DeviceUpdateMode` is empty (ORGB `CorsairPeripheralV2Controller.cpp:462-510`, `RGBController_CorsairV2Hardware.cpp:233-236`) **[V]**.
   - ckb-next has no Bragi hardware-save path, and its maintainers say the format "has not been reverse engineered" (CKB#523) **[C]**.
   - iCUE itself does not offer hardware-lighting saves for the K65 PLUS (FORUM-K65; Corsair support: no per-key lighting, a bandwidth restriction) **[C]**.
3. **What OLH does on the K65 PLUS W and K100 AIR W over SLIPSTREAM.** It writes a *built-in effect descriptor*: effect id, colour mode, speed, direction, colour list and LED list. The target is a file-like resource: **`0x6d60` on the K65 PLUS W** (`k65plusW.go:125,696-708,1621-1904,1923-1946`) and **`0x6d65` on the K100 AIR W** (`k100airW.go:118-145,760-798,2640-2690`). It does this while the device is in software mode.
   - The K100 AIR path ends with a `0x15 0x01` command that OLH names "flush". The K65 path does not send it.
   - The effect ids match ORGB's hardware-mode enum byte for byte, so this is the firmware's own built-in effect engine **[C]**.
   - **Unproven: whether this descriptor persists after the host releases the device and across a power cycle.**
     - Indirect evidence that it at least survives the switch back to hardware mode: on shutdown, OLH deliberately rewrites the factory "white Watercolor" descriptor before switching to hardware mode (`k65plusW.go:290-326`). That would be pointless if the descriptor vanished on release **[I]**.
     - Power-cycle persistence is unknown.
4. **Properties.** Brightness, sleep timeout, Win-Lock and polling are plain SET-property writes. OLH re-applies all of them on every connect (`k65plusW.go:459-477`), so OLH gives no evidence that they persist. Persistence across power cycles is **unknown [I]** and must be probed (§7).
5. **Software-mode features need a live process.** These are per-key live lighting, host-handled remaps and macros, and the dial / G-key actions OLH synthesises.
   - The key table OLH writes only marks keys as "intercept and report to host". The host then synthesises the action through uinput (`k65plusW.go:352-396,2426-…`) **[V]**.
   - Every host implementation sends a keepalive while in software mode: OLH every 10 s (dongle) or 20 s (wired); CKB every 50 s; ORGB re-sends LEDs after 30 s. ORGB documents a K55 RGB PRO that reverts lighting if it gets no packet within 1 min **[C]**.
6. **Recommendation.**
   - The **stateless plugin** does read-only state, sets properties, and selects a built-in hardware effect (K65/K100 AIR, *after* the probe in §7 confirms it persists). Each request ends by restoring hardware mode.
   - An **optional, user-enabled `systemd --user` companion** owns software mode: per-key frames, remaps, macros, layers and keepalive. It always hands the device back to hardware mode on exit.
   - Both coordinate through a per-device `flock` plus a unix socket (§8).

---

## 1. Bragi transport primitives (shared by all three keyboards)

Frame on hidraw, interface 1 (vendor usage page `0xFF42`, usage 1, 64-byte IN/OUT reports):
- `[0x00 report-id][dest][cmd][args…]`, zero-padded to 65 bytes.
- `dest` = `0x08` for a directly connected device or the dongle itself.
- `dest` = `0x09` for the keyboard reached through the K65 PLUS dongle. OLH sets `Endpoint: 0x09` at `k65plusWdongle.go:297` **[V]**; ORGB uses write id 8 wired / 9 wireless, `CorsairPeripheralV2Controller.h` `CORSAIR_V2_WRITE_WIRED_ID/…WIRELESS_ID` **[C]**.
- In OLH the header is built by `transfer()`: byte 1 = endpoint, then command bytes (`k65plusW.go:2393-2417`) **[V]**.
- The reply is a 64-byte IN report. Byte 2 is the status: 0 = OK, 1 = invalid value, 3 = failed / handle busy, 5 = unsupported. (CKB `bragi_common.c:6-35,179-196`; ORGB `GetErrorString`) **[C]**

| cmd | meaning | args | evidence |
|---|---|---|---|
| `0x01` | SET property | `prop, 0x00, value LE (u16, or u32 for sleep)` | CKB `bragi_proto.h` `BRAGI_SET`; CKB `bragi_common.c:23-35`; OLH everywhere **[C]** |
| `0x02` | GET property | `prop, 0x00`; reply value at bytes 3..6 LE | CKB `bragi_common.c:6-19`; ORGB `GetAddress` **[C]** |
| `0x05` | close handle | `0x01, handle` | CKB `bragi_common.c:169-177`; OLH `cmdCloseEndpoint={05,01,01}` **[C]** |
| `0x06` | write (first chunk) | `handle, u32 LE total length, data…` | CKB `bragi_common.c:60-102`; OLH writes u16 len + 2 zero bytes, `k65plusW.go:1923-1946` **[C]** |
| `0x07` | write (continuation) | `handle, data…` (61 bytes/packet on 64-byte reports) | same **[C]** |
| `0x08` | read chunk | `handle`; data at reply byte 3 onward | CKB `bragi_common.c:124-167` **[V]** |
| `0x09` | probe handle (returns size) | `handle`; size u32 LE at reply bytes 5..8 | CKB `bragi_common.c:109-122`; OLH calls `{09,01}` right after opening `0x6d60` / `0x6d65` (`k65plusW.go:703`, `k100airW.go:762-766`) **[C]** |
| `0x0d` | open handle | `handle, resource u16 LE` | CKB `bragi_common.c:179-196`; ORGB `StartTransaction` **[C]** |
| `0x12` | ping / keepalive | — | CKB `device_bragi.c:15-35`; OLH `cmdHeartbeat` **[C]** |
| `0x0b` | "init protocol". Probably *create file/resource* | resource u16 LE (`0x6d65`) | OLH `k100airW.go:122,790` only; meaning **[I]** |
| `0x15` | "flush". Probably *commit to flash* | `0x01` | OLH `k100airW.go:124,2635,2686`; also used by makr75W, k57rgbW, k70coretklW, k70pmW, vanguard96W, vanguard99airW. Meaning **[I]** |
| `0x24` | dongle: list paired devices | — | OLH `slipstream.go:96`, `k65plusWdongle.go:59` **[V]** |

Mode property `0x03`:
- `1` = hardware, `2` = software (CKB `bragi_proto.h` `BRAGI_MODE_*`; ORGB `CorsairPeripheralV2Devices.h:19-23`) **[C]**.
- OLH uses value **`6`** as "software mode" for the K100 AIR W, MAKR75 W, K70 CORE TKL W, K70 PRO MINI W and K57 W (`k100airW.go:118`, `makr75W.go:118`) **[V]**. The meaning of 6 is unknown **[I]**; it may be "software mode, firmware renders lighting".
- The K65 PLUS W uses `2` (`k65plusW.go:123`) **[V]**.

Handle conventions:
- CKB uses handle 0 for lighting, 1 as a short-lived generic handle ("Do NOT leave it open"), and 2 for secondary lighting (`bragi_proto.h`) **[V]**.
- OLH on the K65 PLUS W uses handle 1 for both the effect file and the key table (`k65plusW.go:125,136`) **[V]**.
- OLH on the K100 RGB uses handle 1 for per-key lighting and handle 2 for the key table (`k100.go:130,150`) **[V]**.
- If an open returns status 3, CKB closes the handle and retries (`bragi_common.c:183-191`) **[V]**. This matters when a previous process died with a handle open.

Reading handles may require software mode. CKB `device_bragi.c:152-162` says "Check if we're in HW mode, and if so, switch to software in order to read the properties/handles", then switches back to hardware at `:234-235` **[V]**. GET of property `0x03` itself works in hardware mode (`:153`) **[V]**.

---

## 2. K65 PLUS Wireless (dongle 1b1c:2b07 → keyboard)

### 2.1 What the firmware does on its own (hardware mode)

From MAN-K65 **[V-doc]**:
- "When iCUE is not running … the keyboard operates in hardware mode which uses profiles and settings saved to the onboard storage."
- **FN+Z** cycles **4 onboard profiles**. The MPI LED blinks the profile colour: 1 White = Water Color, 2 Red = Spiral Rainbow, 3 Green = Rain, 4 Blue = Rainbow Wave.
- **FN+1…0** select hardware effects:

  | Key | Effect |
  |---|---|
  | FN+1 | Water Color |
  | FN+2 | Spiral Rainbow |
  | FN+3 | Rain |
  | FN+4 | Rainbow Wave |
  | FN+5 | Type-Lighting per-key |
  | FN+6 | Type-Lighting Ripple |
  | FN+7 | Color Shift |
  | FN+8 | Color Pulse |
  | FN+9 | Color Wave |
  | FN+0 | Static (8 colours cycle) |

- FN+F5/F6 change backlight brightness. FN+Left sets the dial to brightness mode. FN+L toggles Win Lock.
- **FN+M records an on-the-fly macro** (indicator blinks red while recording). Hardware macros therefore exist, but only as a user gesture on the keyboard.
- **Factory reset**: hold FN + backlight button (G) for 5 s; the MPI blinks red and the keyboard restarts with factory defaults. This is the documented recovery path. No host software is needed.

What iCUE exposes:
- iCUE shows only "Wireless Lighting" for the K65 PLUS: no per-key control and no "save to hardware" option.
- Corsair support (quoted in FORUM-K65): the K65 Plus "does not support individual key lighting", due to bandwidth.
- A user in the same thread reports that in Wireless Lighting the effect visibly restarts about once a minute. That suggests iCUE periodically re-sends the effect or a keepalive **[I]**.
- A Reddit post (thread 1d0xe4t, only the search excerpt was readable) says iCUE has no hardware lighting for the K65 PLUS, so Device Memory Mode cannot set it.
- **Conclusion [C]:** onboard-profile lighting on the K65 PLUS is not host-programmable through any known official path. Only firmware defaults plus FN shortcuts change it.

### 2.2 What OLH writes (all in software mode 2)

Connect sequence (`k65plusW.go:459-477`) **[V]**:
1. Ping `0x12`, retried up to 10× with 1 s between tries (`:436-457`).
2. SET mode = 2: `01 03 00 02`.
3. GET battery `02 0f` (u16 at reply bytes 3..4, ÷10 = %).
4. GET firmware `02 13` (bytes 3,4 and u16 at 5..6).
5. `initLeds`: open handle 1 on resource `0x6d60` (`0d 01 60 6d`), sleep 500 ms, then probe handle 1 (`09 01`) (`:696-708`).
6. Write the effect descriptor (§2.3) with `06 01` + `07 01` chunks, then close `05 01 01` (`:1902-1905`). No flush.
7. Brightness SET `01 02 00 <u16 LE 0..1000>` (`:1907-1920`).
8. Sleep SET `01 0e 00 <u32 LE milliseconds>`, minutes × 60000 (`:946-960`).
9. Performance (§2.5).
10. GET charge state `02 10`.

Shutdown (`StopInternal`, `k65plusW.go:290-326`) **[V]**:
1. `initLeds` again.
2. Write the **default descriptor `22 00 03 04` (Watercolor, mode 3, medium speed) with colour ff ff ff ff**. This matches onboard profile 1's factory "white Watercolor".
3. Close handle 1.
4. SET mode = 1 (hardware).

**[I]** Writing a factory-like default before releasing the device only makes sense if the descriptor in `0x6d60` stays visible after the switch to hardware mode. So `0x6d60` is likely the active / profile-1 lighting slot, not a volatile software-mode buffer. It remains unproven whether it survives a power cycle, and whether FN+Z / FN+1..0 overwrite it.

### 2.3 Effect descriptor format (common to K65 PLUS W, K100 AIR W and the other OLH "W" keyboards)

Layout of the bytes written into the resource, after the `06 <h> <len>` header. Reconstructed from OLH `k100airW.go:1754-2420` (full form) and `k65plusW.go:1621-1904` (K65 form) **[V]** for each field; the unified layout is my synthesis **[I]**.

| off | size | field | values |
|---|---|---|---|
| 0 | 2 | effect id (LE) | see table below |
| 2 | 1 | colour mode | `0x00` none (rainbow-type effects), `0x01` custom colours, `0x02` random colours, `0x03` default/white (Watercolor, `k65plusW.go:315,1894`) |
| 3 | 1 | speed | `0x03` slow, `0x04` medium, `0x05` fast (`k65plusW.go:1680-1690` etc.) |
| 4 | 1 | direction (directional effects only; else 0) | 1..5 accepted by OLH for Color Wave (`k100airW.go:2224-2227`); Rainbow Wave and Spiral are sent as `0x04` / `0x06` |
| 5 | 1 | 0x00 | padding / reserved |
| 6 | 1 | colour count N | 0, 1, 2, 3 |
| 7 | 4·N | colours | each `0xFF`, B, G, R. The `0xFF` is a "marker"; probably per-colour brightness/alpha **[I]** |
| 7+4N | 1 | LED count K | the K100 AIR sends its key count. The K65 PLUS sends 0 (buffers are zero-filled, 89/93 bytes) |
| 8+4N | K | LED indices | layout `packetIndex/3` per key (`k100airW.go:1849-1860`) |

Notes on the table:
- For effects whose OLH header is only 4 bytes (Static, Watercolor, Color Pulse, Color Shift, Type-Key, Type-Ripple), the direction and padding bytes sit at the start of the data buffer. The layout is consistent in every case I checked: Static `k100airW.go:1838-1849`; Color Pulse `:2083-2117`; Watercolor `:2370-2405`; Color Wave `:2202-2245`; Visor `:2308-2345` **[V]**.
- **Length quirk [V]:** OLH writes `len(data)`, which excludes the 3–5 effect-header bytes, as the length field (`k65plusW.go:1927`). CKB writes the true length. So the firmware either tolerates a short length or silently drops the last 3–5 bytes (zeros or LED indices). Our implementation should send the true length. Verify on hardware.

Effect ids (LE u16). The OLH bytes match the ORGB enum (`CorsairPeripheralV2Controller.h:64-78`) **[C]**:

| effect | id | OLH bytes | ORGB name | on K65 FN key |
|---|---|---|---|---|
| Direct (software frame) | 0x0012 | `12 00` | DIRECT | — |
| Static | 0x207E | `7e 20` | STATIC | FN+0 |
| Color Pulse | 0xAD4F | `4f ad` | FLASHING | FN+8 |
| Color Shift | 0xA5FA | `fa a5` | BREATHING | FN+7 |
| Color Wave | 0x7BFF | `ff 7b` | SPECTRUM | FN+9 |
| Rainbow Wave | 0xB94C | `4c b9` | RAINBOW | FN+4 |
| Rain | 0xA07E | `7e a0` | RAIN | FN+3 |
| Spiral Rainbow | 0xAB87 | `87 ab` | SPIRAL | FN+2 |
| Watercolor | 0x0022 | `22 00` | WATERCOLOR | FN+1 |
| Type Lighting – Key | 0xB1F9 | `f9 b1` | REACTIVE | FN+5 |
| Type Lighting – Ripple | 0x09A2 | `a2 09` | RIPPLE | FN+6 |
| Visor | 0x90C0 | `c0 90` | VISOR | not on K65 (K100 AIR, `k100airW.go:2308`) |
| Gradient (custom table) | 0xEBF4 | written via a 512-byte colour table first (`k100airW.go:2440-2560,2570-2638`) | — | K100 AIR only **[I]** |

The ten FN-key effects in MAN-K65 are exactly OLH's K65 `rgbModes` list (`k65plusW.go:153-165`) **[C]**.

K65 PLUS specifics in OLH (`k65plusW.go:1621-1904`) **[V]**:
- No custom colours except Static.
- Static = `7e 20 01` followed by data where byte 3 = 1 (colour count), then `ff B G R`.
- Off = Static with black.
- Rain random = `7e a0 02 <speed> 01`.
- Rainbow Wave = `4c b9 00 <speed> 04`.
- Spiral = `87 ab 00 <speed> 06`.
- Color Wave = `ff 7b 02 <speed> 04`.
- Color Pulse / Shift / Type-Key / Ripple: `<id> 02 <speed>`.
- So per-key static colours are impossible on the K65 PLUS W (FORUM-K65 **[C]**). Over the wired USB connection (k65plusWU) OLH does per-key direct frames via resource `0x22`, but only in software mode (`k65plusWU.go:133-139`) **[V]**.

### 2.4 Key table: remap is software-mode only

Write sequence (`k65plusW.go:2359-2390`) **[V]**:
1. Open handle 1 on resource `0x02` (`0d 01 02`).
2. Write a **123-byte** table (`KeyAssignment = 123`, `:150`) with `06 01` / `07 01`.
3. Close.

Table contents (`:352-396`) **[V]**:
- Index = key's `keyData[0]` (HID usage-like: ESC = 41, F1 = 58, C = 6).
- Value = `keyData[1]` (default action) or `keyData[2]` (the remapped action). Unused bytes are `0x01`.
- Example from `database/keyboard/k65plus.json`: most keys have `[usage, 57, 56]`. F1–F4 and F7 have `[usage, 59, 58]`.
- For modifier keys OLH writes `shift+2`.

How remaps work **[V]**:
- Remapping sets a key to the "56/58" variant. The firmware then reports that key on the vendor input interface (report `data[1]==0x02`).
- OLH's `TriggerKeyAssignment` (`:2426-…`) synthesises the configured action (media, keyboard, mouse, macro, profile switch) through uinput.
- The dial arrives as `data[1]==0x05`, with value 1 or 255.

**[I]** The values look like "pass to HID" (0x39/0x3B) versus "intercept to host" (0x38/0x3A). So this table does not encode the target key. It only diverts keys to the host. Remaps are therefore impossible without a live process. A diverted key with no listener becomes a **dead key**: this is the main hazard of a crashed companion (§8). Whether the table survives a mode switch or power cycle is unknown **[I]**.

### 2.5 Win-lock / "performance" (`k65plusW.go:591-640`) **[V]**

- SET `01 4a 00 <bitmask>`:
  - bit0 = disable Win key
  - bit1 = disable Alt+Tab
  - bit2 = disable Alt+F4
  - bit3 = disable Shift+Tab
- Then SET `01 45 00 <0|1>`: the performance / lock enable flag, also shown by the lock LED.
- Then OLH re-writes the key table.
- The firmware has its own FN+L Win Lock (MAN-K65). These properties are almost certainly the same firmware state **[I]**.

---

## 3. K100 AIR Wireless (slipstream path, `k100airW`)

Constants (`k100airW.go:115-145`) **[V]**:
- Software mode `01 03 00 06`. Hardware mode `01 03 00 01`.
- Brightness `01 02 00`. Sleep `01 0e 00` (u32 ms), preceded by `01 0d 00 01`.
- Auto-brightness properties `0xc4` / `0xc5`.
- Connection mode property `0x3a` (Bluetooth `01 3a 00 02`; USB: `02 3a 00 00` then `01 3a 00 00`).
- Performance `01 4a 00`. Key table on resource `0x02`, handle 1, **135 bytes** (`:156`).

`initLeds` (`k100airW.go:760-798`) **[V]**:
1. Open handle 1 on resource **`0x13`** (`0d 01 13`), then probe `09 01`.
2. Write an 8-byte record `69 6c 01 00 08 00 65 6d` with no effect header.
3. Close handle 1.
4. Send **`0b 65 6d`**.
5. Open handle 0 on resource `0x2e` (top-bar LEDs), then sleep 500 ms.

Interpretation [I]: resource `0x13` is a directory/index naming the lighting file `0x6d65`, and `0x0b` creates (or allocates) it. **Do not replay this** until reads in §7 show what is already there.

`writeColor` (`k100airW.go:2640-2690`) **[V]**:
1. Open handle 1 on `0x6d65`, then probe `09 01`.
2. Write the descriptor (§2.3) with `06 01` / `07 01`.
3. Close `05 01 01`.
4. **Send `15 01` ("flush")**.

The MAKR75 W uses the same pattern on `0x6d67` (`makr75W.go:2364-2412`) **[V]**. **[I]:** `0x15` likely commits the file to non-volatile storage. If so, every OLH colour change on these boards was a flash write.

Top bar (`k100airW.go:609-645,2692-2710`) **[V]**: handle 0 / resource `0x2e`, data type `2b 00`, 40-byte buffer with per-icon records (lock icon, connection status). It is software-only and rendered while the host runs **[I]**.

Shutdown (`k100airW.go:287-295`) **[V]**: SET hardware mode only. It does **not** reset the effect. Contrast with the K65 PLUS, which does.

Device memory per FORUM-K100AIR **[C with BITTECH]**: hardware profiles are created in iCUE "Device Memory Mode" over USB, and the hardware-mode preset list is shorter than the software list. No format is published anywhere.

## 4. K100 RGB (wired, PIDs 0x1b7c / 0x1b7d / 0x1bc5)

- OLH (`k100.go:126-160`) **[V]**:
  - Software mode 2.
  - Per-key frames on handle 1, resource **`0x22`** (`0d 01 22`), data type `12 00` (DIRECT), 1024-byte reports.
  - Key table on handle 2, resource `0x02`, 138 bytes.
  - Brightness is written to **both** `0x02` and `0x6d` (`:131-134,1908-1922`).
  - Polling SET `01 01 00 <rate>` (`:1012-1040`). CKB maps 1 = 8 ms, 2 = 4 ms, 3 = 2 ms, 4 = 1 ms (`bragi_proto.h`); the K100 also supports higher rates.
  - Debounce: enable `01 09 00 01` / `01 39 00 01`, value `01 0a 00` / `01 38 00` = 30 + n (n in 1..9) (`:1925-1982`).
  - **Keepalive `0x12` every 20 s** (`:150-152,895-922`).
  - Stop = hardware mode, then close (`:332-365`).
- OLH has **no hardware-effect path for the K100 RGB** **[V]**.
- ORGB pins the K100 to lighting resource `0x22`, uses Direct mode only, and does not implement hardware effects (`CorsairPeripheralV2Controller.cpp:117-131,462-510`) **[V]**.
- CKB does Direct lighting only, with a 50 s poll **[V]**.
- Onboard storage: 8 MB, up to 200 profiles with up to 20 lighting layers, programmable only by iCUE (BITTECH) **[C with FORUM-K100AIR]**. **No open-source writer exists (CKB#523) [C].**
- So for the K100 RGB, hardware-mode lighting is whatever iCUE last stored. A Linux host can only switch it into software mode and stream frames.

---

## 5. Answers to the scoped questions

### (a) Can hardware-mode lighting be programmed from the host?

| Capability | K65 PLUS W | K100 AIR W | K100 RGB |
|---|---|---|---|
| Built-in effect (id, speed, direction, colours) written by host | **Yes in software mode** (OLH, resource `0x6d60`) [V]. Survives switch to hardware mode: **likely [I]**. Survives power cycle: **unknown** | Yes in software mode (OLH, `0x6d65` + flush) [V]. Persistence unknown; the flush suggests a flash write [I] | No known path [V by absence in OLH/ORGB/CKB] |
| Static single colour | Yes (Static `0x207E`, 1 colour) [V] | Yes, with an LED list [V] | Only via software Direct frames |
| Per-key static colours persisted | **No.** Firmware/iCUE does not offer it wirelessly [C] | **Unclear [I].** The descriptor carries an LED list, so a "Static with a subset of keys" may be possible. Multi-layer is not in OLH | Only via iCUE onboard profiles; format not reverse-engineered [C] |
| Onboard profile slots (FN+Z) programmable | No known path; iCUE offers none [C] | iCUE Device Memory Mode (USB). No open format [C] | iCUE only [C] |
| Writes iCUE uses for "hardware lighting" | Not reverse-engineered publicly. iCUE likely uses the same open/write/close resources plus directory/create/flush commands (`0x13` index, `0x0b`, `0x15`) seen in OLH for the K100 AIR [I] | same | same |

ORGB, exactly: the **"Corsair Peripherals V2 Hardware"** controller (`RGBController_CorsairV2Hardware.cpp`) exposes "Direct" and "Static" modes. But `DeviceUpdateMode()` is empty (`:233-236`) and the hardware-mode packet builder in `UpdateHWMode` is commented out (`CorsairPeripheralV2Controller.cpp:462-510`). In practice ORGB supports **no** hardware modes on Bragi devices **[V]**. Its device list has no K65 PLUS and no K100 AIR; it does have the K100 RGB (`CorsairPeripheralV2ControllerDetect.cpp:108-110`) and the generic Slipstream receivers **[V]**. The disabled builder hints at a layout different from OLH's (write to "handle 1", 0x30 bytes, mode id at offset 8). Treat it as dead code, not evidence.

### (b) Onboard key remapping / macros

- **Firmware-native:** the K65 PLUS supports FN+M on-the-fly macro recording (MAN-K65) [V-doc]. iCUE Device Memory Mode stores "Hardware Actions" on the K100 / K100 AIR [C].
- **Host-written:** no open implementation. The only key table OLH writes (resource `0x02`) is a divert-to-host table. Its semantics require a live process (§2.4) [V].
- **Verdict:** stored remaps and macros cannot be written from Linux today. Software remaps and macros need the companion.

### (c) Which properties persist across power cycles

There is no source evidence for any of them. OLH, CKB and ORGB all re-apply settings on every connect, so none of them tells us about persistence.

| property | id | write format | persistence |
|---|---|---|---|
| brightness | `0x02` (K100 RGB also `0x6d`) | u16 0..1000 | FN+F5/F6 adjusts it in hardware mode, so it is firmware state and probably persisted with the profile [I] |
| polling rate | `0x01` | 1..4 or higher | Likely persisted (iCUE treats it as a device setting) [I] |
| sleep timeout | `0x0e` | u32 ms (K100 AIR also sets `0x0d`=1) | [I] |
| Win-lock bitmask | `0x4a` | bitmask | [I] |
| lock enable | `0x45` | 0/1 | [I]. FN+L toggles it in hardware |
| mode | `0x03` | 1/2/6 | **Volatile.** ORGB: the mode "persists until the next USB enumeration" if not reverted (`CorsairPeripheralV2Controller.cpp:139-149`) [V]. Whether a keyboard power cycle (without dongle re-enumeration) resets it is [I] |
| effect descriptor | resource `0x6d60` / `0x6d65` | file | §2.2 / §3 [I] |

The probe in §7 measures all of these.

### (d) Risk assessment

**Flash writes**
- Likely flash-touching operations [I]:
  - resource writes followed by `0x15` (K100 AIR family)
  - `0x0b` create
  - writes to directory resource `0x13`
  - very likely any property the firmware persists
- The K65 PLUS W `0x6d60` write has no flush. It may be RAM-only, or the firmware may commit on close [I].
- Endurance is unknown. Assume worst case ~10k erase cycles per sector (MCU internal flash), and ~100k for external SPI NOR such as the K100 RGB's 8 MB store [I, typical datasheet ranges].
- OLH writes the effect on **every** connect and reconnect, plus every UI change. Sleep/wake reconnects can happen dozens of times a day. Many OLH users run this for months with no wear reports. That is weak evidence of tolerance (or of no flash write at all) [I].

**Rules**
- Only write on an explicit user action.
- Read-compare-skip: read the resource or property first and skip identical writes.
- Debounce sliders: commit at most one persistent write per ~2 s and never stream.
- Keep a per-device daily write counter in `$XDG_STATE_HOME`, and refuse above e.g. 200/day without `force`.

**Brick and recovery**
- The legacy protocol shows that blind command sweeps are dangerous: `07 02 aa` put a mouse into its bootloader (RAVEN) [V-doc].
- **Never sweep command ids or property ids, and never SET unknown properties.**
- Never write resources other than those catalogued here.
- Never touch the pairing resources `0x05` (pairing id) or `0x06` (encryption key) (CKB `bragi_proto.h`). Corrupting them would at worst unpair the keyboard and dongle; recovery is FN+Delete for 3 s plus re-inserting the dongle (MAN-K65).
- Do not use `0x0b` (create) or write directory resource `0x13` on the K65 until reads show the structure.
- Avoid the connection-mode property `0x3a` on the K100 AIR; it can drop the keyboard to Bluetooth (`k100airW.go:687-708`).
- Recovery without software: factory reset on the K65 PLUS (FN + backlight button, 5 s) [V-doc]. For the K100 family the usual Corsair reset (hold ESC while connecting) is [I] and must be confirmed in its manual.
- No firmware updates (out of scope by decision).

**Software-mode hazards**
- Leaving the device in software mode with keys diverted to the host gives dead keys and dead media/dial keys.
- ORGB notes the K70 Core stops sending onboard input events (e.g. the volume knob) in software mode (`CorsairPeripheralV2Controller.cpp:141-147`) [V].
- Every code path must restore hardware mode, and restore the default key table if it was changed.

### (e) Architecture

See §8.

---

## 6. Keepalive / watchdog evidence (software mode)

| implementation | interval | what | cite |
|---|---|---|---|
| OLH K65 PLUS dongle | 10 s | ping dongle + ping each paired device + battery GET | `k65plusWdongle.go:56,500-540` [V] |
| OLH K100 RGB / K65 PLUS wired / K100 AIR wired | 20 s | `0x12` | `k100.go:150,895-922`; `k65plusWU.go:149`; `k100airWU.go:153` [V] |
| CKB (all Bragi) | 50 s | `0x12` | `device_bragi.c:15-35` [V] |
| ORGB | resend LEDs if idle 30 s, check every 12.5 s | direct frame | `CorsairPeripheralV2Controller.h:34-35`, `RGBController_CorsairV2Hardware.cpp:64-70,238-251` (K55 RGB PRO reverts after 1 min) [V] |

**[C]:** software mode has a firmware watchdog on the order of ~60 s; the exact timeout per device is unknown. Consequence: a stateless request that leaves the device in software mode will "self-heal" by reverting after the watchdog. That is not something to rely on, and it is exactly why live effects need the companion.

---

## 7. Safe empirical probing plan (run only after the user stops OLH)

Preconditions:
- `systemctl stop OpenLinkHub` (the user does this).
- `fuser /dev/hidraw*` shows nobody else holds interface 1.
- The device is identified by VID:PID 1b1c:2b07, interface 1, usage page 0xFF42 usage 1.
- Write reports as 65 bytes `[0x00, dest, cmd…]` with `dest=0x09` for the keyboard behind the dongle and `0x08` for the dongle.
- Log every TX/RX hex to `~/.local/state/hyprdeck/corsair-probe/<ts>.log`.
- One process at a time, 100–200 ms between packets, with a 500 ms read timeout.

**Phase 1: pure GETs in hardware mode (no state change).**
- Dongle (`dest 0x08`): ping `12`; GET `03` (mode); GET `13` (fw); `24` (paired devices).
- Keyboard (`dest 0x09`):
  - ping `12`
  - GET `03`, `12` (PID), `11` (VID), `13`, `14`, `41` (layout), `0f`, `10`
  - GET `02` (brightness), `0e` (sleep), `0d`, `4a`, `45`, `01` (poll)
- Record values and status bytes. A status of 5 means the property is not supported, which is useful negative data.

**Phase 2: read-only handle reads.**
1. Open handle 1 on `0x6d60` (`0d 01 60 6d`) in hardware mode, then probe `09 01` to get the size.
2. Read `08 01` until the size is exhausted. Close `05 01 01`.
3. If the open fails in hardware mode: SET mode 2, repeat, SET mode 1 (mirrors CKB `device_bragi.c:152-235`).
4. Repeat for resource `0x02` (key table, expected 123 bytes) and `0x13` (index, read-only).
5. Save the raw bytes as **backups**.
6. Optionally try `0x6d61`–`0x6d63`, one open each. An error status means "doesn't exist". Do not retry with other commands.

**Phase 3: correlation (no host writes).**
1. In hardware mode, press FN+1…FN+0, then FN+Z through profiles 1–4.
2. After each press, re-read `0x6d60` (and any `0x6d6x` that exists) plus GET `02`.
3. Decision:
   - If the bytes change and decode with §2.3, then `0x6d60` is the live hardware-profile effect, and host writes there are hardware-mode lighting.
   - If they never change, `0x6d60` is a software-mode-only "wireless lighting" slot.

**Phase 4: one reversible property write each.**
1. SET brightness `02` to 500 in hardware mode. If it is rejected, do it in mode 2, then restore mode 1. Observe.
2. Power-cycle the keyboard with its switch (dongle stays in). GET `02`. Then unplug and replug the dongle. GET `02`.
3. Repeat for `0e`, `4a`/`45` and `01`.
4. Restore the original values from Phase 1.

**Phase 5: one effect write.**
1. Mode 2. Open h1 on `0x6d60`, probe, write Static red (`7e 20 01` + data with colour count 1 and `ff 00 00 ff`, true length), close. Mode 1.
2. Observe. Power-cycle and observe. Press FN+Z ×4 and observe.
3. Then restore the Phase 2 backup bytes the same way.
4. Also try the same write **without** entering mode 2. Success there gives the simplest stateless path.
5. Never send `0x15` or `0x0b` on the K65 unless Phase 5 shows the effect is lost on power cycle. Even then, first test `0x15 0x01` on the K65 in isolation (it uses no resource argument) and observe.

**Phase 6: watchdog measurement.**
1. Enter mode 2 and do nothing.
2. Poll GET `03` every 5 s (GETs may themselves reset the watchdog, so also test pure silence followed by one GET at 30/60/120 s).
3. Record when the mode returns to 1 or the lighting reverts.

For the K100 AIR: the same plan with `0x6d65` and `0x13`, over USB or SLIPSTREAM.
For the K100 RGB: Phases 1, 4 and 6 only. There is no effect resource to test.

---

## 8. Recommended architecture

### 8.1 Stateless plugin (per hyprdeck request, < 15 s, no daemon)

Every request follows this transaction:
1. Take `flock` on `$XDG_RUNTIME_DIR/hyprdeck-corsair-<serial>.lock`. If the companion holds it, forward to the companion socket instead.
2. Open hidraw, then ping.
3. (Optional) GET mode. If it is not 1 and the companion is not running, treat it as a "stale software mode" and recover.
4. Do the operation.
5. Close any opened handle. If the mode was changed, SET mode 1.
6. Close hidraw.

Use a hard internal budget of about 5 s and reply with an error envelope rather than leaving the device mid-transaction. Use handle 1 only and never leave it open (CKB convention).

What it can do:
- **State (read-only GETs):** battery %, charging, firmware, PID / variant, layout, current mode, brightness, sleep, Win-lock, poll rate, and the current effect decoded from `0x6d60` (if Phase 2 allows reads in hardware mode).
- **Actions (only after the §7 probe proves persistence):**
  - brightness 0..1000
  - sleep timeout
  - Win-lock / Alt-Tab / Alt-F4 / Shift-Tab bitmask
  - polling rate (wired K100)
  - "Hardware effect" selection (effect id, speed, direction, up to 2–3 colours) on the K65 PLUS / K100 AIR
- All writes use read-compare-skip, a debounce and the daily cap from §5(d).
- **"Rescue" action:** close handles 0/1/2, SET mode 1, and (if known) write the default key table. This is the user's way out of a dead-key state.

### 8.2 Optional companion (`systemd --user`, enabled explicitly by the user, outside hyprdeck)

It owns software mode while running:
- per-key live lighting (wired resource `0x22` Direct frames; not possible on the K65 PLUS wireless)
- dynamic and reactive effects
- the K100 AIR top bar
- software remaps, macros and layers (key table on `0x02` + uinput)
- dial actions
- keepalive `0x12` every ≤10 s (dongle) or ≤20 s (wired)
- reconnect handling (dongle notifications)

Lifecycle:
- On start, take the flock, back up the key table, then SET mode 2 (or 6 on the K100 AIR, if probing shows a difference).
- On stop (SIGTERM / `ExecStop`), stop frames, restore the default key table and the backed-up effect, close handles, then SET mode 1. A separate `ExecStopPost=` runs the plugin's rescue command so a crash still releases the device.
- Exposes a unix socket at `$XDG_RUNTIME_DIR/hyprdeck-corsair.sock`. The stateless plugin forwards state/actions there when the lock is held, so both never interleave packets on the same handle numbers.

### 8.3 Coexistence

| situation | owner | lighting source |
|---|---|---|
| companion off | firmware (hardware mode) | onboard / FN-selected effect, or the effect the plugin wrote |
| companion on | companion (software mode) | companion frames |
| companion crashes | device stays in software mode until the watchdog fires (~60 s) or re-enumeration | `ExecStopPost` rescue restores mode 1 |

The plugin's state output should always report the mode, and who owns the device.

---

## 9. Proven vs speculative summary

**Proven [V]/[C]:**
- Bragi primitives (§1).
- Effect ids and descriptor fields (§2.3).
- OLH's software-mode effect writes to `0x6d60` (K65 PLUS) and `0x6d65` + flush (K100 AIR).
- The key table is divert-to-host only.
- Keepalive is needed in software mode; the mode is volatile.
- No open-source onboard-profile writer exists.
- The K65 PLUS firmware has 4 profiles, 10 built-in effects, FN+M macros and a factory reset.

**Speculative [I]:**
- Whether `0x6d60` / `0x6d65` contents persist into hardware mode and across power cycles.
- The meaning of `0x0b`, `0x15`, resource `0x13` and mode value 6.
- Property persistence.
- Watchdog timeouts per device.
- Flash endurance.

**Hardware-lighting programming for the K65 PLUS is unproven.** Run §7 before shipping any persistent-lighting action.
