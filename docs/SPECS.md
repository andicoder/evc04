# evc04 — Specification

A complete, self-contained brief for the on-box firmware. No external context is
required: everything the EVC04 does on the wire, the control math, the firmware
behaviour, and the open hardware questions are below.

---

## At a glance

The whole system in one picture — the closed control/data loop and the physical
wiring. Labels are the real MQTT topics (§8) so the diagram doubles as a map into
the rest of this spec. No new claims here; details and evidence live in §2–§9.

```
                   CONTROL / DATA FLOW  —  the closed loop
                   ═════════════════════════════════════════

  ┌────────────────────────────────────────────────────────┐
  │ Home Assistant / evcc              (the "brain")        │
  │ day-ahead price | PV surplus | departure planning       │
  └────────────────────────────┬───────────────────────────┘
                     target +   │
                     grid-power │  (heartbeat)
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ MQTT broker                                             │
  │   in   evc04/charge/target      desired current (A)     │
  │   in   evc04/charge/grid_power  signed grid power (W)   │
  │   out  evc04/charge/status      liveness (retained)     │
  └────────────────────────────┬───────────────────────────┘
                               │
                               ▼
  ┌────────────────────────────────────────────────────────┐
  │ ESP32 inside the box   (`core` control + `firmware`)    │
  │   UART2 → RS485 slave  ==  emulated Inepro PRO380        │
  │   V4 grant tracking: regulate the box's own lb_current   │
  │   report MAX+shed / MAX / MAX−headroom   (§6)            │
  │   UART1 → CN28 LOG: read lb_current + car draw (feedback)│
  └──────────┬──────────────────────────────▲───────────────┘
     RS485    │                              │ CN28 LOG
     9600 8E1 ▼                              │ 9600 8N1
  ┌────────────────────────────────────────────────────────┐
  │ EVC04 wallbox  ·  Power Optimizer                       │
  │   polls the emulated meter @ ~1 Hz on CN20              │
  │   own closed loop: grants the car lb_current, ramps to  │
  │   hold total main-line current = MAX_BOX_AMPERE (DIP)   │
  └────────────────────────────┬───────────────────────────┘
                     delivers   │
                       charge   ▼
                           ┌─────────┐
                           │   car   │
                           └─────────┘

   feedback that closes the loop (V4, #135): the box grants the car a current
   (`lb_current`) and reports it on the CN28 LOG; the ESP reads that grant and
   nudges the meter answer so the box sheds toward, holds at, or is handed
   headroom for `target`.

   legend — the two inbound values:
     desired current  (evc04/charge/target)      setpoint: how fast you WANT to charge,
                                                  from the brain (price / PV / departure)
     grid power       (evc04/charge/grid_power)  signed watts, raw pass-through; its
                                                  *cadence* is the liveness heartbeat —
                                                  NOT part of the modulation math (#136)


                   PHYSICAL WIRING
                   ═══════════════════

   ESP32 DevKitC V4   (sealed inside the wallbox enclosure)
   ┌──────────────────────────────────────────────────────────┐
   │ UART1  GPIO16 TX / GPIO17 RX  ── 3.3 V TTL, direct ──►  CN28 "LOG" (read)
   │        9600 8N1  (no level shifter)                        │
   │                                                            │
   │ UART2  GPIO25 TX / GPIO26 RX ─► TTL485 v2 ─► A/B/GND ──► CN20 (RS485)
   │        9600 8E1  auto-direction (no DE line)               │  the meter bus
   │                                                            │
   │ WiFi ──────── MQTT broker ──────── HA / evcc               │
   └──────────────────────────────────────────────────────────┘
        CN20 silk:  V │ GND │ A │ B      DIP 4-5-6 = MAX_BOX_AMPERE (16 A)
        (leave V unconnected)            the box's current ceiling

   Full pin assignment and transceiver wiring: `esp32-pinout.md`.


              SOFTWARE COMPONENTS  —  two threads, one lock-free hand-off
              ══════════════════════════════════════════════════════════

  The firmware runs two worker threads that share only a lock-free `Handoff`
  (two atomics), so the box's ~1 Hz meter poll is always answered no matter
  what the control side is doing (§7).

  ══════════════════════════ MQTT broker ══════════════════════════
              │  target ▼  grid_power ▼  enable ▼         status ▲ (retained)
              ▼
  ┌────────────────────────────────────────────────────────────┐
  │ THREAD 1 · prober   (UART1 CN28 LOG + MQTT + control tick) │
  │   read CN28 LOG → lb_current + car draw (the V4 feedback)  │
  │   MQTT intake: target / grid_power / enable / probe_over   │
  │   ~1 Hz Controller.tick() → core grant_tracking_current    │
  │   publish retained evc04/charge/status · MQTT-triggered OTA │
  └───────────────┬──────────────────────────────▲────────────┘
     reported (f32)│         Handoff (2 atomics)   │ last_poll ms
                   ▼                               │
  ┌────────────────────────────────────────────────────────────┐
  │ THREAD 2 · rs485    (UART2 PRO380 Modbus-RTU slave)        │
  │   answer 0x500C×6 poll · reported×3 (float32 ABCD) + CRC16 │
  │   stamp each poll's timestamp for the status liveness      │
  └────────────────────┬───────────────────────────────────────┘
                       │  raw RTU frames over UART2 + TTL485 v2
                       ▼
  ═══════════════════════════ EVC04  (CN20) ═══════════════════════

  `main()` owns the WiFi guard and a 60 s task watchdog; if the prober loop ever
  returns it reboots to re-run bring-up. The RS485 slave keeps answering even
  while the prober is busy — a silent meter hard-faults the box (§9).
```

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
because the box also reports its own per-car grant (`lb_current`) on the CN28 LOG,
the firmware regulates that grant directly and the box **modulates** (proven on
hardware, §6). The charging *brain* (price / PV / departure planning) stays in the
external controller — Home Assistant or **evcc** (§8) — never in the firmware,
which is a mode-agnostic actuator: `target` in, meter emulation out.

