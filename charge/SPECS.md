# evc04-charge — Specification

A complete, self-contained brief for building the service. No external context is
required: everything the EVC04 does on the wire, the control math, the service
behaviour, and the open hardware questions are below.

---

## At a glance

The whole system in one picture — the closed control/data loop and the physical
wiring. Labels are the real env vars (§7) and MQTT topics (§8) so the diagram
doubles as a map into the rest of this spec. No new claims here; details and
evidence live in §2–§9.

```
                   CONTROL / DATA FLOW  —  the closed loop
                   ═════════════════════════════════════════

  ┌────────────────────────────────────────────────────────┐
  │ Home Assistant / evcc              (the "brain")       │
  │ day-ahead price | PV surplus | departure planning      │
  └────────────────────────────┬───────────────────────────┘
                     target +  │
                     measured  │
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ MQTT broker                                            │
  │   in   MQTT_TOPIC_TARGET     desired current (A)       │
  │   in   MQTT_TOPIC_MEASURED   live current (A)          │
  │   out  MQTT_TOPIC_STATUS     liveness (retained)       │
  └────────────────────────────┬───────────────────────────┘
                     target +  │
                     measured  │
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ evc04-charge daemon                                    │
  │   Modbus-RTU slave  ==  emulated Inepro PRO380         │
  │   reported = clamp(offset + measured)                  │
  │   offset   = MAX_BOX_AMPERE - target                   │
  │   soft-ramp | min-charge cutoff | failsafes->full      │
  └────────────────────────────┬───────────────────────────┘
                      raw RTU  │
                     over TCP  │
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ TCP<->RS485 gateway     (Waveshare, transparent)       │
  └────────────────────────────┬───────────────────────────┘
                        RS485  │
                     9600 8E1  │
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ EVC04 wallbox  ·  Power Optimizer                      │
  │   polls the emulated meter @ ~1 Hz                     │
  │   own closed loop: rampere charge current until          │
  │   total main-line current = MAX_BOX_AMPERE (DIP 4-5-6) │
  └────────────────────────────┬───────────────────────────┘
                     delivers  │
                       charge  │
                               ▼
                           ┌─────────┐
                           │   car   │
                           └─────────┘

   feedback that closes the loop: the car's real draw raises the
   measured main-line current → a CT reports it → MQTT_TOPIC_MEASURED
   → daemon serves offset + measured → the box settles at `target`.

   legend — the two inbound values:
     desired current  (MQTT_TOPIC_TARGET)    setpoint: how fast you WANT to charge,
                                             from the brain (price / PV / departure)
     live current     (MQTT_TOPIC_MEASURED)  measurement: what is ACTUALLY flowing
                                             now, read from a CT — closes the loop


                   PHYSICAL WIRING
                   ═══════════════════

   measured-current source                 ┌───────────────────────────┐
   (grid / total CT today,                 │  broker + daemon host     │
    e.g. sensor.net_power;     ──MQTT──►    │  (container)              │
    charger-side CT later —                 └─────────────┬─────────────┘
    same MQTT_TOPIC_MEASURED)                             │ LAN / PoE
                                                          ▼
                                            ┌───────────────────────────┐
                                            │ Waveshare RS485-TO-ETH (B) │
                                            └─────────────┬─────────────┘
                                                          │ one twisted pair
                                                          │ A / B, 9600 8E1
                                                          ▼
                                            EVC04  CN20  ( V │ GND │ A │ B )
                                            ────────────────────────────────
                                            DIP 4-5-6  =  MAX_BOX_AMPERE
                                            the box's current ceiling (16 A)


              SOFTWARE COMPONENTS  —  tasks & tokio channels
              ════════════════════════════════════════════════

  Runtime: #[tokio::main(current_thread)].  Three cooperating tasks,
  wired only through lock-free tokio::sync::watch channels (no locks).

  ══════════════════════════ MQTT broker ══════════════════════════
                       │  target ▼   measured ▼                      status ▲ (each poll)
                       │
  ┌────────────────────────────────────────────────────────────┐
  │ TASK 1 · run_mqtt    (main · rumqttc event loop)           │
  │   on target   → apply()           writes «target»          │
  │   on measured → apply_measured()  writes «measured»        │
  │   on parse    → last_error        writes «error»           │
  │   serves status() back to the broker each poll             │
  └────────────────────┬───────────────────────────────────────┘
                       │  watch<Sample> «target»        → run_ramp AND Controller
                       │  watch<Measurement> «measured» ───────────→ Controller
                       ▼
  ┌────────────────────────────────────────────────────────────┐
  │ TASK 2 · run_ramp    (spawned)                             │
  │   read «target» · soft-ramp the offset @ ~1 Hz             │
  │   offset = MAX_BOX_AMPERE − target  (rate-limited)         │
  └────────────────────┬───────────────────────────────────────┘
                       │  watch<Ampere> «offset»        ───────────→ Controller
                       ▼
  ┌────────────────────────────────────────────────────────────┐
  │ Controller   (Clone — pure snapshot reader)                │
  │   reads «target» + «measured» + «offset»                   │
  │   reported = clamp(offset + measured)                      │
  │   min-charge cutoff · staleness failsafes → full           │
  └────────────────────┬───────────────────────────────────────┘
                       │  Controller.reported_frame()   (cloned; read every poll)
                       ▼
  ┌────────────────────────────────────────────────────────────┐
  │ TASK 3 · run_link    (spawned — gateway TCP + RTU slave)   │
  │   answer 0x500C×6 poll · validate/emit CRC16 · watchdog    │
  │   per poll     → writes «poll»     (watch<Instant>)        │
  │   link up/down → writes «gateway»  (watch<LinkHealth>)     │
  └────────────────────┬───────────────────────────────────────┘
                       │  raw RTU frames over TCP
                       ▼
  ═══════════════════ EVC04  (via TCP↔RS485 gateway) ═══════════════

  status() (in TASK 1) reads Controller + «gateway» + «poll» + «error»
  → publishes MQTT_TOPIC_STATUS (retained, + offline LWT).
```

