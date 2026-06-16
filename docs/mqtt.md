# MQTT contract

The control surface of `evc04-charge`. An external controller (e.g. Home
Assistant following Tibber prices / PV surplus) sets a **target charge current**;
the service translates it to the fabricated household current the EVC04 reads
(`reported = fuse_limit − target`, see [`SPECS.md`](../SPECS.md) §6) and reports
liveness back.

Two topics, both configured via env vars ([`SPECS.md`](../SPECS.md) §7):

| Direction | Env var | Default suggestion |
|---|---|---|
| Inbound (command) | `MQTT_TOPIC_TARGET` | `evc04/target` |
| Outbound (state) | `MQTT_TOPIC_STATUS` | `evc04/status` |

All payloads are UTF-8 JSON. Connection uses `MQTT_USER` / `MQTT_PASS`; QoS and
retention are fixed by this contract (below), not configurable.

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

### Semantics

- **Range:** `0 … FUSE_LIMIT_A`. The value is clamped to this range before the
  control math runs (`reported_current` already clamps — [`SPECS.md`](../SPECS.md)
  §6), so out-of-range numbers are accepted and clamped, **not** rejected:
  - `amps = 0` → report `FUSE_LIMIT_A` → **charging pauses**.
  - `amps ≥ FUSE_LIMIT_A` → report `0` → **maximum charge** the box allows.
  - in between → continuous modulation.
- **Retained** so a service restart resumes the last commanded target without
  waiting for the controller to re-publish.
- **Invalid payloads are ignored, not applied:** malformed JSON, a missing
  `amps`, a non-numeric or non-finite (`NaN`/`Inf`) `amps`. The last valid target
  stays in effect and the rejection is surfaced in the status topic
  (`last_error`). This is deliberate: a controller bug must never silently push
  the charger to an unintended current.
- **Staleness → failsafe.** If no valid target arrives within the failsafe
  timeout, the service serves the current derived from `FAILSAFE_TARGET_A`
  instead ([`SPECS.md`](../SPECS.md) §9, default assumption: fail toward
  no-charge). `failsafe: true` is reflected in status. A fresh valid target
  resumes normal control.

> Why JSON and not a bare number: the object leaves room for additive fields
> (e.g. an explicit `mode` or `source`) without breaking existing publishers.
> New fields are optional and ignored by older service versions.

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
  "reported_a": 9.5,
  "last_poll_age_s": 0.4,
  "gateway": "connected",
  "mqtt": "connected",
  "failsafe": false,
  "last_error": null
}
```

| Field | Type | Meaning |
|---|---|---|
| `online` | bool | Service is running and the control loop is live. Set to `false` by the broker via LWT (below) if the service dies. |
| `target_a` | number | Currently effective target (post-clamp), amps. Reflects the failsafe value when `failsafe` is `true`. |
| `reported_a` | number | Household current the meter slave is serving per phase (`fuse_limit − target_a`), amps. |
| `last_poll_age_s` | number | Seconds since the EVC04 last polled us. The box polls ~1 Hz; a growing value signals a dead RS485 link. |
| `gateway` | string | RS485↔TCP gateway link: `connected` \| `reconnecting` \| `down`. |
| `mqtt` | string | Broker link as seen by the service: `connected` \| `reconnecting`. |
| `failsafe` | bool | `true` while serving `FAILSAFE_TARGET_A` because the target went stale. |
| `last_error` | string \| null | Human-readable reason for the most recent rejected target or link fault; `null` when healthy. |

### Last Will and Testament

On connect, the service registers an LWT on `MQTT_TOPIC_STATUS` (retained) so an
ungraceful disconnect flips status to offline without any client polling:

```json
{ "online": false }
```

---

## Home Assistant wiring (reference)

```yaml
# Command: a number entity publishes {"amps": <value>} to the target topic.
mqtt:
  number:
    - name: "EVC04 target current"
      command_topic: "evc04/target"
      command_template: '{"amps": {{ value }}}'
      min: 0
      max: 16          # = FUSE_LIMIT_A at the installation
      step: 0.5
      unit_of_measurement: "A"
      retain: true

  # State: one sensor exposes the status JSON as attributes.
  sensor:
    - name: "EVC04 status"
      state_topic: "evc04/status"
      value_template: "{{ 'online' if value_json.online else 'offline' }}"
      json_attributes_topic: "evc04/status"
```

`max` must match the `FUSE_LIMIT_A` configured for the box; the DIP-selected fuse
limit is an open hardware item ([`SPECS.md`](../SPECS.md) §9) and is **not**
published by this service.
