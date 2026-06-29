# CN28 LOG protocol

Reference for the ASCII **LOG console** on the Vestel EVC04-AC11-T2P mainboard
header **CN28** — the diagnostic stream the [`evc04-cn28-prober`](../firmware/)
firmware taps and the [`evc04-cn28-core`](../core/) crate decodes
(`core/src/cn28.rs`).

This is reverse-engineered from a live box (no vendor docs); it is complete to the
lines observed so far and grows as new ones appear. Treat field meanings marked
*(inferred)* as best-effort.

## Transport

| Property | Value |
|---|---|
| Signal | 3.3 V UART (TTL) on CN28 |
| Bitrate | **9600 baud, 8N1**, no flow control |
| Wiring | the mainboard's `TX2/RX2` silk is **swapped** vs. the ESP side — on the prober GPIO16 = TX, GPIO17 = RX (see `docs/esp32-pinout.md`) |
| Framing | text lines terminated by `LF` (`\n`); some end `CR LF` (`\r\n`). The decoder strips a trailing `\r`. |
| Cadence | **request/response-ish**: the box emits nothing useful unprompted — writing any byte (the prober sends `\r\n`) opens a window in which it streams a burst of the lines below. A burst can begin or end mid-line, even mid-token, so consumers must reassemble whole lines across read windows before parsing (`core::cn28::LineReassembler`). |
| Robustness | unknown/garbled lines must be ignored, never fatal. |

Two data domains share the console:

