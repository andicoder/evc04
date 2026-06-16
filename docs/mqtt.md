# MQTT contract

The control surface of `evc04-charge`. An external controller (Home Assistant, or
**evcc** as a custom charger) drives the service with two inputs — a **target
charge current** and a **live measured current** — and reads liveness/state back.
The service is a mode-agnostic actuator: it serves `reported = clamp(offset +
measured)` with `offset = fuse_limit − target` (the closed loop, see
[`SPECS.md`](../SPECS.md) §6). The charging *brain* (price / PV / departure) lives
in the controller, never here.

> **Implementation status.** The current build implements the **target** topic and
> the base **status** fields, serving the open-loop `reported = fuse_limit − target`
> (on/off only). The **measured** topic and the closed-loop status fields below
> (`measured_a`, `offset_a`, `measurement_age_s`, `ramping`, `measurement_failsafe`)
> are the target contract, tracked by #21–#25; they are documented here so
> publishers and the HA/evcc wiring stay stable across the rollout.

Topics, all configured via env vars ([`SPECS.md`](../SPECS.md) §7):

| Direction | Env var | Default |
|---|---|---|
| Inbound — target | `MQTT_TOPIC_TARGET` | `evc04/target` |
| Inbound — measured | `MQTT_TOPIC_MEASURED` | `evc04/measured` |
| Outbound — status | `MQTT_TOPIC_STATUS` | `evc04/status` |

All payloads are UTF-8 JSON. Connection uses `MQTT_USER` / `MQTT_PASS`; QoS (1) and
retention are fixed by this contract, not configurable.

---

## Inbound — target charge current

**Topic:** `MQTT_TOPIC_TARGET` · **QoS 1** · **publish retained**

The controller publishes the desired per-phase charge current in amps:

```json
{ "amps": 6.5 }
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `amps` | number | yes | Desired charge current per phase, amps. |

### Semantics — the target selects the mode

The service has no modes; the controller picks behaviour purely by the target it
publishes (closed loop, [`SPECS.md`](../SPECS.md) §6):

- **`amps ≥ FUSE_LIMIT_A`** → `offset = 0` → the box holds the *total* current at
  the fuse limit → **full charge** (still bounded by the main fuse).
- **`MIN_CHARGE_A ≤ amps < FUSE_LIMIT_A`** → **modulate**: the closed loop tracks
  the delivered current toward `amps` (requires a live measured feed; stable band
  ~9–15 A with HA-speed measurement).
- **`amps < MIN_CHARGE_A`** (~6 A) → **pause**: below the 3-phase floor the box
  can't hold a stable current, so the service serves a hard pause.

Out-of-range numbers are accepted and clamped, not rejected.

> The current open-loop build serves `reported = fuse_limit − target` directly,
> which gives **on/off** only — full below the fuse, pause at/above it — until the
> measured loop lands (#21).

- **Retained** so a restart resumes the last commanded target without waiting for
  the controller to re-publish.
- **Invalid payloads are ignored, not applied:** malformed JSON, missing `amps`,
  non-numeric or non-finite (`NaN`/`Inf`). The last valid target stays in effect
  and the rejection is surfaced in status (`last_error`). A controller bug must
  never silently push the charger to an unintended current.
- **Staleness → failsafe.** If no valid target arrives within `FAILSAFE_AFTER_S`,
  the service serves `FAILSAFE_TARGET_A` ([`SPECS.md`](../SPECS.md) §9, fail toward
  no-charge); `failsafe: true` in status. A fresh valid target resumes control.

> Why JSON not a bare number: the object leaves room for additive fields without
> breaking publishers; new fields are optional and ignored by older versions.

---

## Inbound — measured current (closes the loop)

**Topic:** `MQTT_TOPIC_MEASURED` · **QoS 1** · **publish retained** · *(planned, #22)*

The live per-phase current the meter should reflect so the box's feedback loop sees
its own draw rise and modulates ([`SPECS.md`](../SPECS.md) §6). Same payload shape
as the target:

```json
{ "amps": 9.1 }
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `amps` | number | yes | Live measured current, amps (a single value applied to all 3 phases). |

### Semantics

- **Source-agnostic.** The publisher decides what `amps` means: **total/grid
  current** today (load-management + PV-surplus semantics), or a charger-side **CT
  measuring the car** later for precise control — **no service change** either way.
