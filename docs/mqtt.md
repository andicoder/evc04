# MQTT contract

The control surface of `evc04-charge`. An external controller (Home Assistant, or
**evcc** as a custom charger) drives the service with two inputs — a **target
charge current** and a **live measured current** — and reads liveness/state back.
The service is a mode-agnostic actuator: it serves `reported = clamp(offset +
measured)` with `offset = MAX_BOX_AMPERE − target` (the closed loop, see
[`SPECS.md`](../SPECS.md) §6). The charging *brain* (price / PV / departure) lives
in the controller, never here.

Topics, all configured via env vars ([`SPECS.md`](../SPECS.md) §7):

| Direction          | Env var               | Default          |
| ------------------ | --------------------- | ---------------- |
| Inbound — target   | `MQTT_TOPIC_TARGET`   | `evc04/target`   |
| Inbound — measured | `MQTT_TOPIC_MEASURED` | `evc04/measured` |
| Outbound — status  | `MQTT_TOPIC_STATUS`   | `evc04/status`   |

All payloads are UTF-8 JSON. Connection uses `MQTT_USER` / `MQTT_PASS`; QoS (1) and
retention are fixed by this contract, not configurable.

---

## Inbound — target charge current

**Topic:** `MQTT_TOPIC_TARGET` · **QoS 1** · **publish retained**

The controller publishes the desired per-phase charge current in ampere:

```json
{ "ampere": 6.5 }
```

| Field  | Type   | Required | Meaning                                 |
| ------ | ------ | -------- | --------------------------------------- |
| `ampere` | number | yes      | Desired charge current per phase, ampere. |

### Semantics — the target selects the mode

The service has no modes; the controller picks behaviour purely by the target it
publishes (closed loop, [`SPECS.md`](../SPECS.md) §6):