The six `watch` channels (all `tokio::sync::watch`, last-value-wins, lock-free):

| channel    | payload           | written by                     | read by               | carries                                   |
|------------|-------------------|--------------------------------|-----------------------|-------------------------------------------|
| «target»   | `Sample`          | `run_mqtt` → `apply()`         | `run_ramp`,`Controller` | desired current + arrival time (staleness) |
| «measured» | `Measurement`     | `run_mqtt` → `apply_measured()`| `Controller`          | live current that closes the loop          |
| «offset»   | `Ampere`          | `run_ramp`                     | `Controller`          | soft-ramped `MAX_BOX_AMPERE − target`      |
| «poll»     | `Instant`         | `run_link` (each answered poll)| `status()`            | bus liveness (`last_poll_age_s`)           |
| «gateway»  | `LinkHealth`      | `run_link`                     | `status()`            | TCP link up/down                           |
| «error»    | `Option<String>`  | `run_mqtt` callbacks           | `status()`            | last parse/validation error                |

`Controller` itself is `Clone`, not a channel: each clone holds the three receiver
handles, so `run_link` and `status()` read a consistent snapshot without locking.

---

## 1. Goal

Control charging on a **Vestel EVC04-AC11-T2P** wallbox so that an external
controller (Home Assistant / evcc, following day-ahead prices and/or PV surplus)
can decide **when and how fast** the car charges — both **continuous current
modulation** and **on/off** gating, the controller's choice.

The constraint that shapes the entire design: **this box has no communication
module**, so none of the "normal" control paths work (see §2). The only available
lever is the box's **Power Optimizer**, which polls an external energy meter over
RS485 and runs a **closed feedback loop** that rampere charge current until the
measured total reaches its configured current limit. We **emulate that meter**:
feeding it a value that tracks the **live measured current** closes the loop and
the box **modulates** (proven on hardware); feeding a static value gives **on/off**
only (§6). The charging *brain* (price / PV / departure planning) stays in the
external controller — Home Assistant or **evcc** (§8) — never in this service,
which is a mode-agnostic actuator: `target` + `measured` in, meter emulation out.

**This service is a throttle-only overlay.** The box's baseline — no meter, or the
Power Optimizer disabled — is **full charge (11 kW)**. We emulate the meter *only
to charge less* than that baseline, for PV surplus / price optimisation / load
distribution. **Protecting the building fuse is explicitly out of scope** — that is
the job of the installation and the DIP-set limit. The failsafe direction on a
control-layer failure is **configurable** (§9): the default is **`pause`** — for an
evcc/HA-managed box, a control-path blip must *stop* charging, not start it at the
worst time (#52). The original *never worse than no tool* baseline (fall back to
**full charge**) stays available (`*_FAILSAFE=full_charge`) for an unmanaged box.

---

## 2. Hardware facts (the "why")

**Charger:** Vestel EVC04-**AC11-T2P** — the *basic* Home variant.
- 11 kW, 3-phase, tethered Type 2, RFID only.
- Model code has **no `W`/`D`/`S`/`A` suffix** → **no WiFi, no display, no app,
  no Ethernet/comms module**. Type-label LAN/WiFi/BT/IMEI/MAC fields are blank.
- OCPP 1.6 and Modbus-**TCP** *slave* control exist **only on the SW / Connect /
  HOME SMART variants** that have the optional network module. Not us.
- Example unit used during reverse-engineering: SN `70001992100133`, mfd
  2021-06-07.

**Control paths that were tried and ruled out:**
- ❌ **OCPP** — needs the network module (absent).
- ❌ **Modbus-TCP control** — needs the network module (absent).
- ❌ **Modbus-RTU *slave* on CN20** — the basic box does **not** answer as a
  slave. CN20 A/B is the Power Optimizer's **meter-reading bus** where the EVC04
  is the **master**. Confirmed by exhaustive bench testing (all slave IDs,
  register types, both A/B polarities, correct 9600 8E1 framing → zero bytes
  returned) and by evcc's `charger/vestel.go` ("Vestel/Hymes wallboxes with
  Ethernet (SW modells)").

**Control path that works:** ✅ **Emulate the meter** the EVC04 polls (§4–§6).

**The Power Optimizer:** enabled via on-board **DIP switches 4-5-6** (set a current
limit per a DIP table; any non-all-off value enables polling). Once enabled, the
box continuously polls an external meter on CN20 and runs a closed loop that holds
the measured total current at that limit (see §6). We mirror this limit as
`MAX_BOX_AMPERE`.

```
DIP 4-5-6 = Power Optimizer main-fuse current limit (manual Table-14).
Only these 3 pins set the limit; all-OFF disables the optimizer.
(Pin 1 reserved, Pin 2 = External Enable, Pin 3 = Locked Cable.)
Never flip DIPs while the box is powered.

  63 A  (ON-ON-OFF)                 16 A  (OFF-OFF-ON)
        4    5    6                       4    5    6
      +----+----+----+                  +----+----+----+
   ON |[##]|[##]|    |               ON |    |    |[##]|
      +----+----+----+                  +----+----+----+
  OFF |    |    |[##]|              OFF |[##]|[##]|    |
      +----+----+----+                  +----+----+----+

  full table        ( # = ON / rocker up,  . = OFF / rocker down )
  ----------------------------------------------------------------
   4 5 6   limit            4 5 6   limit
   . . .   disabled         # . .   32 A
   . . #   16 A             # . #   40 A
   . # .   20 A             # # .   63 A
   . # #   25 A             # # #   80 A
```

The DIP-set limit must equal `MAX_BOX_AMPERE` so the §6 offset math lands the edge
where expected. The DIP value trades full-charge headroom against modulation range
(§9): a **low** limit (16 A, tested) maps the closed loop onto the car's real
6–16 A envelope for PV modulation; a **high** limit guarantees unthrottled full
charge under household load. **16 A is the current recommended operating DIP.**

