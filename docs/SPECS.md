# evc04 — Specification

A complete, self-contained brief for the on-box firmware. No external context is
required: everything the EVC04 does on the wire, the control math, the firmware
behaviour, and the open hardware questions are below.

---

## At a glance

The whole system in one picture — the closed control/data loop and the physical
wiring. Labels are the real MQTT topics (§8) so the diagram doubles as a map into
the rest of this spec. No new claims here; details and evidence live in §2–§8.

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
  while the prober is busy — a silent meter hard-faults the box (§7).
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
firmware **pauses** (§7): for an evcc/HA-managed box a control-path blip must *stop*
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

The DIP-set limit must equal `MAX_BOX_AMPERE` so the §6 control math lands the edge
where expected. The DIP value trades full-charge headroom against modulation range:
a **low** limit (16 A, tested) maps the closed loop onto the car's real
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

### A static feed is on/off only

A **static** meter value can't land the box anywhere in between: a fabricated constant
never rises as the car draws, so the box ramps to full while `reported <
MAX_BOX_AMPERE` and cuts off once `reported ≥ MAX_BOX_AMPERE`, with only a 1–2 A
transition zone at the edge — **not** a usable proportional band (confirmed on a car
at both DIP 65 A and DIP 16 A). Proportional control therefore has to ride the box's
*own* grant loop, not a static feed (below).