**The firmware is a throttle-only overlay.** The box's baseline — no meter, or the
Power Optimizer disabled — is **full charge (11 kW)**. We emulate the meter *only
to charge less* than that baseline, for PV surplus / price optimisation / load
distribution. **Protecting the building fuse is explicitly out of scope** — that is
the job of the installation and the DIP-set limit. On any control-layer failure the
firmware **pauses** (§9): for an evcc/HA-managed box a control-path blip must *stop*
charging, not start it at the worst time (#52). The controller's liveness is carried
by the **grid-power heartbeat** — more than 15 s of silence pauses the box (#136).

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

The controller is the **ESP32 inside the wallbox** (§7), so it drives the RS485
bus **directly** — no TCP↔RS485 gateway, no host in the loop. It taps two of the
box's internal ports:

- **CN20 — the RS485 meter bus** (Modbus RTU, **9600 8E1**; even parity is
  mandatory — the Inepro/EVC04 bus uses it). A **TTL485 v2 auto-direction
  transceiver** on UART2 (GPIO25 TX / GPIO26 RX) bridges the ESP's TTL UART to the
  differential A/B pair; the module flips bus direction in hardware, so there is no
  DE line to drive. Silkscreen `V | GND | A | B` — the `V` pin is left unconnected.
- **CN28 — the "LOG" header** (plain **3.3 V TTL UART, 9600 8N1**), wired direct to
  UART1 (GPIO16 TX / GPIO17 RX), no transceiver. This read-only console reports the
  box's own delivered current and per-car grant (`lb_current`) — the V4 control
  feedback (§6).

Both run at 9600 baud on independent UART controllers (different parity: 8E1 vs
8N1). Full pin assignment, transceiver wiring, and the ESP32 gotchas are in
[`esp32-pinout.md`](esp32-pinout.md).

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

`measured_current` is a **live** per-phase current published over MQTT (this was
the original loop, since superseded by V4 below). Now the box sees its own draw
climb, the loop settles, and the
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

> **Implementation status.** The closed-loop offset+measured model above was the
> original control (`reported = clamp(soft_ramped_offset + measured)`, min-charge
> cutoff, staleness failsafes; #22–#25). The on-box firmware has since **replaced it
> with the V4 grant-tracking loop** below (#135), which regulates the box's own
> `lb_current` directly and drops the grid feed from the modulation math. Both live
> in `core`; the firmware links only V4. The status topic still exposes the
> approximated evcc `charge_state` and there is an evcc charger template
> ([`evcc.md`](evcc.md), #28).

### On-box floor-seek: layered integral trim (#119, core + firmware only)

The `9–15 A` band above is set by the ~3–6 s measurement round-trip. Because the
on-box firmware also reads the box's own delivered current from the CN28 LOG, an
earlier iteration added a **layered integral trim** on top of the proven loop to
push below that band toward the box's real minimum:

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
| at `MAX` … `MAX + 0.5` | hold (dead zone; #57's "at the ceiling holds") — **only while the car draws** (see below) |
| `MAX + 0.5` … cut | shed `floor(excess)` A — **proportional**: +1.0/+1.5 → −1 A per ~6 s, +2.0 → −2 A per ~6 s, ridden live from 16 A down to 6 A with no cut |
| above the cut threshold (> +2, ≤ +4) | hard cut (session drop; pause reports `MAX + PAUSE_MARGIN_AMPERE` = +4) |

Session start grants the apparent headroom outright: `lb ← MAX − reported`.

Three more rules, measured on the flag-day cutover 2026-07-03
(`2026-07-03-flagday-start-cut.log` + the staircase capture): **at/over the
ceiling with an idle car (< ~1 A) the box withdraws the PWM within one eval** —
the dead-zone hold only exists while the car draws, and it degrades the grant
(observed 16→10) even at ~5 A, i.e. below the pilot minimum; **after any cut it
refuses a new session for ~30 s** (with little apparent headroom it took minutes
to re-engage); and **a shed step of ≥2 A that lands at/below the 6 A pilot
minimum drops the session** instead of shedding (lb 8 at reported +2 → cut while
the car drew 6.2 A — the measured −1 A ride down *to* 6 from 2026-07-02 did not
cut). A real car also needs 10–30 s of standing offer before it draws anything
("contactor lag"), so a controller that reports the ceiling the moment the box
grants produces an endless ~40 s grant/cut cycle and the car shows "Ladegerät
nicht bereit".

**Consequence — the V4 controller** (`lb_tracking_report`, `core`): regulate the
grant directly on the ~5 s CN28 `lb_current` feedback and stop stacking
offset/measured/trim. Grant above target → report `MAX + clamp(err, 1, 2)` (the box
sheds it proportionally); at target (±1 A) → report exactly `MAX` (holds); below
target → report the deficit as headroom (`MAX − (target − lb)`), which also covers
the start-grant (`lb = 0` → grant = target). **Ramp pin**: while the car draws
less than the 6 A pilot minimum (max phase current from the CN28 metering) the
report is `MAX − target + car` instead — per the box's grant law that holds
`lb = target` through the contactor lag *and* the 0→6 A ramp; any ceiling report
before the car draws properly triggers the idle-car cut/degrade above. **Shed
floor**: the over-report is additionally capped at `lb − (MIN_CHARGE + 1)`, so
the box is never told to shed into its pilot floor — target 6 deliberately
settles at 7 A (inside the ±1 A acceptance) instead of risking the ≥2 A-step
cut. Proven in the simulator across the full 6–16 A staircase, under deep PV
export, for eval periods 5–10 s and car start lags 0/15 s
(`core/tests/replay.rs`); the box-side dynamics are pinned by the 2026-07-02
probe and the 2026-07-03 flag-day captures. **Live-proven 2026-07-03**
(`2026-07-03-flagday-staircase-pass.log`): the full staircase passed on hardware
— every stage settled within ±1 A (16→15, 12→11, 10→9, 8→7, 6→7, 8→7, 16→15,
grants are whole amps and land one below on down-steps), zero session cuts after
charge start. Grid `measured` is no longer part of the modulation math — which
also retires the H2 export-clamp failure mode by construction (#136).

---

## 7. The on-box firmware

The controller is a classic **ESP32 (Xtensa) sealed inside the wallbox** running
`esp-idf` Rust (`firmware/`), linking the host-tested control brain (`core/`). It
drives the RS485 meter bus directly (§3) — no gateway, no container, no k8s.

### Two threads, one lock-free hand-off

`main()` wires the hardware and launches two worker threads that share only a
lock-free `Handoff` (two atomics), so the box's ~1 Hz meter poll is answered no
matter what the control side is doing:

1. **Prober thread** (UART1 CN28 LOG). Reads the box's own delivered current and
   per-car grant (`lb_current`) off the LOG console — the V4 feedback — and runs
   the MQTT intake, the ~1 Hz control tick, the retained status publish, and
   MQTT-triggered OTA.
2. **RS485 slave thread** (UART2 + TTL485 v2). Parses incoming RTU frames; for the
   `addr 1 / FC03 / start 0x500C / qty 6` poll it answers the 12-byte `>fff`
   payload (L1/L2/L3 reported current) + Modbus CRC16, and stamps each poll's
   timestamp. Anything else is ignored / CRC-validated.

The `Handoff` carries exactly two scalars: `reported` (worker → slave, the current
to answer with) and `last_poll` (slave → worker, for the status liveness). The slave
never blocks on the worker, and it is constructed **paused** (`reported =
MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE`) so it serves a safe value before the first
tick. `main()` also owns the WiFi guard, a 60 s **task watchdog**, and a status-LED
thread; if the prober loop ever returns, it **reboots** to re-run bring-up.

### Control — the V4 grant loop

Each tick computes the reported current with `core`'s `grant_tracking_current`
(§6): regulate the box's own `lb_current` directly — report `MAX + shed` to bleed
an over-grant down, `MAX` to hold at target, `MAX − headroom` (and the ramp-pin
posture through the contactor lag) to hand it up. The knobs are **compile-time
constants**, matched to this install's DIP setting:

| Constant | Value | Meaning |
|---|---|---|
| `MAX_BOX_AMPERE` | 16 | the DIP 4-5-6 ceiling (§2) — must equal the physical DIP |
| `MIN_CHARGE_AMPERE` | 6 | below this the 3φ floor can't hold — hard pause |
| `PAUSE_MARGIN_AMPERE` | 4 | amps **over** the ceiling a pause reports so an active charge actually cuts (#57) |
| `LB_TRACKING_MAX_OVER_AMPERE` | 2 | cap on the over-report — the box's strongest shed rate, clear of its cut threshold |
| `GRID_TIMEOUT` / `CN28_TIMEOUT` | 15 s | staleness windows for the two failsafes below |

A **measurement probe** (`evc04/charge/probe_over`) can lift the served answer to
`MAX + over` (0 … 3.5 A, auto-expiring after 60 s) to characterise the box's cut
threshold on hardware without touching the command state — `charge_state` stays
derived from the un-probed value.

### Failsafes — everything pauses

Unlike the retired daemon's configurable direction, the firmware's failsafes are
**pause-only** — the safe direction for an evcc/HA-managed box (§9, #52):

- **Grid heartbeat stale** (`grid_power` silent > 15 s) → pause. The controller's
  liveness is the heartbeat's *cadence*, not its watts (#136).
- **CN28 feedback stale** (> 15 s) → pause; the V4 regulation is blind without it.
- **`enable = false`** → hard pause, independent of the target.
- **Cold start** (no target yet) → pause; the `Handoff` starts paused and a
  first-ever boot never defaults to charging (#59).

The **target is a latched setpoint** — it never ages out. evcc's MQTT charger
publishes the current on-change and then holds it, so aging the target would
deadlock (box forgets → pauses → evcc never re-sends). The grid heartbeat carries
the "controller alive" check instead.

### Persistence

The `target`/`enable` topics are non-retained, so the last commanded setpoint is
persisted to **NVS** (namespace `charge`, written only on change → negligible flash
wear) and restored on boot. Without it an OTA/reboot would cold-start paused until
evcc's next change and the car would not resume. WiFi calibration uses its own NVS
namespace; the two never collide.

### Configuration

No config files, no env vars at runtime. `WIFI_SSID` / `WIFI_PASSWORD` / `MQTT_URL`
are **baked in at build time** (`env!`, never committed); everything else is a
compile-time constant. The build is per-install, not a generic image.

### Continuous availability

A **silent meter is dangerous**: with the Power Optimizer enabled the box
**hard-faults to a solid red LED** when the meter stops answering (§9) — it does
*not* fall back to full charge. So the RS485 slave must never wedge: it runs on its
own thread, the task watchdog reboots a hung chip, and an OTA rollout must **overlap**
(the new image answers polls before the old stops).

### Build & flash

Local only — the Espressif Xtensa toolchain and the device dependency keep
`firmware/` out of CI:

```
firmware/bootstrap.sh              # once: sysdeps + espup + cargo tools + esp-clang shim
export WIFI_SSID=… WIFI_PASSWORD=… MQTT_URL=mqtt://user:pass@host:1883
cd firmware && cargo make build    # native esp build → host ELF
cargo make flash                   # flash + USB monitor (first flash only)
```

### OTA

Once sealed in the enclosure the board is never wired to USB again; new firmware
rolls out over WiFi in a **durable `device/*` namespace** (outlives any one role):

```
evc04/device/ota          (in)  http:// firmware URL → pull + flash the inactive slot
evc04/device/ota/status   (out) non-retained progress: downloading | ok | failed <e>
```

`firmware/partitions.csv` gives the ESP32 two app slots (`ota_0`/`ota_1`, no
`factory`; otadata picks the bootable one) and `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`
is set. A pushed image boots **pending-verify** and only cancels its rollback once it
has re-reached WiFi **and** the broker (on the first CONNECTED), so an image that
can't get online auto-reverts on the next reset. `firmware/ota_push.sh` serves the
`.bin` from a *temporary* local HTTP server, triggers the pull, waits for
`ok`/`failed`, then shuts the server down. Transport is **plain HTTP on the trusted
LAN**; image **signing is deferred** ("rollback now, sign later" — a later build-config
change, not an eFuse burn, so it can ship in an OTA without re-opening the box).

### Origin

The control logic was proven first as a **k3s daemon** (RS485 over a Waveshare
TCP↔RS485 gateway; retired) and then ported `no_std` into `core/` so the firmware
could link it unchanged. The `core` + `firmware` split itself was bootstrapped by
the **CN28 remote-prober Vorprojekt** (#66) — a read-only protocol-discovery tool
that established the native ESP32 Rust footing while resolving the CN28 LOG format.
The verified frames in §5/§10 keep the wire protocol a fixed target.

---

## 8. MQTT contract

**The full, authoritative contract lives in [`mqtt.md`](mqtt.md).** All payloads are
UTF-8 JSON; QoS 1; the status topic is retained. Topics are device-scoped under
`evc04/charge/*`. Summary:

- **Inbound — target** (`evc04/charge/target`): `{ "ampere": N }`, the desired charge
  current — a **latched setpoint** (never ages out; evcc holds it on-change).
  Out-of-range is clamped; invalid payloads are ignored and the last good value held,
  surfaced in `last_error`.
- **Inbound — grid_power** (`evc04/charge/grid_power`): `{ "watt": N }`, the raw
  **signed** grid power (negative = export), forwarded untouched — no W→A math in the
  firmware. It is **not** part of the modulation math (V4 regulates on the box's own
  grant, §6); only its **cadence** is the controller liveness heartbeat (> 15 s →
  pause, #136). The watts are a status diagnostic.
- **Inbound — enable** (`evc04/charge/enable`): `{ "enable": bool }`, a hard-pause
  gate independent of the target (#60).
- **Inbound — probe_over** (`evc04/charge/probe_over`): `{ "ampere": N }`, the
  measurement-probe lift (§7); 0 clears.
- **Outbound — status** (`evc04/charge/status`, retained, + offline LWT): `online`,
  `target_ampere`, `reported_ampere`, `grid_power_w`, `grid_age_s`, `grid_failsafe`,
  `last_poll_age_s`, `charge_state`, `enabled`, `last_error`, `lb_current_ampere`
  (the box's grant — the V4 feedback), `cn28_feedback_stale`, and `probe_over_ampere`.
  `charge_state` is the approximated evcc `B`/`C` state (#28; `A` is never asserted —
  the emulation has no control-pilot line).

**The brain is evcc** (#28): the firmware is a mode-agnostic actuator, driven as an
**evcc custom charger** — `maxcurrent` → target, `enable` → the enable gate, `status`
← our `charge_state`. evcc's control interval must exceed the box's ~5 s grant-eval
cadence (§6) or the two loops hunt. The working charger template, min/max-current
band, and nested-loop timing live in [`evcc.md`](evcc.md). A HA-only setup (number
entity → target, sensor ← status) also works for simple on/off + manual current.

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
  - **Design consequence — two failsafe layers; the firmware always pauses:**
    1. **Control input stale, slave still answering** (broker down, controller
       offline, cold start past the grace window): keep answering, but **pause**
       (report `MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE` → box stops, #57) — the safe
       direction for an **evcc/HA-managed** box, where a control-path blip (e.g. a
       nightly router reconnect) must **not** flip an intended pause into charging
       overnight. The target is a **latched** setpoint (evcc's idle cadence is
       decision-driven and unbounded, so a target timeout would deadlock); liveness
       is carried by the **grid-power heartbeat** instead — > 15 s silent pauses, as
       does a stale CN28 grant feed (> 15 s, blind regulation) or `enable = false`
       (#52/#136). The retired daemon made this direction configurable
       (`full_charge`/`hold_last` for an unmanaged box); the firmware is pause-only
       by design.
    2. **Process dead, slave silent** (crash): with the Power Optimizer enabled the
       box **hard-faults to red — it does *not* fall back to full charge**. This
       layer is unreachable from inside the process, so the firmware **auto-reboots**
       (60 s task watchdog) and OTA rollouts must **overlap** (§7). The box's own
       **Failsafe Current** (control-interface reg `2000`, SW variants only) or the
       **External Enable** input (DIP pin 2 + relay) is an optional independent
       backstop against total loss.
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
- [x] **Input-staleness failsafe (#25) — done.** The V4 firmware pauses on any stale
      input: a silent **grid-power heartbeat** (> 15 s) means the controller is gone
      and the latched target would otherwise charge forever, and a stale **CN28 grant
      feed** (> 15 s) means the regulation is blind — both pause (report above the
      ceiling), never charge on. (The retired daemon instead fell back to full charge
      on a stale *measured* input; the pause-only firmware is the safer default for an
      evcc/HA-managed box, §1.)
- [ ] **Mid-charge meter-loss latch:** confirm whether a meter dropout *during an
      active charge* latches the red fault (needs power-cycle) or clears soft like
      the idle case (§7 / failsafe above).
- [ ] **Permanent install:** ESP32 sealed inside the enclosure with a stable power
      feed, WiFi provisioned (SSID/creds baked at build, §7), and strain-relieved
      CN20 (RS485) and CN28 (LOG) taps. No external gateway.

**Fallback if emulation proves unreliable:** **External Enable Input** (DIP pin 2)
+ a Shelly/relay → pure on/off time-scheduling from HA, no Modbus at all. Coarser
but bulletproof.

---

## 10. Quick reference

```
Bus:        9600 8E1, ESP32 UART2 → TTL485 v2 → CN20 (direct, no gateway)
Poll:       addr 1, FC 0x03, start 0x500C, qty 6, ~1.006 s, content-agnostic
Payload:    struct.pack('>fff', L1_A, L2_A, L3_A)   # Inepro PRO380, 3× float32 ABCD
Control:    V4 GRANT-TRACKING: regulate the box's own lb_current (CN28 LOG feedback).
            grant > target → report MAX + clamp(err,1,2)  (box sheds proportionally)
            grant ≈ target (±1A) → report MAX             (box holds)
            grant < target → report MAX − headroom        (box ramps up; covers start)
            target < MIN_CHARGE_AMPERE (~6A) → hard pause (report MAX + PAUSE_MARGIN)
            DIP 16A; 3φ floor ~6A; grid_power is a liveness heartbeat only (#136).
            All failsafes (grid-stale, cn28-stale, enable=false, cold start) → pause.
Poll frame: 01 03 50 0c 00 06 14 cb
Examples:   0A→01 03 0c 00000000×3 93 70  |  16A→…41800000×3 97 ae  |  63A→…427c0000×3 13 97
```