- Served value is `reported = clamp(offset + amps)`, `offset = fuse_limit − target`
  (soft-ramped). Publish at home-automation speed (~1–6 s); the faster the
  measurement, the lower the charge current the inner loop can hold.
- **Retained**, **invalid payloads ignored / last good held / surfaced in
  `last_error`** — same discipline as the target.
- **Staleness → measurement failsafe.** If no valid measurement arrives within
  `MEAS_STALE_TIMEOUT_S`, serving `offset + stale` is dangerous, so the service
  abandons the closed loop and serves a **safe static pause**
  (`measurement_failsafe: true`). Distinct from — and taking precedence over — the
  target failsafe ([`SPECS.md`](../SPECS.md) §9, #25).

---

## Outbound — status

**Topic:** `MQTT_TOPIC_STATUS` · **QoS 1** · **publish retained**

A single retained JSON object, republished whenever a field changes (and at least
on every state transition). Home Assistant reads it via one MQTT sensor using
`json_attributes_topic` + value templates.

```json
{
  "online": true,
  "target_a": 6.5,
  "measured_a": 5.2,
  "offset_a": 1.3,
  "reported_a": 6.5,
  "last_poll_age_s": 0.4,
  "measurement_age_s": 1.1,
  "gateway": "connected",
  "mqtt": "connected",
  "ramping": false,
  "failsafe": false,
  "measurement_failsafe": false,
  "last_error": null
}
```

| Field | Type | Meaning |
|---|---|---|
| `online` | bool | Service running and the control loop live. Set `false` by the broker via LWT if the service dies. |
| `target_a` | number | Effective target (post-clamp), amps. Reflects `FAILSAFE_TARGET_A` when `failsafe` is true. |
| `measured_a` | number | Last live measured current consumed, amps. *(planned, #22)* |
| `offset_a` | number | Current soft-ramped offset `= fuse_limit − target`, amps. *(planned, #24)* |
| `reported_a` | number | Current the slave is serving per phase: `clamp(offset_a + measured_a)` (closed-loop) or `fuse_limit − target_a` (open-loop build), amps. |
| `last_poll_age_s` | number | Seconds since the EVC04 last polled us (~1 Hz; a growing value signals a dead RS485 link). |
| `measurement_age_s` | number | Seconds since the last valid measured value; drives the measurement failsafe. *(planned, #25)* |
| `gateway` | string | RS485↔TCP gateway link: `connected` \| `reconnecting` \| `down`. |
| `mqtt` | string | Broker link as seen by the service: `connected` \| `reconnecting`. |
| `ramping` | bool | `true` while the offset is still soft-ramping toward its setpoint. *(planned, #24)* |
| `failsafe` | bool | `true` while serving `FAILSAFE_TARGET_A` because the **target** went stale. |
| `measurement_failsafe` | bool | `true` while serving a safe static pause because the **measured** input went stale (precedence over `failsafe`). *(planned, #25)* |
| `last_error` | string \| null | Reason for the most recent rejected input or link fault; `null` when healthy. |

### Last Will and Testament

On connect, the service registers an LWT on `MQTT_TOPIC_STATUS` (retained) so an
ungraceful disconnect flips status to offline without any client polling:

```json
{ "online": false }
```

---

## Driving from evcc (recommended)

The charging brain is **evcc** (#28); this service is its **custom charger**:

- evcc `maxcurrent` → the **target** topic (amps); `enable=false` → a target below
  `MIN_CHARGE_A` (pause), `enable=true` → resume the commanded target.
- evcc reads the **status** topic for charging state / current.
- The **measured** topic is independent — HA/evcc publishes the live grid (or
  later car) current there.
- **Timing:** evcc's control interval must exceed the inner loop's settle time
  (~30–60 s) or the two loops hunt; use a matching interval + hysteresis.

A working evcc charger template is planned for `docs/` (#28).

---

## Home Assistant wiring (reference)

```yaml
mqtt:
  number:
    # Command: a number entity publishes {"amps": <value>} to the target topic.
    - name: "EVC04 target current"
      command_topic: "evc04/target"
      command_template: '{"amps": {{ value }}}'
      min: 0
      max: 16          # = FUSE_LIMIT_A / DIP setting at the installation (SPECS §2)
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
#   payload: { "amps": {{ states('sensor.grid_current_l1') }} }
```

`max` must match the `FUSE_LIMIT_A` / DIP setting for the box
([`SPECS.md`](../SPECS.md) §2/§9).