**CN connectors (for context):**
- **CN20** — RS485 meter bus. Silkscreen `V | GND | A | B`. This is where we tap.
- **CN25 (VESLINK)** — Vestel service/firmware port (WG-VESTA Veslink USB).
  Installer-only; not a user control path.

---

## 3. Physical link

The service speaks **Modbus RTU**, but it runs in a container (no local serial
port at the box). So the RS485 bus is bridged to the network with a
**TCP↔RS485 gateway** in **transparent / raw mode**:

- Reference gateway: **Waveshare RS485 TO ETH (B)**, PoE variant (single cable).
- Gateway mode: **transparent passthrough (Protocol = None)** — *not* the
  gateway's own "Modbus TCP↔RTU" mode. We frame Modbus RTU ourselves (CRC and
  all) and the gateway just shuttles raw bytes.
- Serial line params: **9600 baud, 8 data bits, EVEN parity, 1 stop bit
  (9600 8E1)**. (8E1 is mandatory — the Inepro/EVC04 bus uses even parity.)
- Wiring: one twisted CAT pair → EVC04 **CN20** terminals **A** and **B**.

The service opens a **TCP socket** to the gateway (host:port, e.g.
`192.168.x.x:4196`) and reads/writes raw RTU frames over it.

---

## 4. The protocol on the wire

The EVC04 (master) polls our emulated meter (slave) on a fixed timer:

| Field | Value |
|---|---|
| Bus params | **9600 8E1** |
| Slave address | **1** |
| Function code | **0x03** (Read Holding Registers) |
| Start register | **0x500C** (decimal 20492) |
| Quantity | **6** registers |
| Cadence | fixed **~1.006 s**, timer-driven |

The exact poll frame (hex):

```
01 03 50 0c 00 06 14 cb
└┬ └┬ └──┬─ └──┬─ └──┬─
 │  │    │     │     └ CRC16 (Modbus)
 │  │    │     └────── quantity = 6
 │  │    └──────────── start register = 0x500C
 │  └───────────────── FC = 0x03
 └──────────────────── slave addr = 1
```

**Important:** the cadence is **content-agnostic** — the box re-polls every
~1.006 s regardless of whether we answer with a valid frame, stay silent, or send
a bad CRC. So the **bus alone cannot tell you whether the box acted on your
values**; the only ground-truth observable is the **actual delivered charge
current**, which needs a car plugged in (see §8).

---

## 5. Register map (identified: Inepro PRO380)

The `0x500C × 6` block is the **Inepro PRO380** meter map (confirmed against
official Inepro register docs V1.18 and V2.18). The 6 registers are **3× Float32,
big-endian (ABCD byte order)** = the three per-phase **currents**:

| Register | Field | Type | Unit |
|---|---|---|---|
| `0x500C` | **L1 current** | Float32 ABCD | A |
| `0x500E` | **L2 current** | Float32 ABCD | A |
| `0x5010` | **L3 current** | Float32 ABCD | A |

Encoding of the 12-byte response payload is plain:

```python
import struct
payload = struct.pack('>fff', i_l1, i_l2, i_l3)   # big-endian float32 ABCD
```

Verified full response frames (addr 01, FC 03, byte-count 0x0c, + CRC16):

```
all 0 A   →  01 03 0c 00000000 00000000 00000000 93 70
16 A      →  01 03 0c 41800000 41800000 41800000 97 ae
63 A      →  01 03 0c 427c0000 427c0000 427c0000 13 97
```

The fuller Inepro PRO380 map also exposes voltages (`0x5000`), frequency
(`0x5008`), active power (`0x5012`), and energy (`0x6000`), **but the EVC04 only
ever reads the three currents at `0x500C/0E/10`.** Implement just those; you may
optionally answer the wider map for robustness, but it is not required.

---

## 6. Control math

### The box runs its own closed loop

The Power Optimizer's nominal rule looks open-loop:

```
available_charge_current = MAX_BOX_AMPERE − reported_household_current   (per phase)
```

`MAX_BOX_AMPERE` is the value selected by **DIP 4-5-6** on the board (§2). But the box
does **not** treat the meter value as a static ceiling. The Power Optimizer
measures the **total main-line current including the charger's own draw** and runs
a **closed feedback loop**, ramping the charge current until the *measured total*
sits at `MAX_BOX_AMPERE`. This single fact drives the whole control design.

### A static feed gives on/off only (measured, with a car)

Because a fabricated static value never rises as the car charges, a **static**
`reported = MAX_BOX_AMPERE − target` cannot land the loop anywhere in between — the box
always rampere to full (`reported < MAX_BOX_AMPERE`) or cuts off
(`reported ≥ MAX_BOX_AMPERE`). Bench tested with a car at `MAX_BOX_AMPERE = 65 A`
(DIP on-on-off):

| reported (all 3φ) | `65 − reported` | delivered charge | state |
|---|---|---|---|
| 0 … 63 A | 65 … 2 | ~11–12 kW | **full** |
| 64 A | 1 | ~7 kW, unstable (still ramping down) | transition |
| 65 A | 0 | ~0 | **pause** |
| ≥ 66 A | ≤ −1 | ~0 | pause |

The pause edge sits **right at `reported = MAX_BOX_AMPERE`**, with only a 1–2 A
transition zone — **not** a usable proportional band (the box's hardware max is far
below 65 A, so `available` stays ≥ box-max until `reported` is within ~2 A of the
limit). Re-tested at DIP 16 A: the **same** on/off cliff. So the earlier claim of
"continuous modulation in between" was **wrong** for the static model.