> **Pause must exceed the ceiling (#57).** To *cut* an active charge the report must
> **exceed** the limit, not merely reach it: at `reported = MAX_BOX_AMPERE` the box
> holds the charge; at `MAX_BOX_AMPERE + 2..4 A` it cuts (grid flipped import→export on
> hardware). So a hard pause reports `MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE` (§7), and
> `charge_state` treats only that full pause level (`reported ≥ MAX_BOX_AMPERE +
> PAUSE_MARGIN_AMPERE`) as paused — a V4 shed report (`MAX+1..MAX+2`) is live
> modulation and stays `C`; flashing `B` mid-shed zeroes evcc's charge-power
> estimate and rattles its PV loop (live 2026-07-05).

### Mode selection lives in the controller, not here

The firmware is **mode-agnostic**: it consumes `target` and nothing else. The
controller (Home Assistant / evcc, §8) picks behaviour purely by the target it
publishes:

- **full charge** (cheap price window) → `target ≥ MAX_BOX_AMPERE` → report the
  ceiling → the box holds the *total* at `MAX_BOX_AMPERE` (fuse protection is the box's
  job, not ours, §1).
- **modulate** (PV surplus) → `target` = surplus-derived → tracked via the grant loop
  below.
- **pause** → `target < MIN_CHARGE_AMPERE` (~6 A, the 3-phase floor the box can't hold
  a stable current below) → hard pause.

`MAX_BOX_AMPERE` must match the DIP setting (§2) for the math to land where expected;
the DIP value itself trades full-charge headroom against modulation range (§2).

### The V4 grant loop (shipped) — regulate the box's own grant (#134/#135)

The box's grant dynamics, measured on hardware 2026-07-02/03 and refitted from
the 2026-07-05 characterization campaign (fixtures
`core/tests/fixtures/sessions/`, model `core/src/charge/boxsim.rs`). The box
runs **one grant law on two clocks**, recomputing its grant to the car
(`lb_current` in the CN28 LOG) from the meter value it polls:

```
lb ← round(car_draw + (MAX − reported))          clamped to [0, MAX]
```

— i.e. the *signed* apparent headroom added on top of the live draw. **Downward
moves and cut checks run on a fast ~4–6 s clock; session starts and upward
moves on a slow ~30 s clock** (rep-0 wire: re-offer/up-grant gaps 28.2 s;
engage latencies 17–66 s; post-cut re-engage 28–32 s on the flag-day capture —
the "~30 s cut cooldown" is this same clock, reset by the cut). Everything
previously modelled as separate branches falls out of the law:

- *dead zone*: `round(MAX + 0.5 − reported at MAX+0.5) = MAX` — pure rounding;
- *proportional shed*: with the car tracking its grant down, each fast eval
  computes `round(car − excess)` = −1 A at +1.0/+1.5, −2 A at +2.0;
- *idle-car cut*: car 0 at the ceiling computes grant 0;
- *pilot-floor cut*: **a computed grant below the 6 A pilot minimum drops the
  session** — the flag-day staircase cut (lb 8, reported +2, car ~6 → grant 4)
  *and* the end of the 2026-07-02 probe +1.0 ride (reached 6, next eval
  computes 5 → "session drop + self-recovery" per the fixture);
- a pause report (`MAX + PAUSE_MARGIN_AMPERE` = +4, at/above the > +2 cut
  threshold) cuts within one fast eval.

**Session start** is gated, not immediate: on each slow-clock eval the box
opens only when the opening grant `ceil(MAX − reported)` **exceeds `MAX/2`**
(campaign sweeps: eleven refusals at ≤ 8.0 A headroom over 5–6 min each;
engages at ≥ 8.4 A, opening grants all `ceil`). Right after **meter silence**
(reboot/OTA of the emulated meter) the threshold drops to the pilot floor —
the 2026-07-02 sessions opened at ~6.5 A headroom, the 2026-07-05 post-OTA
morning session at ~6 A. Consequence: a steady-state target below ~8 A can
*continue* a session but never *open* one (the controller works around this
with the **cold-start kick**, below). A real car also needs 10–30 s of
standing offer before it draws anything ("contactor lag"), so a controller
that reports the ceiling the moment the box grants produces an endless ~40 s
grant/cut cycle and the car shows "Ladegerät nicht bereit".

**Consequence — the V4 controller** (`lb_tracking_report`, `core`): regulate the
grant directly on the ~5 s CN28 `lb_current` feedback and stop stacking
offset/measured/trim. Grant above target → report `MAX + clamp(err, 1, 2)` (the box
sheds it proportionally); at target (±1 A) → report exactly `MAX` (holds); below
target → report the deficit as headroom (`MAX − (target − lb)`), which also covers
the start-grant (`lb = 0` → grant = target). **Ramp pin**: once a session exists
(`lb > 0`) and while the car draws less than the **target** (max phase current
from the CN28 metering) the report is `MAX − target + floor(car)` instead — per
the box's grant law that holds `lb = target` through the contactor lag *and*
the whole ramp. Any ceiling report before the car has reached the target makes
the box shed the grant back toward the live draw (flag-day: cut at car 0,
degrade 16→10 at car ~5; replay vs the campaign-fitted box: a pin released at
the 6 A pilot minimum turns every engage of a higher target into a ~2.5 A-per-
up-period crawl). The car term is floored to whole amps: a fractional term
races the box's own sub-amp rounding and pins the grant one amp *under* target
(grant stuck at 5 for target 6, observed live 2026-07-05, second bite). The pin
must **not** apply before the session: the
start law grants the bare headroom (no car term, above), so folding the MID
standby noise (~45 mA) into the pre-session report lands the start grant just
below the 6 A pilot floor and the box never opens (observed live 2026-07-05 —
`reported 10.045` at target 6, `lb` stuck at 0, pilot stuck in `B`). **Cold-start
kick** (live 2026-07-09): serving the clean deficit `MAX − target` is still not
enough to *open* a session near the floor — the start law needs the opening grant
to **exceed `MAX/2`**, but `MAX − target` grants only `target`, so target 6 (grant
6 ≤ 8) leaves the box refusing indefinitely: offer ~5 A, pilot stuck in `B`, both
`lb` and `ev_requested` 0, and the offer is **invariant** to the meter value there
(`reported 10` at target 6 and `reported 8` at target 8 both stall; only `reported
0` opened it). So whenever the box grants nothing (`lb = 0`) the controller serves
the **full offer** (`reported = 0` → grant `MAX`, well over `MAX/2`) to force the box
open, then falls through to the pin path the instant it grants. The rule is
deliberately **memoryless**: a session that merely *ends* (car full, unplugged)
leaves no pause behind — `enable`, `target` and both feeds stay valid — so any
"already kicked once" latch stays disarmed for the life of the process and the box
never reopens (live 2026-07-10; the box sat at `reported 10, lb 0, cp B` with the
car plugged in and 4.4 kW of surplus). A one-tick full offer cannot lift an open
session either, since the box's up clock is ~30 s. Live-proven 2026-07-09: the exact
stall opened `B → C` at target 6 (`reported 0` → box granted 16 → the pin ramped
down to 7); the car is still in its contactor lag when the ceiling grant lands, so
it never draws it. **Shed
floor**: the over-report is additionally capped at `lb − (MIN_CHARGE + 1)`, so
the box is never told to shed into its pilot floor — target 6 deliberately
settles at 7 A (inside the ±1 A acceptance) instead of risking the ≥2 A-step
cut. Proven in the simulator across the full 6–16 A staircase, under deep PV
export, across the box's clock bands (up 24–36 s, down 4–6 s), car start lags
0/15 s, ~2 % car overdraw and MID standby noise (`core/tests/replay.rs`); the
box-side dynamics are pinned by the 2026-07-02 probe, the 2026-07-03 flag-day
captures and the 2026-07-05 campaign sweeps. **Live-proven 2026-07-03**
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
tick. `main()` also owns the WiFi guard and a 60 s **task watchdog**; if the prober
loop ever returns, it **reboots** to re-run bring-up.

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
**pause-only** — the safe direction for an evcc/HA-managed box (#52):

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

No config files, no env vars at runtime. `WIFI_SSID` / `WIFI_PASSWORD` / `MQTT_URL` /
`OTLP_LOGS_URL` — and the optional `OTLP_LOGS_AUTH` — are **baked in at build time**
(`env!`, never committed); everything else is a compile-time constant. The build is
per-install, not a generic image.

### Logging

`tracing` is the facade the whole firmware writes to — including everything
esp-idf-svc emits through the `log` crate, bridged in — and every record goes two
places: the USB serial console, and **OTLP log records over HTTP/protobuf** to
`OTLP_LOGS_URL` (the full signal endpoint, e.g. `http://collector.lan:4318/v1/logs`).
Records carry `service.name`, `service.version` (the `git describe` build id) and
`service.instance.id` (the board's MAC) as resource attributes, and every export
carries a `stream-name: evc04` header plus, when `OTLP_LOGS_AUTH` was set at build
time, that value as `Authorization` (e.g. `Basic <base64>`). Leaving it unset posts
unauthenticated — fine for a collector that only listens on the trusted LAN.

Why it exists: on 2026-09-02 the box latched a solid red fault for ~10 h while every
parsed field read healthy — the only witness would have been the raw CN28 lines, and
those were kept nowhere. So:

- **One record per reassembled CN28 LOG line** (target `evc04::cn28`), the body
  escaped rather than lossily decoded, so wreckage survives the trip.
- **The hex dump rides only on a line that fails to parse** — the forensic case.
  Attaching it to every line would double a stream already near its budget.
- **A monotonic parse-failure count** on every such record: on its own it would have
  flagged the incident at 22:06. Blank padding lines never count.
- **Control ticks are logged on change**, not at 1 Hz, and each carries the
  `reason` — which rule of the grant law (§6) produced the value: `Failsafe`,
  `PilotProbe`, `Shut`, `ColdStartKick`, `RampPin` or `LbTracking`. The served
  current cannot be inverted back into a decision (a failsafe pause and a box
  held shut both report `max + margin`), so the rule names itself rather than
  `core` growing a logger it does not want.
- **The fault is sticky** (`fault` on the telemetry topic, see
  [`cn28-log-protocol.md`](cn28-log-protocol.md)) — the box now remembers.

Only the firmware's own records leave the box, plus anything anyone reports as a
problem. Everything from this crate and `core` carries an `evc04` target prefix;
third-party crates (esp-idf-svc, the MQTT and HTTP clients) arrive through the
`log` bridge under the target `log`, and their routine chatter is dropped — on a
bounded queue it crowds out the records that are ours. A warning or error from
them still passes: on a box reachable only over the network, an esp-idf-svc error
is sometimes the only account of why it went.

Verbosity is switchable at runtime, because the box is sealed and every other way
to change it costs an OTA:

```
evc04/device/log_level    (in)  {"level":"debug"}  |  {"level":"info"}
```

`debug` adds the per-window CN28 chatter and **expires on its own after 15
minutes**, the same bounded-diagnostic shape as the measurement probe — a
forgotten debug session would out-run the exporter queue and drown the records it
was switched on to find.

The plane is built so it can never endanger the meter poll: emitting a record is a
`try_send` onto a bounded queue that **drops** when the collector is unreachable, all
network work happens on the SDK's own exporter thread, and that thread runs inside
the SDK's telemetry-suppressed scope so shipping a batch cannot generate another one.
Nothing is persisted across a reboot — with two OTA slots there is no flash to spare,
and the reboot itself is already reported (`reset_reason`, plus the sticky fault).
Timestamps come from SNTP, which syncs a few seconds after boot rather than delaying
the workers; the first records of a boot carry a pre-sync timestamp.

### Continuous availability

A **silent meter is dangerous**: with the Power Optimizer enabled the box
**hard-faults to a solid red LED** when the meter stops answering — it does
*not* fall back to full charge. So the RS485 slave must never wedge: it runs on its
own thread, the task watchdog reboots a hung chip, and an OTA rollout must **overlap**
(the new image answers polls before the old stops).

### Build & flash

Local only — the Espressif Xtensa toolchain and the device dependency keep
`firmware/` out of CI:

```
firmware/bootstrap.sh              # once: sysdeps + espup + cargo tools + esp-clang shim
export WIFI_SSID=… WIFI_PASSWORD=… MQTT_URL=mqtt://user:pass@host:1883
export OTLP_LOGS_URL=http://collector.lan:4318/v1/logs
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
The verified frames in §5/§9 keep the wire protocol a fixed target.

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

## 9. Quick reference

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
