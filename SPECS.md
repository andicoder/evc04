# evc04-charge — Specification

A complete, self-contained brief for building the service. No external context is
required: everything the EVC04 does on the wire, the control math, the service
behaviour, and the open hardware questions are below.

---

## 1. Goal

Control charging on a **Vestel EVC04-AC11-T2P** wallbox so that an external
controller (Home Assistant, following Tibber day-ahead prices and/or PV surplus)
can decide **when and how fast** the car charges — ideally with **continuous
current modulation**, at minimum with **on/off** gating.

The constraint that shapes the entire design: **this box has no communication
module**, so none of the "normal" control paths work (see §2). The only available
lever is the box's **Power Optimizer**, which polls an external energy meter over
RS485 and reduces charge current to stay under a fuse limit. We **emulate that
meter** and feed it fabricated values.

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

**The Power Optimizer:** enabled via on-board **DIP switches 4-5-6** (set a fuse
limit per a DIP table; any non-all-off value enables polling). Once enabled, the
box continuously polls an external meter on CN20 and limits
`charge_current ≤ fuse_limit − household_current`.

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

The Power Optimizer computes:

```
available_charge_current = fuse_limit − reported_household_current   (per phase)
```

So to command a target charge current we report:

```
reported_current = fuse_limit − target_charge_current               (per phase, all 3)
```

- Report **0 A** → full headroom → car charges at the box maximum.
- Report **≈ fuse_limit** (or above) → zero/negative headroom → charging pauses.
- Report **fuse_limit − X** → box allows ~**X A** → continuous modulation.

`fuse_limit` is the value selected by **DIP 4-5-6** on the board. **It must be
read off the actual installation** (see open item in §9) — the exact headroom
math (and whether the box clamps/rounds) is only confirmable with a car.

**Two strategies** (pick A first; B is the full version):

- **Strategy A — on/off (simple, ships the Tibber goal):** cheap hour → report
  0 A; expensive hour → report a high current (e.g. 80 A) → charging pauses.
  Needs no further reverse-engineering; binary, robust.
- **Strategy B — modulation (full):** report `fuse_limit − target` for a target
  current that tracks dynamic price / PV surplus → continuous control.

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
3. **MQTT control.** Subscribe to a **target charge current** topic; convert it
   to the reported household current via `report = fuse_limit − target` and update
   the values the slave serves. **Publish liveness/status** back to MQTT.

**Configuration — all via environment variables** (no config files, no secrets in
the image). At minimum:

| Env var (suggested) | Meaning |
|---|---|
| `GATEWAY_HOST` / `GATEWAY_PORT` | RS485↔TCP gateway address (e.g. Waveshare) |
| `FUSE_LIMIT_A` | the DIP-selected fuse limit, amps (for the headroom math) |
| `MQTT_HOST` / `MQTT_PORT` / `MQTT_USER` / `MQTT_PASS` | broker |
| `MQTT_TOPIC_TARGET` | inbound: target charge current (A) |
| `MQTT_TOPIC_STATUS` | outbound: liveness/state (retained) |
| `SLAVE_ADDR` | default 1 |
| `POLL_REGISTER` / `POLL_QTY` | default 0x500C / 6 (override only for debugging) |
| `FAILSAFE_TARGET_A` | value to serve if MQTT goes stale (see §9) |

**Prototype that already worked:** a hand-rolled pymodbus RTU slave answering the
`0x500C × 6` poll with the real Inepro float map over the Waveshare in transparent
mode. The box accepted the framing cleanly (no resync storms). Rebuild a bench
venv with `python3 -m venv venv && venv/bin/pip install pymodbus pyserial`.
Suggested stack: **Python + pymodbus + paho-mqtt** (but the choice is open).

---

## 8. MQTT contract (to be finalised)

Not yet fixed — **define and document it as part of the build**. It must cover:

- **Inbound target topic** — payload schema (plain number in amps? JSON?),
  units, valid range, what an out-of-range or missing value means.
- **Outbound status topic** — retained state: connected?, last poll age, current
  reported A, current target A, gateway/MQTT health.
- **Retention & QoS** — target probably retained so a restart resumes the last
  command; status retained for HA.

Keep it simple and HA-friendly (e.g. a number entity publishing to the target
topic, an MQTT sensor reading status).

---

## 9. Open items — must be resolved on real hardware (car plugged in)

These are **not** answerable from the bus alone; they need an observable
(delivered charge current with a car connected):

- [ ] **Read the DIP 4-5-6 fuse limit** actually set on the box → the `limit −
      target` math depends on it. Photograph current DIP state first (revert
      safety).
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
  - **Design consequence:** a meter dropout **faults** the box, it does not merely
    pause. So the service must fail toward *being present and answering*, not toward
    silence. Implement `FAILSAFE_TARGET_A` for the case where MQTT goes stale but
    the slave is still up (keep answering with a safe target), and consider the
    box's own **Failsafe Current** (control-interface reg `2000`, only on SW
    variants) or the **External Enable** input (DIP pin 2 + a relay) as an
    independent backstop only against total service loss.
  - [ ] **Still open (needs a car):** exact meter-timeout window (how many missed
        polls before red?), and whether a fault taken mid-charge latches vs. clears
        soft like the idle case.
- [ ] **Validate end-to-end with a car:** report all-zeros → confirm full-current
      charge; sweep reported current upward → confirm charge amps drop ~linearly;
      nail the exact headroom math, any rounding/clamping, and the on/off
      threshold for Strategy A.
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
- The service is **generic**: every site-specific value (gateway address, fuse
  limit, MQTT broker/topics) is an env var. Nothing about any particular home is
  hard-coded.

---

## 11. Quick reference

```
Bus:        9600 8E1, transparent TCP↔RS485 gateway
Poll:       addr 1, FC 0x03, start 0x500C, qty 6, ~1.006 s, content-agnostic
Payload:    struct.pack('>fff', L1_A, L2_A, L3_A)   # Inepro PRO380, 3× float32 ABCD
Control:    reported_A = fuse_limit_A − target_charge_A   (per phase)
            report 0   → charge max ;  report ≥ fuse_limit → pause
Poll frame: 01 03 50 0c 00 06 14 cb
Examples:   0A→01 03 0c 00000000×3 93 70  |  16A→…41800000×3 97 ae  |  63A→…427c0000×3 13 97
```
