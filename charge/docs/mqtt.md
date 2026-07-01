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
| Inbound — enable   | `MQTT_TOPIC_ENABLE`   | `evc04/enable`   |
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
  can't hold a stable current, so the service serves a hard pause — `reported =
  MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE`, **above** the ceiling so the box actually cuts an
  active charge (reporting exactly the ceiling holds it, #57).

Out-of-range numbers are accepted and clamped, not rejected.

- **Retained** so a restart resumes the last commanded target without waiting for
  the controller to re-publish.
- **Invalid payloads are ignored, not applied:** malformed JSON, missing `ampere`,
  non-numeric or non-finite (`NaN`/`Inf`). The last valid target stays in effect
  and the rejection is surfaced in status (`last_error`). A controller bug must
  never silently push the charger to an unintended current.
- **Staleness → failsafe.** If no valid target arrives within `TARGET_TIMEOUT_SECONDS`,
  the service engages the **configurable** `TARGET_FAILSAFE` direction (#51/#52) and
  sets `failsafe: true`. A fresh valid target resumes control.
  - `pause` (**default**) → report above the ceiling (`+ PAUSE_MARGIN_AMPERE`, box stops,
    #57) — safe for an **evcc/HA-managed** box: a control-path blip can't flip an intended
    pause into charging (#52).
  - `full_charge` → `reported = 0`, the meterless-box baseline
    ([`SPECS.md`](../SPECS.md) §9, *never worse than no tool*) — for an
    HA-automation-only / unmanaged box.
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
  direction (#51/#52), setting `measurement_failsafe: true`. Same modes as
  `TARGET_FAILSAFE` (`pause` default / `full_charge` / `hold_last`)
  ([`SPECS.md`](../SPECS.md) §9, #25).

---

## Inbound — enable gate (optional, #60)

**Topic:** `MQTT_TOPIC_ENABLE` · **QoS 1** · **publish retained**

A dedicated on/off override, **independent of the target current**. The target topic
still selects the mode (above); this gate sits on top of it:

```json
{ "enable": false }
```

| Field    | Type | Required | Meaning                                              |
| -------- | ---- | -------- | ---------------------------------------------------- |
| `enable` | bool | yes      | `false` hard-pauses the box; `true` honors the target. |

### Semantics — an override layered on the target

- **`enable: false`** → hard pause (`reported = MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE`,
  above the ceiling so an active charge actually cuts, #57), **regardless of the
  commanded target**. It also wins over a `full_charge` target failsafe — an explicit
  off is the safest directive.
- **`enable: true`** → honor the commanded `target` (modulate / full charge as usual).
- **Default when never received: `true`** (honor the target). So existing single-topic
  deployments — the HA `number` entity, or evcc on the old contract — keep working
  unchanged; the gate is purely additive.
- **Retained**, **invalid payloads ignored / last good held / surfaced in `last_error`**
  — same discipline as target/measured. A malformed publish never flips charging.
- **No staleness failsafe.** The gate is a latch the retained topic restores on
  reconnect; it does not age out (a cold start with no enable message defaults to `true`,
  but a no-target cold start still pauses, #59).

### Why a separate topic

Overloading the single `target` topic to mean *both* "how much" and "on/off" makes an
evcc charger map its `enable` and `maxcurrent` set-plugins onto the same topic, where
they race: whenever `enable(true)` (≈ a tiny current = pause) lands as the last write,
the box never starts. A dedicated enable topic removes the collision (#60).

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
  "enabled": true,
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
| `enabled`              | bool           | The enable gate (#60): `false` while charging is hard-paused regardless of the target. `true` by default and for single-topic deployments. evcc's `enabled` read maps here. |
| `last_error`           | string or null | Reason for the most recent rejected input or link fault; `null` when healthy. |

> **`charge_state` is a command, not the car's real pilot state — don't confuse it with
> `cp_state`.** `charge_state` (this control-plane `status` topic) is what the emulation
> *commands*: derived purely from `reported_ampere` (`> max` → `B`, else `C`), only ever
> `B`/`C`, and read by evcc as its charger `status`. With no control-pilot line it can
> never assert `A` (no vehicle) — so through this field evcc **cannot** tell an unplugged
> car from a connected-but-idle one, and it can even read `C` spuriously when grid import
> alone trips the charging floor.
>
> The box's **real** IEC-61851 pilot state (`A`/`B`/`C`/`F`) is `cp_state`, decoded from
> the CN28 LOG on the telemetry plane (`evc04/cn28/telemetry`, see
> [`docs/cn28-log-protocol.md`](../../docs/cn28-log-protocol.md)). It is the genuine
> plug/charge observation, but it is nullable/unreliable — `null` until the next
> plug/unplug/charge event (#117) — so HA treats it as *unavailable* when null and it
> must stay an observation/diagnostic signal, **not** an automation or control input.
>
> The two look alike (both use `A`/`B`/`C`) but answer different questions:
> `charge_state` = our *intent*, `cp_state` = the box's *reality*. Keep the roles
> separate; never feed the unreliable `cp_state` into the control path.

### Last Will and Testament

On connect, the service registers an LWT on `MQTT_TOPIC_STATUS` (retained) so an
ungraceful disconnect flips status to offline without any client polling:

```json
{ "online": false }
```

---

## Driving from evcc (recommended)

The charging brain is **evcc** (#28); this service is its **custom charger**:

- evcc `maxcurrent` → the **target** topic (ampere); evcc `enable` → the **enable**
  topic (`{"enable": true|false}`). Separate topics so on/off and the current setpoint
  never race on one write path (#60).
- evcc reads the **status** topic: `charge_state` (`B`/`C`), `target_ampere`, and
  `enabled`.
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
  `problem`), `ramping`, `enabled` (the on/off gate, #60).

**No command entity is published.** Setting the target from HA would make HA a
*commander*, and there must be exactly one ([`SPECS.md`](../SPECS.md) §6). To control
from HA, use the manual `number` above instead — and then don't also run evcc.

> The broker user must be allowed to publish under `HA_DISCOVERY_PREFIX` (e.g.
> `homeassistant/#`).