> **Refinement (#57).** To actually *cut* an active charge the report must **exceed**
> the limit, not merely reach it: at `reported = MAX_BOX_AMPERE` the box holds the
> charge; at `reported = MAX_BOX_AMPERE + 2..4 A` it cuts (grid flipped from import to
> export on hardware). So a hard pause / `pause` failsafe reports
> `MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE` (§7), and `charge_state` treats only
> `reported > MAX_BOX_AMPERE` as paused (reporting *at* the ceiling is live modulation).

### Closing the loop makes it modulate (proven)

Feed a value that **rises with the actual draw**:

```
reported = clamp(offset + measured_current, 0, ..)        (per phase)
offset   = MAX_BOX_AMPERE − target
```

`measured_current` is a **live** per-phase current published to the service over
MQTT (§7/§8). Now the box sees its own draw climb, the loop settles, and the
delivered current tracks `MAX_BOX_AMPERE − offset = target`. Proven on hardware:

- offset 0 → settles **15 A**; offset 4 → settles **12 A** (stable equilibria).
- **Soft-ramping** the offset toward its setpoint (rate-limited, not a step)
  extends the stable range down to ~**9 A** — a hard offset jump shocks the box
  into over-throttling below the car's 6 A floor and the session collapses.
- With home-automation-speed measurement (~3–6 s round-trip) the stable range is
  ~**9–15 A**; the bottom (6–8 A) hunts and would need a faster (~1 s) measurement
  source (publishable over the same topic, no service change).
  > **Superseded (#134/#135).** The "needs a ~1 s source" conclusion was wrong: the
  > 6–8 A bottom was unreachable because the upper clamp at `MAX_BOX_AMPERE` never
  > let the box see "over the limit" (H1), not because the measurement was too slow.
  > The box ramps *itself* once told it is over — see *The box's grant loop,
  > measured* below (live ride-down to 6 A on ~5 s feedback).
- **3-phase floor ≈ 6 A ≈ 4.1 kW.** Below that the box can't hold a stable
  current, so a **minimum-charge cutoff** applies: `target < MIN_CHARGE_AMPERE` (~6 A)
  → serve a hard pause (`reported = MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE`, **above** the
  ceiling so the box actually cuts — reporting exactly the ceiling holds an active charge,
  #57), don't try to modulate the floor.

### The measured input is source-agnostic

The service does not care what `measured_current` represents — it just serves
`offset + measured`. Today the home-automation side publishes **total/grid
current** (giving load-management + PV-surplus semantics); a charger-side CT can be
retrofitted later by publishing the **car** current to the **same topic**, for
precise per-car control, with **no service change**.

### Mode selection lives in the controller, not here

The service is **mode-agnostic**: it consumes `target` + `measured` and nothing
else. The controller (Home Assistant / evcc, §8) picks behaviour purely by the
target it publishes:

- **full charge** (cheap price window) → `target ≥ MAX_BOX_AMPERE` → offset 0 → the
  loop holds the *total* at `MAX_BOX_AMPERE` (effectively max; the box's own loop
  keeps the total within that limit — fuse protection is the box's job, not ours, §1).
- **modulate** (PV surplus) → `target` = surplus-derived → tracked within the
  stable band.
- **pause** → `target < MIN_CHARGE_AMPERE` → hard pause.

`MAX_BOX_AMPERE` must match the DIP setting (§2) for the offset math to land where
expected; the DIP value itself trades full-charge headroom against modulation
range (§9).

> **Implementation status.** The closed-loop model above is **implemented** (#22–#25):
> the measured input, `reported = clamp(soft_ramped_offset + measured)`, the
> min-charge cutoff, and both staleness failsafes (target + measurement, always
> toward full charge) ship in the daemon. The status topic also exposes the
> approximated evcc `charge_state` and ships an evcc charger template
> ([`docs/evcc.md`](docs/evcc.md), #28). Only the high-DIP modulation question
> (#27, needs a hardware test) is deferred beyond v0.1.

### On-box floor-seek: layered integral trim (#119, core + firmware only)

The `9–15 A` band above is set by the ~3–6 s measurement round-trip. The **on-box**
device (the `core` + `firmware` port, **not** this daemon — the daemon has no CN28
feed) adds a **layered integral trim** on top of the proven loop to push below that
band toward the box's real minimum:

```
reported = clamp(offset + measured + trim, 0, MAX_BOX_AMPERE)     (per phase)
# advanced once per FRESH CN28 sample (~5 s), NOT per 1 s tick:
trim += TRIM_KI · (cn28_actual − target)        # anti-windup: clamp(trim, 0, TRIM_MAX)
```

`cn28_actual` is the box's own delivered per-phase current read from the CN28 LOG
(the internal KLEFR meter, #108). The trim is a slow **floor-seeker**: while the box
charges above target it grows, lifting `reported` so the box throttles further, until
`cn28_actual = target` (equilibrium) or `trim` saturates at `TRIM_MAX` — and that
**saturation is the achievable-minimum signal** (#119 Goal 2: the box may not reach
6 A, so the trim *reveals* the real floor instead of assuming it).

Key constraint — **CN28 feedback is ~5 s, not ~1 s.** The box answers every 2 s wake
but only recomputes its metering ~every 5 s, so the trim integrates **per fresh
sample**, not per tick — integrating stale data each 1 s tick would over-correct ~5×
and oscillate. When the feedback goes stale the trim **decays toward 0** (`TRIM_DECAY`
per sample), relaxing back to the proven `offset + measured` loop rather than holding
a value it can no longer see. `trim = 0` is byte-identical to the pre-#119 path, so
the hardware-proven 9–15 A behaviour is unchanged whenever the trim is idle.

> **Superseded by the measured grant loop below (#134/#135).** The trim can only
> lift `reported` *to* the ceiling, never over it (H1), so it pins at the edge and
> saturates instead of pushing the box down. It stays documented as the shipped
> state until the V4 controller replaces it.

### The box's grant loop, measured (#134/#135)

The dynamics behind all of the above, measured on hardware 2026-07-02 (fixtures
`core/tests/fixtures/sessions/`, model `core/src/charge/boxsim.rs`). On a **~6 s
eval cadence** (observed 5–10 s) the box recomputes its grant to the car
(`lb_current` in the CN28 LOG) from the meter value it polls:

| meter reads (vs. DIP limit `MAX`) | box response per eval |
|---|---|
| below `MAX` | `lb ← min(car_draw + (MAX − reported), MAX)` — headroom is added **on top of the live draw** (the fast-up ratchet) |
| at `MAX` … `MAX + 0.5` | hold (dead zone; #57's "at the ceiling holds") |
| `MAX + 0.5` … cut | shed `floor(excess)` A — **proportional**: +1.0/+1.5 → −1 A per ~6 s, +2.0 → −2 A per ~6 s, ridden live from 16 A down to 6 A with no cut |
| above the cut threshold (> +2, ≤ +4) | hard cut (session drop; pause reports `MAX + PAUSE_MARGIN_AMPERE` = +4) |

Session start grants the apparent headroom outright: `lb ← MAX − reported`.

**Consequence — the V4 controller** (`lb_tracking_report`, `core`): regulate the
grant directly on the ~5 s CN28 `lb_current` feedback and stop stacking
offset/measured/trim. Grant above target → report `MAX + clamp(err, 1, 2)` (the box
sheds it proportionally); at target (±1 A) → report exactly `MAX` (holds); below
target → report the deficit as headroom (`MAX − (target − lb)`), which also covers
the start-grant (`lb = 0` → grant = target). Proven in the simulator across the
full 6–16 A staircase, under deep PV export, for eval periods 5–10 s
(`core/tests/replay.rs`); the single planned live test (#135 step 6) confirmed the
box side. Grid `measured` is no longer part of the modulation math — which also
retires the H2 export-clamp failure mode by construction (#136).

---

## 7. The service

A long-running process with three concerns:

1. **Gateway link.** Maintain the TCP connection to the RS485 gateway
   (transparent mode, 9600 8E1 implied on the serial side). **Auto-reconnect**
   with backoff; a **watchdog** that detects a dead/stalled link and re-establishes
   it. The box polls at 1 Hz — missing a few polls is fine, a prolonged silence
   is **not**: with the Power Optimizer enabled the box **hard-faults to a solid
   red LED** when the meter stops answering (confirmed on real hardware, see §9).
   Treat continuous availability of the slave as a hard requirement, and make
   rollouts **overlap** — the new instance must be answering polls *before* the
   old one stops, or the box sees a gap and faults.
2. **Modbus-RTU slave.** Parse incoming RTU frames; for the `addr 1 / FC03 /
   start 0x500C / qty 6` poll, respond with the 12-byte `>fff` payload (L1/L2/L3
   reported current) + correct Modbus CRC16. Ignore / exception-respond to
   anything else. Validate inbound CRC.
3. **MQTT control.** Subscribe to **two** inbound topics — a **target charge
   current** and a **live measured current** (§6). Compute the served value
   `reported = clamp(offset + measured)` with `offset = MAX_BOX_AMPERE − target`
   **soft-ramped** toward its setpoint, apply the **minimum-charge cutoff**
   (`target < MIN_CHARGE_AMPERE` → hard pause), and update the values the slave serves.
   Two independent staleness checks (§9), both failing toward **full charge**
   (report 0): if the **target** goes stale, drop the command and serve full charge;
   if the **measured** input goes stale, abandon the closed loop and serve the same
   static full-charge value (never `offset + stale_measured`). **Publish
   liveness/status** back to MQTT.

**Configuration — all via environment variables** (no config files, no secrets in
the image). At minimum:

| Env var (suggested) | Meaning |
|---|---|
| `GATEWAY_HOST` / `GATEWAY_PORT` | RS485↔TCP gateway address (e.g. Waveshare) |
| `MAX_BOX_AMPERE` | the box's DIP-set current ceiling, ampere — our 100 % reference for the headroom math (must match DIP 4-5-6) |
| `MQTT_HOST` / `MQTT_PORT` / `MQTT_USER` / `MQTT_PASS` | broker |
| `MQTT_TOPIC_TARGET` | inbound: target charge current (A) |
| `MQTT_TOPIC_MEASURED` | inbound: live measured per-phase current (A), closes the loop (default `evc04/measured`) |
| `MQTT_TOPIC_STATUS` | outbound: liveness/state (retained) |
| `SLAVE_ADDRESS` | default 1 |
| `POLL_REGISTER` / `POLL_QUANTITY` | default 0x500C / 6 (override only for debugging) |
| `MIN_CHARGE_AMPERE` | below this target → hard pause; don't modulate the 3φ floor (default 6) |
| `PAUSE_MARGIN_AMPERE` | amps **above** `MAX_BOX_AMPERE` a pause reports so the box actually cuts an active charge — reporting exactly the ceiling holds it (hardware-confirmed, #57); default 4 |
| `RAMP_RATE_AMPERE_PER_SECOND` | soft-ramp slope for the offset, A per second (default 0.5) |
| `TARGET_TIMEOUT_SECONDS` | seconds the last target stays valid before the **full-charge** failsafe engages (default 60; must exceed the controller's republish interval) |
| `MEASURED_TIMEOUT_SECONDS` | seconds the last measured value stays valid before the measurement failsafe falls back to **full charge** (default 15; see §9) |
| `RUST_LOG` | log verbosity (`tracing` `EnvFilter` syntax; default `info`). E.g. `info`, `debug`, or `evc04_charge=debug,rumqttc=warn`. Logs go to stdout; the 1 Hz poll path is at `trace`, so `info` stays quiet and never prints `MQTT_PASS` (#43) |
| `HA_DISCOVERY_ENABLED` | publish Home Assistant MQTT discovery configs on connect so HA auto-creates the read-only status sensors (default `false`, opt-in; #46) |
| `HA_DISCOVERY_PREFIX` | HA discovery prefix (default `homeassistant`) |
| `HA_DISCOVERY_NODE_ID` | node-id segment + device identifier for discovery (default `evc04`; make unique per install when several share a broker) |
| `TARGET_FAILSAFE` | direction when the **target** goes stale: `pause` (**default**, report `limit + PAUSE_MARGIN_AMPERE` → box stops, #57) \| `full_charge` (report 0, the meterless baseline) \| `hold_last` (keep the last command). `full_charge` only for an HA-automation-only box (#51/#52) |
| `MEASURED_FAILSAFE` | direction when the **measured** input goes stale: same modes, **default `pause`** (#51/#52) |

**Origin:** a hand-rolled pymodbus RTU slave first proved the `0x500C × 6` poll
could be answered cleanly over the Waveshare in transparent mode (no resync
storms). The shipping service is **Rust / tokio** (see `CLAUDE.md` for the stack
rationale); the verified frames in §5/§11 make the protocol a fixed target.

---

## 8. MQTT contract

**The full, authoritative contract lives in [`docs/mqtt.md`](docs/mqtt.md).** All
payloads are UTF-8 JSON; QoS 1; target/measured/status retained. Summary:

- **Inbound — target** (`MQTT_TOPIC_TARGET`): `{ "ampere": N }`, the desired charge
  current. Out-of-range is clamped (not rejected); invalid payloads are ignored
  and the last good value held, surfaced in `last_error`.
- **Inbound — measured** (`MQTT_TOPIC_MEASURED`): `{ "ampere": N }`, the live
  per-phase current that closes the loop (§6). Source-agnostic; same
  hold-last-good / staleness discipline as the target.
  > **V4 firmware delta (#135/#136):** on the on-box firmware this topic is
  > replaced by **`evc04/charge/grid_power`** — `{ "watt": N }`, the raw *signed*
  > grid power forwarded untouched (no formulas outside evc). V4 regulates on the
  > box's own grant (§6, the measured grant loop) and consumes only the topic's
  > cadence as the controller liveness heartbeat (>15 s → pause); the watts are a
  > status diagnostic. The status object drops the offset/ramp/trim fields and
  > gains `grid_power_w`, `grid_age_s`, `grid_failsafe`, `lb_current_ampere`.
- **Outbound — status** (`MQTT_TOPIC_STATUS`, retained, + offline LWT): `online`,
  `target_ampere`, `reported_ampere`, `last_poll_age_s`, `gateway`, `mqtt`, `last_error`,
  plus the closed-loop fields `measured_ampere`, `offset_ampere`, `measurement_age_s`,
  `ramping`, the two failsafe flags (`failsafe` for target, plus a measurement
  failsafe), and `charge_state` — the approximated evcc `B`/`C` charging state (#28;
  `A` is never asserted, the emulation has no control-pilot line).

**The brain is evcc** (#28): this service is a mode-agnostic actuator, driven as an
**evcc custom charger** — `maxcurrent` → target, `enable=false` → target below the
cutoff, `status` ← our `charge_state`. evcc's control interval must exceed the
inner loop's settle time (~30–60 s) or the two loops hunt. The working charger
template, min/max-current band, and nested-loop timing live in
[`docs/evcc.md`](docs/evcc.md). A HA-only setup (number entity → target, sensor ←
status) also works for simple on/off + manual current.

---

## 9. Open items — must be resolved on real hardware (car plugged in)

These are **not** answerable from the bus alone; they need an observable
(delivered charge current with a car connected):

- [x] **DIP 4-5-6 current limit = 65 A** (DIP on-on-off). Confirmed empirically: the
      charge cliffs to pause near `reported = 65 A` (see §6).
- [x] **Pause must exceed the limit, not just reach it (#57).** At `reported =
      MAX_BOX_AMPERE` the box holds an active charge; only `reported >
      MAX_BOX_AMPERE` cuts it. Hard pause / `pause` failsafe now report
      `MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE` (default +4 A); `charge_state` treats
      only `reported > MAX_BOX_AMPERE` as paused (see §6/§7).
- **Failsafe behaviour — partially confirmed on real hardware (no car):**
  - **Meter goes silent** (Power Optimizer enabled): the box raises a
    **meter-communication fault → solid red LED**. It keeps polling at ~1 Hz the
    whole time (it never gives up, just waits for the meter to return).
  - **Meter returns:** after a few consecutive good readings the box clears the
    fault and goes **green/ready** again — a **soft** clear, no power-cycle needed
    (observed once, idle/no car). Whether a fault taken *mid-charge* latches and
    needs a physical power-cycle is **still open** (needs a car).
  - **Meter reports 0 A** with no car: box is simply **ready/idle** — 0 A does
    **not** start a phantom session, so it is the safe startup/unknown-state value.
    With a car, `reported = 0` is **full charge** — so "full charge" and "safe
    default" are the *same* served frame.
  - **Design consequence — two failsafe layers; the in-app direction is configurable:**
    1. **Control input stale, slave still answering** (broker down, controller
       offline, cold start past the grace window): keep answering, with a
       **configurable** direction per channel (`TARGET_FAILSAFE` / `MEASURED_FAILSAFE`,
       #51/#52). `TARGET_TIMEOUT_SECONDS` bounds the target staleness and
       `MEASURED_TIMEOUT_SECONDS` the measured one.
       - **`pause`** (**default**, report `limit + PAUSE_MARGIN_AMPERE` → box stops, #57):
         the safe direction for
         an **evcc/HA-managed** box, where a control-path blip (e.g. a nightly router
         reconnect) must **not** flip an intended pause into charging overnight. evcc's
         idle target cadence is decision-driven and unbounded, so no finite timeout
         alone is enough — the direction must change, not just the window (#52).
       - **`full_charge`** (`reported = 0` — the meterless-box baseline): for a
         Home-Assistant-automation-only / unmanaged box where charging-on-fault is
         acceptable and fuse protection is out of scope (§1) — *never worse than no tool*.
       - **`hold_last`**: keep serving the last command (a stale pause stays a pause).
         Caveat: can hold a stale *charge* across a charge→no-charge boundary.
       When both failsafes fire with a forced value, the safest (least-charge) wins.
    2. **Process dead, slave silent** (crash): with the Power Optimizer enabled the
       box **hard-faults to red — it does *not* fall back to full charge**. This
       layer is unreachable from inside the app, so the deployment **must
       auto-restart** the process and make rollouts **overlap** (§7). The box's own
       **Failsafe Current** (control-interface reg `2000`, SW variants only) or the
       **External Enable** input (DIP pin 2 + relay) is an optional independent
       backstop against total service loss.
  - [ ] **Still open (needs a car):** exact meter-timeout window (how many missed
        polls before red?), and whether a fault taken mid-charge latches vs. clears
        soft like the idle case.
- [x] **Validate end-to-end with a car — done.** Reported all-zeros → full-current
      charge (~11–12 kW). Ascending sweep showed the box charges full until
      `reported ≈ 63 A` and cliffs to pause at `reported = 65 A` (= `MAX_BOX_AMPERE`),
      with only a 1–2 A transition zone — **not** a wide linear region (see §6).
      This is the static-feed on/off behaviour the hardware gives at this DIP, and
      the evidence that motivated the closed-loop model (§6).
      Caveat: HA's house meter (`sensor.smartmeter_*`) does **not** see the
      wallbox; the usable observable was the Tibber Pulse grid sensor
      (`sensor.net_power`). Cars also ramp up slowly (~1–2 min) after plug-in, so
      measure throttling by sweeping reported current **upward** (charge down =
      fast response), never downward (charge up = slow ramp confound).
- [x] **Closed-loop modulation at a lower current limit — proven.** At DIP 16 A,
      feeding `offset + live_measured_current` makes the box modulate: offset 0 →
      15 A, offset 4 → 12 A; soft-ramping the offset holds the loop stable down to
      ~9 A (~9–15 A with HA-speed measurement; the 6–8 A bottom hunts). Static feed
      still cliffs on/off at any DIP. This is what replaced the open-loop model
      (§6).
- [x] **Operating DIP current limit decided = 16 A (#27).** The DIP couples two
      opposed goals: **modulation** wants a low limit (16 A maps onto the car's
      6–16 A envelope, tested) while **guaranteed full charge** wants a high limit
      (offset 0 holds the *total* at the limit, so at 16 A a loaded household
      throttles the car). The installation is set to **DIP 16 A**, trading some
      full-charge headroom for the proven stable modulation band — so `MAX_BOX_AMPERE`
      = 16 must match the physical DIP 4-5-6 setting. Open sub-question (only if more
      full-charge headroom is later needed): **does closed-loop modulation stay stable
      at a higher DIP (e.g. 32 A)?** Untested; re-run the offset/soft-ramp sweep at
      DIP 32 A with simulated household load before changing it. Photograph DIP state
      first (revert safety).
- [x] **Measurement-input staleness failsafe (#25) — done.** Distinct from the
      target staleness above: serving `offset + stale_measured` is meaningless — a
      frozen value no longer tracks the draw — so a stale **measured** input abandons
      the closed loop and falls back to **full charge** (`reported = 0`) within
      `MEASURED_TIMEOUT_SECONDS`. Same static baseline as the target failsafe, never a
      pause (§1); fuse protection is out of scope, so there is nothing to protect by
      cutting off.
- [ ] **Mid-charge meter-loss latch:** confirm whether a meter dropout *during an
      active charge* latches the red fault (needs power-cycle) or clears soft like
      the idle case (§7 / failsafe above).
- [ ] **Permanent install:** fixed Waveshare config (9600 8E1, transparent mode,
      static IP / DHCP reservation), LAN/PoE drop at the wallbox, strain-relieved
      CN20 tap.

**Fallback if emulation proves unreliable:** **External Enable Input** (DIP pin 2)
+ a Shelly/relay → pure on/off time-scheduling from HA, no Modbus at all. Coarser
but bulletproof.

---

## 10. Deployment model

Ship like a normal app, **not** a thin image wrapper:

- This repo owns the code, tests, and **CI that builds a container image and
  pushes it to GHCR** (e.g. `ghcr.io/<owner>/evc04-charge:vX.Y.Z`).
- The consuming infra (a separate Ansible/k8s repo) only **pins an image tag** and
  deploys manifests + the env config above. No application logic lives there.
- The service is **generic**: every site-specific value (gateway address, the box's
  current ceiling, MQTT broker/topics) is an env var. Nothing about any particular
  home is hard-coded.

---

## 11. Quick reference

```
Bus:        9600 8E1, transparent TCP↔RS485 gateway
Poll:       addr 1, FC 0x03, start 0x500C, qty 6, ~1.006 s, content-agnostic
Payload:    struct.pack('>fff', L1_A, L2_A, L3_A)   # Inepro PRO380, 3× float32 ABCD
Control:    CLOSED-LOOP: box loops on total measured current (incl. own draw).
            reported_A = clamp(offset + measured_A)   (per phase, offset soft-ramped)
            offset = MAX_BOX_AMPERE − target_A
            target ≥ MAX_BOX_AMPERE → offset 0 → box holds total at the limit (full)
            target < MIN_CHARGE_AMPERE (~6A) → hard pause
            Static feed = on/off ONLY (proven). Closed loop modulates: DIP 16A
            stable ~9–15A; 3φ floor ~6A.
            Both failsafes (target-stale, measured-stale) → full charge (report 0).
Poll frame: 01 03 50 0c 00 06 14 cb
Examples:   0A→01 03 0c 00000000×3 93 70  |  16A→…41800000×3 97 ae  |  63A→…427c0000×3 13 97
```

---

## 12. Combined ESP32 device (read + control) — exploratory track

Decision #65 proposes collapsing both EVC04 sub-systems — CN28 LOG **read**
(telemetry) and the PRO380 meter-emulation **control** path — onto a single
classic ESP32 (Xtensa) inside the box, by porting the Rust control core onto the
MCU rather than running stock ESPHome. This drops the TCP↔RS485 gateway and the
k8s dependency for control. **It is additive:** the production daemon at the repo
root (§1–§11) stays the control path until the port is proven on real hardware.

### Vorprojekt: CN28 remote prober (#66)

The lowest-risk first step establishes the native ESP32 Rust footing (the `core` +
`firmware` split) while doing real work — resolving the open CN28 LOG protocol
mysteries. It is a **protocol-discovery tool, read/explore only**: no RS485, no
control, no safety criticality.

CN28 is strictly request/response (15 s of silence yields 0 bytes; any byte on
Box-RX triggers exactly one ASCII response frame), so discovery means *actively
sending bytes* — ideally remotely over MQTT, without reflashing.

Two independent crates (no root workspace — the daemon is untouched):

- **`/core`** (`evc04-cn28-core`, `no_std` + `alloc`, host-tested on stable) —
  `command::decode_command` turns an MQTT payload into raw CN28 bytes (escapes
  `\\ \r \n \t \0 \xHH`); `dump::to_hex` / `dump::to_printable` render responses.
- **`/firmware`** (`evc04-cn28-prober`, `esp-idf-svc`, target
  `xtensa-esp32-espidf`, built/flashed **locally, not in CI**) — WiFi + MQTT + one
  hardware UART (UART1, 9600 8N1; UART0 stays free for the log monitor).

MQTT contract:

```
evc04/cn28/cmd        (in)  command payload → decode_command → bytes written to CN28
evc04/cn28/baud       (in)  integer UART rate → live change_baudrate (baud sweep, #79)
evc04/cn28/raw        (out) raw response bytes
evc04/cn28/raw/hex    (out) lowercase space-separated hex
evc04/cn28/raw/ascii  (out) printable ASCII, non-printables → '.'
evc04/cn28/status     (out) LWT online/offline (retained); also non-retained `baud <n>` echoes
```

The `cn28/*` topics above are scoped to the Vorprojekt prober. **OTA lives in its
own durable `device/*` namespace** (#76) because it outlives the prober — it stays
in use whatever firmware role this ESP takes later:

```
evc04/device/ota          (in)  http:// firmware URL → pull + flash inactive slot
evc04/device/ota/status   (out) non-retained progress: downloading | ok | failed <e>
```

Wiring (AZ-Delivery **ESP32 DevKit C V4**, 38-pin WROOM-32; onboard USB-UART +
3.3 V regulator + auto-program, so the first flash needs no buttons and it boots
straight into the app on power-up — the prerequisite for OTA-only updates, #76).
UART1 → CN28. The CN28 "LOG" header is 4-pin **3.3 V TTL**, pinout bottom→top
`GND · RX · TX · 3.3V` — same level as the ESP, so wire it **directly, no level
shifter**. All three signal wires are on the ESP's right-hand header:

```
ESP GPIO16 (UART1 TX)  ──►  CN28 RX   (pin 2)
ESP GPIO17 (UART1 RX)  ◄──  CN28 TX   (pin 3)
ESP GND                ───  CN28 GND  (pin 1, common ground)
```

Bench bring-up (#72) proved the TX/RX roles **opposite** to the first guess: the
working assignment is **GPIO16 = TX, GPIO17 = RX** (firmware drives UART1 this way),
not the `TX2`/`RX2` silkscreen default — anchor on these GPIO numbers, not the
silk. Leave the CN28 3.3 V pin
(pin 4) unconnected — the DevKitC is self-powered (USB/VIN) and feeding it would
fight the onboard rail. Do **not** wire `GPIO0`/BOOT (strapping pin, owned by the
onboard button) or `TX0`/`RX0` (the USB console — the first-flash + monitor path
OTA later replaces).

Bring-up status (#72): LOG read **fully working** (2026-06-27). The two earlier
open items on the link itself are resolved:

1. **RX/TX were swapped.** With the first GPIO17=TX/GPIO16=RX assignment the link
   read **zero bytes**; flipping the firmware to **GPIO16=TX / GPIO17=RX** restored
   it. The box's `TX2`/`RX2` silk maps opposite to the first guess.
2. **Baud is 9600, not 115200.** At 115200 the response was a run of `0x00`; a
   sweep over `evc04/cn28/baud` found **9600 8N1** gives clean ASCII (the box's LOG
   console runs at the same rate as its RS485 meter side). `CN28_BAUD` is now 9600.

With both fixed the LOG streams readable lines, e.g. `A: 18  W: 0  Wh: 0`,
`KLEFR NOT DETECTED!`, `Any metering device NOT detected!`, `No data received from
P1!` — the last lines are a live confirmation that the box is **not** reading its
meter yet, the same symptom as the climbing `last_poll_age_s` on the RS485 side.

Still open: the board flaps `online`/`offline` under load (LWT), pointing at a
brownout / marginal 5 V supply or GND.

Build: run `firmware/bootstrap.sh` once (system deps + `espup` + cargo tools + the
libxml2/ICU compat shim esp-clang needs on rolling distros), then `cargo make
build` and `cargo make flash` on the host. `WIFI_SSID`/`WIFI_PASSWORD`/`MQTT_URL`
are baked in at build time (env!, never committed); export real values before
flashing.

OTA (#76): once sealed in the enclosure the board is never wired to USB again, so
new firmware rolls out over WiFi. `firmware/partitions.csv` gives the ESP32 two
app slots (`ota_0`/`ota_1`, no `factory`; otadata picks the bootable one) and
`sdkconfig.defaults` enables `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`. Publishing a
`http://` URL to `evc04/device/ota` streams the `.bin` into the inactive slot and
reboots into it; the new image boots *pending-verify* and only cancels its
rollback once it has re-reached WiFi **and** the broker (`confirm_running_slot` on
the first CONNECTED), so an image that can't get online auto-reverts on the next
reset. Transport is **plain HTTP on the trusted LAN** — `firmware/ota_push.sh`
builds the release image, serves it from a *temporary* local HTTP server, triggers
the pull, waits for `ok`/`failed` on `evc04/device/ota/status`, then shuts the
server down (no permanent hosting on the broker box). **Image signing is
deferred** ("rollback now, sign later"): rollback guards a *broken* image today;
signing (a build-config change, not an eFuse burn, so it can ship in a later OTA
without re-opening the box) is what will later guard a *malicious* one served by
another LAN host.

The **structured CN28 parser is deliberately deferred** — it is the next step once
captures from this prober confirm the frame format (the `wc` fragment, the shell
command surface, the `S:` pilot-state and `ERROR:` codes are still open).