1. **Metering** — the box's *own internal* energy meter (brand **KLEFR**, 3 CTs),
   independent of any external meter on the RS485 side. (Confirmed by pulling both
   the emulated PRO380 and the RS485 gateway — the phase data kept flowing; evc04
   issue #108.)
2. **Charge control** — the control-pilot (CP) state machine, offered current,
   load management, faults.

Units throughout: phase voltage in **millivolts**, current in **milliamps**,
power in **watts**, energy in **watt-hours**; control currents in **whole amps**;
temperature in **whole °C**. (Verified by correlation while charging at 6–16 A.)

---

## 1. Per-phase metering

```
P1:\tV: 234102\tA: 36\tW: 1\tWh: 0
P2:\tV: 232727\tA: 18\tW: 0\tWh: 0
P3:\tV: 234605\tA: 18\tW: 0\tWh: 0
```

One line per phase `P1`/`P2`/`P3`. Fields are **TAB-delimited** (`0x09`); each
`X: ` label is followed by a space then the value.

| Field | Unit | Meaning |
|---|---|---|
| `V` | mV | phase voltage (e.g. `234102` = 234.102 V) |
| `A` | mA | phase current (e.g. `6160` = 6.16 A) |
| `W` | W | phase active power |
| `Wh` | Wh | phase energy counter |

Idle reads a small noise current (~`A: 36` = 36 mA); under charge the three phases
track the draw (seen up to ~`A: 15252` / `W: 3506` per phase ≈ 11 kW at 16 A).

---

## 2. Temperature

```
Temp: 42 C 
```

Internal temperature, whole **°C**. Note the **trailing space** after `C`.

---

## 3. Charge-control currents

```
ev current: 16
max_offered_current: 16
lb current:16
TEMP lb current: 10
current_without_dlm_without_unplugged1
```

| Line | Unit | Meaning |
|---|---|---|
| `ev current: <n>` | A | current the EV is drawing / has requested |
| `max_offered_current: <n>` | A | ceiling currently offered to the EV |
| `lb current:<n>` | A | load-balancing current limit *(note: no space after the colon)* |
| `TEMP lb current: <n>` | A | load-balancing limit after thermal derating *(inferred)* |
| `current_without_dlm_without_unplugged<n>` | A | the current that would be offered ignoring dynamic load management and unplug derating — a diagnostic *(inferred)* |
| `lb active` | — | load balancing engaged |
| `lb wait for time` | — | load balancing holding for a time/schedule window *(inferred)* |

These fill in only while a vehicle is connected/charging; idle they are absent (the
decoded snapshot leaves them `null`).

---

## 4. Meter detection

Emitted while the box probes its meter inputs (notably at boot without a meter).

```
KLEFR: 1
KLEFR DETECTED
Klefr active
Nref: 448
```

| Line | Meaning |
|---|---|
| `<PROBE>: <n>` | a value the box prints for a meter-type probe during detection (e.g. `KLEFR: 1`, `PO: 1`) |
| `<PROBE> DETECTED` | that meter type **was** found (positive verdict, e.g. `KLEFR DETECTED`) |
| `<PROBE> NOT DETECTED!` | that meter type was **not** found (e.g. `KLEFR NOT DETECTED!`) |
| `Any metering device NOT detected!` | no meter found at all |
| `<probe> detect start` | began probing that meter type |
| `<probe>_init` | initialising that meter type |
| `No data received from P<n>!` | no meter response on phase *n* |
| `Klefr active` | the KLEFR meter is active |
| `Nref: <n>` | a reference value printed during detection *(inferred: neutral/zero reference)* |

`<PROBE>` tokens seen: `P1`, `P2`, `P3`, `PO`, `KLEFR`. Case differs between the
global (`detected`) and per-probe (`DETECTED`) forms — matched verbatim.

---

## 5. Control-pilot (CP) state machine

The headline control line, emitted on **state transitions** (not periodically):

```
S:C2 Auth:1 D:281 Cmax:16 Ph:3 Relay:7
```

| Field | Meaning |
|---|---|
| `S:<state><n>` | IEC-61851 CP **state** letter + a sub-index (see table); e.g. `A1`, `B1`, `C2`, `F1` |
| `Auth:<0/1>` | authorised to charge |
| `D:<n>` | CP **PWM duty** *(inferred — tracks `Cmax`: `D:281` at 16 A, `D:123` low)* |
| `Cmax:<n>` | max current currently offered (A) |
| `Ph:<n>` | number of phases (3) |
| `Relay:<n>` | contactor/relay state bitmask *(inferred)* |

### States (observed)

| State | Meaning | Seen as |
|---|---|---|
| `A` | no vehicle (unplugged) | `S:A1` |
| `B` | vehicle connected, not charging | `S:B1` |
| `C` | charging | `S:C2` |
| `F` | fault / no meter | `S:F1` |

A full plug → charge → stop → unplug cycle (evcc driving enable on/off):

```
S:B1 … Cmax:0  Relay:7   plugged, charging disabled
S:B1 … Cmax:16 Relay:7   current offered, about to start
S:C2 … Cmax:16 Relay:7   charging
S:C2 … Cmax:12 … 8 … 7   graceful ramp-down after enable=false
Stop Pwm1                 PWM cut
S:B1 … Cmax:0  Relay:7   back to connected-idle
S:A1 … Cmax:0  Relay:7   unplugged
```

`S:` plus `Cmax` is a real **plug- and charge-state** signal — no control pilot
read-out or OCPP needed.

### Related control events

| Line | Meaning |
|---|---|
| `Stop Pwm<n>` | CP PWM stopped — charging cut |
| `PE OK<n>` | protective-earth check OK |
| `Powercut Detected` | mains power interruption logged |

---

## 6. Errors

```
ERROR: 22
CLEAR: 22
```

| Line | Meaning |
|---|---|
| `ERROR: <n>` | an error code is raised (seen: `2`, `22`) |
| `CLEAR: <n>` | a previously-raised code is cleared |

---

## 7. Unexplained / artifacts

| Token | Status |
|---|---|
| `wc` | recurring, frequent — **meaning unknown** (evc04 issue #73) |
| `generic` | seen once during meter detection — unknown |
| `e`, `h: 0`, `lb wait for tim`, `…wc` fragments | **line fragments**, not real lines: a burst split mid-line across read windows. The reassembler joins these; a stray fragment that still fails to parse is dropped. |

---

## Decoder mapping

`core::cn28::parse_line` decodes a subset into `LogRecord`, folded into a
`Cn28Snapshot` and published as JSON on `evc04/cn28/telemetry`:

- per-phase `PhaseReading { v_mv, a_ma, w, wh }`, `temp_c`
- `ev_current_a`, `max_offered_a`, `lb_current_a`
- `meter_detected` (`true` on `… DETECTED`, `false` on `Any … NOT detected!`)
- `last_error` (set by `ERROR:`, cleared by `CLEAR:`)

The CP `S:` state line is captured raw today; structured decoding (→ plug/charge
state for Home Assistant / evcc) is the next step.

Anything unrecognised passes through untouched — it never breaks the snapshot.
