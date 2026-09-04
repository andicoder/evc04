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
| Interleaving | the box writes a **second output stream on top of the one it is already printing** — see below. A line can therefore start with, or contain, unrelated text. |

### Interleaved output ("the splice")

The box does not serialise its own console writes. A second producer emits status
text — observed as a 16-byte `MP lb current: 6` block — **into the middle of a line
already in flight**, overwriting its opening bytes. Measured on 2026-08-16 with a
`raw-debug` capture image (evc04#159), 66 probe windows across two runs:

- The splice lands on the **head of the burst**, every time.
- The first line of a burst is the `P1:` metering line, so **`P1`'s label was
  destroyed in 13 of 13 bursts** while `P2:` and `P3:` arrived clean. What survived
  in front of the payload was junk like `b c`, `.TE`, `fer` — often not even the
  phase digit.
- The V/A/W/Wh payload itself was intact in every case; only the label died.

This is genuinely the box's own output, not a read artefact: instrumenting every
`uart.read()` with a poisoned scratch buffer (`core::debug::trace`, topic
`evc04/cn28/raw/reads`) reported **zero unwritten bytes in every read of every
window**, so the bytes really do arrive on the wire in that order.

Jittering the probe interval does **not** help — the collision is tied to the burst
itself, not to a phase-lock between the poll period and the box's print period. A
90 s capture at `2 s ± 700 ms` produced the same 9-of-9 destroyed `P1` labels as the
fixed 2 s cadence.

**Consequences for a decoder.** Anchoring a record at the start of the line is wrong
for this transport. `core::cn28::parse_line` therefore falls back to *scanning*: it
anchors on the distinctive payload (`:\tV: …\tA: …\tW: …\tWh: …`, or `S:` plus a
`Cmax:` field) and reads the label backwards from there. Two rules keep that honest:

- A phase label counts only as a full `P<n>`. A lone surviving digit is rejected —
  the box prints `lb current:3`, and junk ending in a digit would otherwise
  mislabel a phase.
- A payload whose label is gone is held as *unlabelled* and promoted to phase 1
  **only** if a labelled `P2` follows it immediately, which is the documented burst
  frame below. Otherwise it is dropped. A mislabelled phase is worse than a missing
  reading.

### Connector

CN28 is a **4-pin 3.3 V TTL header**. Physical order, **counted from the bottom** of
the header (observed on one box — confirm orientation against your unit):

| Pin (from bottom) | Signal              | Wires to ESP32     |
|-------------------|---------------------|--------------------|
| 1                 | GND                 | GND                |
| 2                 | box TX (box sends)  | GPIO17 (UART1 RX)  |
| 3                 | box RX (box recvs)  | GPIO16 (UART1 TX)  |
| 4                 | — (NC)              | leave unconnected  |

Normal UART **cross-over**: the box's transmit (pin 2) feeds the ESP's **RX**
(GPIO17), and the box's receive (pin 3) is driven by the ESP's **TX** (GPIO16) —
never RX-to-RX. 🤔 The CN28 silk labels are **unreliable** (they don't line up with
signal direction the naive way), so wire by the pin position / function above, not by
the printed `RX2`/`TX2`. Pin 4 has no supply, so the ESP is powered separately (USB).
See [`esp32-pinout.md`](esp32-pinout.md) for the full device wiring.

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

One line per phase `P1`/`P2`/`P3`, emitted back to back and **in that order** —
which is what lets a decoder recover `P1`'s spliced-away label (see "Interleaved
output" above). Fields are **TAB-delimited** (`0x09`); each `X: ` label is followed
by a space then the value.

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

> **Unknown until the first transition.** Because `S:` is emitted only on a
> transition, the CP state is fundamentally *"last transition observed"* — the
> console cannot be queried for the current state. After a reboot the decoder
> holds no `cp_state` (it reports `null`) until the next plug/unplug/charge event.
> `null` is the honest, safe reading: the HA `cp_state` sensor keys its
> availability on that field, so it shows **unavailable** rather than a frozen
> `B`/`C` while unknown, and the evcc-facing `charge_state` mirror (#148)
> publishes `""` — which evcc treats as an error and **retains its previous
> status** (it does *not* map an unknown pilot to `A`). See evc04 #117.

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

## 8. Frame cadence

The LOG console is request/response: a byte on RX triggers a burst. But the
**metering values inside that burst refresh only ~every 5 s** — the box answers every
wake (the firmware auto-wakes every 2 s), yet only recomputes the per-phase `P{n}` /
`A:` / `lb current` / `ev current` fields on its own ~5 s internal metering tick. So
polling faster than ~5 s returns **repeated** values, not fresher ones. Observed live
on hardware.

That ~5 s metering cadence is what the **V4 control loop regulates on**: it tracks the
box's own grant (`lb_current`) from this feed, advancing **once per fresh ~5 s sample,
not per 1 s poll** (`SPECS.md` §6) — reacting on every poll would integrate stale,
repeated values and oscillate. The V4 loop was proven to ride the full 6–16 A
staircase at this feedback rate (settling ~1 A high near the 6 A floor).

---

## Decoder mapping

`core::cn28::parse_line` decodes a subset into `LogRecord`, folded into a
`Cn28Snapshot` and published as JSON on `evc04/cn28/telemetry`:

- per-phase `PhaseReading { v_mv, a_ma, w, wh }`, `temp_c`
- `ev_current_a`, `max_offered_a`, `lb_current_a`
- `meter_detected` (`true` on `… DETECTED`, `false` on `Any … NOT detected!`)
- `fault` — the standing fault as `{ code, first_seen_ms, count }` (`null` while
  healthy): raised by `ERROR: {n}`, and cleared **only** by a `CLEAR:` naming the
  same code. Stickiness is deliberate (#3): the box's own field is edge-triggered,
  so before this a fault that ended before anyone looked was unreportable.

The CP `S:` state line is decoded into `cp_state` (`null` until the first
transition) and, since #148, mirrored — guarded — into the control-plane
`charge_state` evcc reads (see [`mqtt.md`](mqtt.md)).

Anything unrecognised passes through untouched — it never breaks the snapshot.
