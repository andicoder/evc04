# evc04-charge — Specification

A complete, self-contained brief for building the service. No external context is
required: everything the EVC04 does on the wire, the control math, the service
behaviour, and the open hardware questions are below.

---

## 1. Goal

Control charging on a **Vestel EVC04-AC11-T2P** wallbox so that an external
controller (Home Assistant / evcc, following day-ahead prices and/or PV surplus)
can decide **when and how fast** the car charges — both **continuous current
modulation** and **on/off** gating, the controller's choice.

The constraint that shapes the entire design: **this box has no communication
module**, so none of the "normal" control paths work (see §2). The only available
lever is the box's **Power Optimizer**, which polls an external energy meter over
RS485 and runs a **closed feedback loop** that ramps charge current until the
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
the job of the installation and the DIP-set limit. The governing rule everywhere:
**never worse than no tool** — every failure of the control layer falls back to the
baseline, **full charge** (§9).

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
always ramps to full (`reported < MAX_BOX_AMPERE`) or cuts off
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
- **3-phase floor ≈ 6 A ≈ 4.1 kW.** Below that the box can't hold a stable
  current, so a **minimum-charge cutoff** applies: `target < MIN_CHARGE_AMPERE` (~6 A)
  → serve a hard pause (`reported ≥ MAX_BOX_AMPERE`), don't try to modulate the floor.

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

> **Implementation status.** The closed-loop model above is the **target design**
> (tracked by #21–#28). The current code still serves the **static** open-loop
> value (`reported = MAX_BOX_AMPERE − target`) — on/off only — as the shipped stepping
> stone; #22–#25 add the measured input, the soft-ramp, the min-charge cutoff, and
> the measurement-loss failsafe.

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
| `MAX_BOX_AMPERE` | the box's DIP-set current ceiling, amps — our 100 % reference for the headroom math (must match DIP 4-5-6) |
| `MQTT_HOST` / `MQTT_PORT` / `MQTT_USER` / `MQTT_PASS` | broker |
| `MQTT_TOPIC_TARGET` | inbound: target charge current (A) |
| `MQTT_TOPIC_MEASURED` | inbound: live measured per-phase current (A), closes the loop (default `evc04/measured`) |
| `MQTT_TOPIC_STATUS` | outbound: liveness/state (retained) |
| `SLAVE_ADDR` | default 1 |
| `POLL_REGISTER` / `POLL_QTY` | default 0x500C / 6 (override only for debugging) |
| `MIN_CHARGE_AMPERE` | below this target → hard pause; don't modulate the 3φ floor (default 6) |
| `RAMP_RATE_AMPERE_PER_S` | soft-ramp slope for the offset, A per second (default 0.5) |
| `FAILSAFE_AFTER_S` | seconds the last target stays valid before the **full-charge** failsafe engages (default 60; must exceed the controller's republish interval) |
| `MEAS_STALE_TIMEOUT_S` | seconds the last measured value stays valid before the measurement failsafe falls back to **full charge** (default 15; see §9) |

**Origin:** a hand-rolled pymodbus RTU slave first proved the `0x500C × 6` poll
could be answered cleanly over the Waveshare in transparent mode (no resync
storms). The shipping service is **Rust / tokio** (see `CLAUDE.md` for the stack
rationale); the verified frames in §5/§11 make the protocol a fixed target.

---

## 8. MQTT contract

**The full, authoritative contract lives in [`docs/mqtt.md`](docs/mqtt.md).** All
payloads are UTF-8 JSON; QoS 1; target/measured/status retained. Summary:

- **Inbound — target** (`MQTT_TOPIC_TARGET`): `{ "amps": N }`, the desired charge
  current. Out-of-range is clamped (not rejected); invalid payloads are ignored
  and the last good value held, surfaced in `last_error`.
- **Inbound — measured** (`MQTT_TOPIC_MEASURED`): `{ "amps": N }`, the live
  per-phase current that closes the loop (§6). Source-agnostic; same
  hold-last-good / staleness discipline as the target.
- **Outbound — status** (`MQTT_TOPIC_STATUS`, retained, + offline LWT): `online`,
  `target_a`, `reported_a`, `last_poll_age_s`, `gateway`, `mqtt`, `last_error`,
  plus the closed-loop fields `measured_a`, `offset_a`, `measurement_age_s`,
  `ramping`, and the two failsafe flags (`failsafe` for target, plus a measurement
  failsafe).

**The brain is evcc** (#28): this service is a mode-agnostic actuator, driven as an
**evcc custom charger** — `maxcurrent` → target, `enable=false` → target below the
cutoff, status → A/B/C charging state. evcc's control interval must exceed the
inner loop's settle time (~30–60 s) or the two loops hunt. A HA-only setup (number
entity → target, sensor ← status) also works for simple on/off + manual current.

---

## 9. Open items — must be resolved on real hardware (car plugged in)

These are **not** answerable from the bus alone; they need an observable
(delivered charge current with a car connected):

- [x] **DIP 4-5-6 current limit = 65 A** (DIP on-on-off). Confirmed empirically: the
      charge cliffs to pause at exactly `reported = 65 A` (see §6).
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
  - **Design consequence — two failsafe layers, both toward full charge:**
    1. **Control input stale, slave still answering** (broker down, controller
       offline, cold start): keep answering, but serve **full charge** (`reported
       = 0`) — the meterless-box baseline. `FAILSAFE_AFTER_S` bounds the target
       staleness and `MEAS_STALE_TIMEOUT_S` the measured one; neither ever pauses.
       Fuse protection is out of scope (§1), so there is no reason to fail toward
       no-charge — *never worse than no tool*.
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
- [ ] **Decide the operating DIP current limit (#27).** The DIP couples two opposed
      goals: **modulation** wants a low limit (16 A maps onto the car's 6–16 A
      envelope, tested) while **guaranteed full charge** wants a high limit (offset
      0 holds the *total* at the limit, so at 16 A a loaded household throttles the
      car). Open sub-question: **does closed-loop modulation stay stable at a higher
      DIP (e.g. 32 A)?** Untested — re-run the offset/soft-ramp sweep at DIP 32 A
      with simulated household load and decide. Photograph DIP state first (revert
      safety).
- [ ] **Measurement-input staleness failsafe (#25).** Distinct from the target
      staleness above: serving `offset + stale_measured` is meaningless — a frozen
      value no longer tracks the draw — so a stale **measured** input must abandon
      the closed loop and fall back to **full charge** (`reported = 0`) within
      `MEAS_STALE_TIMEOUT_S`. Same static baseline as the target failsafe, never a
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