- **`ampere ≥ MAX_BOX_AMPERE`** → `offset = 0` → the box holds the *total* current at
  `MAX_BOX_AMPERE` → **full charge** (the box's own loop keeps the total within that
  limit — fuse protection is the box's job, not ours).
- **`MIN_CHARGE_AMPERE ≤ ampere < MAX_BOX_AMPERE`** → **modulate**: the closed loop tracks
  the delivered current toward `ampere` (requires a live measured feed; stable band
  ~9–15 A with HA-speed measurement).
- **`ampere < MIN_CHARGE_AMPERE`** (~6 A) → **pause**: below the 3-phase floor the box
  can't hold a stable current, so the service serves a hard pause.

Out-of-range numbers are accepted and clamped, not rejected.

- **Retained** so a restart resumes the last commanded target without waiting for
  the controller to re-publish.
- **Invalid payloads are ignored, not applied:** malformed JSON, missing `ampere`,
  non-numeric or non-finite (`NaN`/`Inf`). The last valid target stays in effect
  and the rejection is surfaced in status (`last_error`). A controller bug must
  never silently push the charger to an unintended current.
- **Staleness → failsafe.** If no valid target arrives within `TARGET_TIMEOUT_SECONDS`,
  the service engages the **configurable** `TARGET_FAILSAFE` direction (#51) and sets
  `failsafe: true`. A fresh valid target resumes control.
  - `full_charge` (default) → `reported = 0`, the meterless-box default
    ([`SPECS.md`](../SPECS.md) §9, *never worse than no tool*) — for HA-automation-only
    boxes.
  - `pause` → report the ceiling (box stops) — the safe choice for an **evcc-managed**
    box, so a control-path blip can't flip an intended pause into charging.
  - `hold_last` → keep serving the last commanded value.

> Why JSON not a bare number: the object leaves room for additive fields without
> breaking publishers; new fields are optional and ignored by older versions.

---

## Inbound — measured current (closes the loop)

**Topic:** `MQTT_TOPIC_MEASURED` · **QoS 1** · **publish retained**

The live per-phase current the meter should reflect so the box's feedback loop sees
its own draw rise and modulates ([`SPECS.md`](../SPECS.md) §6). Same payload shape
as the target:

```json
{ "ampere": 9.1 }
```

| Field  | Type   | Required | Meaning                                                               |
| ------ | ------ | -------- | ------------------------------------------------------------------- |
| `ampere` | number | yes      | Live measured current, ampere (a single value applied to all 3 phases). |

### Semantics

- **Source-agnostic.** The publisher decides what `ampere` means: **total/grid
  current** today (load-management + PV-surplus semantics), or a charger-side **CT
  measuring the car** later for precise control — **no service change** either way.
- Served value is `reported = clamp(offset + ampere)`, `offset = MAX_BOX_AMPERE − target`
  (soft-ramped). Publish at home-automation speed (~1–6 s); the faster the
  measurement, the lower the charge current the inner loop can hold.
- **Retained**, **invalid payloads ignored / last good held / surfaced in
  `last_error`** — same discipline as the target.
- **Staleness → measurement failsafe.** If no valid measurement arrives within
  `MEASURED_TIMEOUT_SECONDS`, serving `offset + stale` is meaningless, so the service
  abandons the closed loop and engages the **configurable** `MEASURED_FAILSAFE`
  direction (#51), setting `measurement_failsafe: true`. Same modes as
  `TARGET_FAILSAFE` (`full_charge` default / `pause` / `hold_last`); `pause` for an
  evcc-managed box ([`SPECS.md`](../SPECS.md) §9, #25).

---

## Outbound — status

**Topic:** `MQTT_TOPIC_STATUS` · **QoS 1** · **publish retained**

A single retained JSON object, republished whenever a field changes (and at least
on every state transition). Home Assistant reads it via one MQTT sensor using
`json_attributes_topic` + value templates.

```json
{
  "online": true,
  "target_ampere": 6.5,
  "measured_ampere": 5.2,
  "offset_ampere": 1.3,
  "reported_ampere": 6.5,
  "last_poll_age_s": 0.4,
  "measurement_age_s": 1.1,
  "gateway": "connected",
  "mqtt": "connected",
  "ramping": false,
  "failsafe": false,
  "measurement_failsafe": false,
  "charge_state": "C",
  "last_error": null
}
```

| Field                  | Type           | Meaning |
| ---------------------- | -------------- | ------- |
| `online`               | bool           | Service running and the control loop live. Set `false` by the broker via LWT if the service dies. |
| `target_ampere`        | number         | Last commanded target (post-clamp), ampere. Stays the commanded value during a failsafe — the `failsafe` flag (not a value jump) signals the override (#51). |
| `measured_ampere`      | number         | Last live measured current consumed, ampere. |
| `offset_ampere`        | number         | Current soft-ramped offset `= MAX_BOX_AMPERE − target`, ampere. |
| `reported_ampere`      | number         | Current the slave is serving per phase: `clamp(offset_ampere + measured_ampere)`, ampere. |
| `last_poll_age_s`      | number         | Seconds since the EVC04 last polled us (~1 Hz; a growing value signals a dead RS485 link). |
| `measurement_age_s`    | number         | Seconds since the last valid measured value; drives the measurement failsafe. |
| `gateway`              | string         | RS485↔TCP gateway link: `connected` / `reconnecting` / `down`. |
| `mqtt`                 | string         | Broker link as seen by the service: `connected` / `reconnecting`. |
| `ramping`              | bool           | `true` while the offset is still soft-ramping toward its setpoint. |
| `failsafe`             | bool           | `true` while serving **full charge** because the **target** went stale (the meterless-box default). |
| `measurement_failsafe` | bool           | `true` while serving full charge because the **measured** input went stale. |
| `charge_state`         | string         | Approximated evcc charge status (#28): `C` while charge is allowed and current flows, else `B` (connected, not charging). `A` (no vehicle) is never asserted — a meter emulation has no control-pilot line. evcc's custom-charger `status` reads this. |
| `last_error`           | string or null | Reason for the most recent rejected input or link fault; `null` when healthy. |

### Last Will and Testament

On connect, the service registers an LWT on `MQTT_TOPIC_STATUS` (retained) so an
ungraceful disconnect flips status to offline without any client polling:

```json
{ "online": false }
```

---

## Driving from evcc (recommended)

The charging brain is **evcc** (#28); this service is its **custom charger**:

- evcc `maxcurrent` → the **target** topic (ampere); `enable=false` → a target below
  `MIN_CHARGE_AMPERE` (pause), `enable=true` → resume the commanded target.
- evcc reads the **status** topic: `charge_state` (`B`/`C`) and `target_ampere`.
- The **measured** topic is independent — HA/evcc publishes the live grid (or
  later car) current there.
- **Timing:** evcc's control interval must exceed the inner loop's settle time
  (~30–60 s) or the two loops hunt; use a matching interval + hysteresis.

A working evcc charger template, the min/max-current guidance, and the
nested-loop timing live in **[evcc.md](evcc.md)**.

---

## Home Assistant wiring (reference)

```yaml
mqtt:
  number:
    # Command: a number entity publishes {"ampere": <value>} to the target topic.
    - name: "EVC04 target current"
      command_topic: "evc04/target"
      command_template: '{"ampere": {{ value }}}'
      min: 0
      max: 16          # = MAX_BOX_AMPERE / DIP setting at the installation (SPECS §2)
      step: 0.5
      unit_of_measurement: "A"
      retain: true

  sensor:
    # State: one sensor exposes the status JSON as attributes.
    - name: "EVC04 status"
      state_topic: "evc04/status"
      value_template: "{{ 'online' if value_json.online else 'offline' }}"
      json_attributes_topic: "evc04/status"

# Measured feed (closes the loop): republish a live current (grid/total today)
# to the measured topic, e.g. an automation on your grid-current sensor:
#   topic:   evc04/measured
#   payload: { "ampere": {{ states('sensor.grid_current_l1') }} }
```

`max` must match the `MAX_BOX_AMPERE` / DIP setting for the box
([`SPECS.md`](../SPECS.md) §2/§9).

---

## Home Assistant auto-discovery (optional)

Instead of the manual YAML above, the service can **self-register** read-only
sensors via [HA MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery):
on each broker connect it publishes retained config topics, and HA creates one
**device** (the wallbox) with a sensor per status field.

**Opt-in** (off by default) so an upgrade never sprays retained configs unasked:

| Env var | Default | Meaning |
| --- | --- | --- |
| `HA_DISCOVERY_ENABLED` | `false` | set `true` to publish discovery configs on connect |
| `HA_DISCOVERY_PREFIX` | `homeassistant` | HA's discovery prefix |
| `HA_DISCOVERY_NODE_ID` | `evc04` | node-id segment + device identifier — make it unique per install when several share a broker |

What you get (all read-only, grouped under one device, availability via the
`online` flag/LWT):

- **sensors** — `reported`, `target`, `measured`, `offset` current (A); `charge_state`;
  and diagnostics: `gateway`/`mqtt` link, `last_poll_age_s`, `measurement_age_s`,
  `last_error`.
- **binary_sensors** (diagnostic) — `failsafe`, `measurement_failsafe` (device class
  `problem`), `ramping`.

**No command entity is published.** Setting the target from HA would make HA a
*commander*, and there must be exactly one ([`SPECS.md`](../SPECS.md) §6). To control
from HA, use the manual `number` above instead — and then don't also run evcc.

> The broker user must be allowed to publish under `HA_DISCOVERY_PREFIX` (e.g.
> `homeassistant/#`).
