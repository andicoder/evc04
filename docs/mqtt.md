# MQTT contract

The control surface of the on-box firmware. An external controller (Home Assistant,
or **evcc** as a custom charger) drives it with a **target charge current** and a
**grid-power heartbeat**, and reads liveness/state back. The firmware is a
mode-agnostic actuator: it regulates the box's own per-car grant (`lb_current`, read
from the CN28 LOG) toward the target and answers the emulated meter accordingly (the
V4 grant loop, see [`SPECS.md`](SPECS.md) §6). The charging *brain* (price / PV /
departure) lives in the controller, never here.

Topics are **device-scoped** under `evc04/charge/*` (fixed, not configurable):

| Direction            | Topic                     | Payload           |
| -------------------- | ------------------------- | ----------------- |
| Inbound — target     | `evc04/charge/target`     | `{ "ampere": N }` |
| Inbound — grid power | `evc04/charge/grid_power` | `{ "watt": N }`   |
| Inbound — enable     | `evc04/charge/enable`     | `{ "enable": b }` |
| Inbound — probe      | `evc04/charge/probe_over` | `{ "ampere": N }` |
| Outbound — status    | `evc04/charge/status`     | (retained JSON)   |

All payloads are UTF-8 JSON; QoS 1. The **status** topic is retained; the inbound
topics are **non-retained** — the firmware persists the last `target`/`enable` to NVS
itself (below), so it does not rely on broker retention. The broker URL and
credentials are baked into the firmware at build time (`MQTT_URL`).

---

## Inbound — target charge current

**Topic:** `evc04/charge/target` · **QoS 1** · **non-retained**

The controller publishes the desired per-phase charge current in ampere:

```json
{ "ampere": 6.5 }
```

| Field  | Type   | Required | Meaning                                 |
| ------ | ------ | -------- | --------------------------------------- |
| `ampere` | number | yes      | Desired charge current per phase, ampere. |

### Semantics — the target selects the mode

The firmware has no modes; the controller picks behaviour purely by the target it
publishes ([`SPECS.md`](SPECS.md) §6, the V4 grant loop):

- **`ampere ≥ MAX_BOX_AMPERE`** → report the ceiling → the box holds the *total* at
  `MAX_BOX_AMPERE` → **full charge** (the box's own loop keeps the total within that
  limit — fuse protection is the box's job, not ours).
- **`MIN_CHARGE_AMPERE ≤ ampere < MAX_BOX_AMPERE`** → **modulate**: the firmware
  regulates the box's grant toward `ampere` on the box's ~5 s grant-eval cadence.
- **`ampere < MIN_CHARGE_AMPERE`** (~6 A) → **pause**: below the 3-phase floor the box
  can't hold a stable current, so the firmware reports `MAX_BOX_AMPERE +
  PAUSE_MARGIN_AMPERE` — **above** the ceiling so the box actually cuts an active
  charge (reporting exactly the ceiling holds it, #57).

Out-of-range numbers are accepted and clamped, not rejected.

- **Latched, never aged out.** The target is a persistent setpoint: evcc's MQTT
  charger publishes it on-change and then holds it, so the firmware never times the
  target out — aging it would deadlock (the box forgets → pauses → evcc never
  re-sends). Controller liveness is carried by the grid heartbeat instead (below).
  The last valid target is persisted to **NVS** so an OTA/reboot resumes the command
  rather than cold-starting paused; a first-ever boot with no stored target still
  starts paused, never charging (#59).
- **Invalid payloads are ignored, not applied:** malformed JSON, missing `ampere`,
  non-numeric or non-finite (`NaN`/`Inf`). The last valid target stays in effect
  and the rejection is surfaced in status (`last_error`). A controller bug must
  never silently push the charger to an unintended current.

> Why JSON not a bare number: the object leaves room for additive fields without
> breaking publishers; new fields are optional and ignored by older versions.

---

## Inbound — grid-power heartbeat

**Topic:** `evc04/charge/grid_power` · **QoS 1** · **non-retained**

The raw grid power, forwarded to the firmware untouched:

```json
{ "watt": -3200 }
```

| Field  | Type   | Required | Meaning                                                                 |
| ------ | ------ | -------- | ----------------------------------------------------------------------- |
| `watt` | number | yes      | Raw **signed** grid power (negative = export). No W→A math, no `≥0` clamp — the publisher forwards the meter reading verbatim. |

### Semantics — a liveness heartbeat, not a control input

- **Not part of the modulation math.** V4 regulates on the box's own CN28 grant
  (`lb_current`), not on the grid reading — this retired the earlier offset+measured
  loop and, with it, the export-clamp failure mode (#136). The watts pass through to
  the status as a diagnostic only.
- **Its cadence is the controller's liveness.** The publisher republishes every ~5 s;
  more than **15 s** of silence means HA/evcc is gone while a latched target would
  otherwise charge forever, so the firmware **pauses** (`grid_failsafe: true`,
  [`SPECS.md`](SPECS.md) §7).
- **Non-retained**, invalid payloads ignored / last good held / surfaced in
  `last_error`.

---

## Inbound — enable gate (optional, #60)

**Topic:** `evc04/charge/enable` · **QoS 1** · **non-retained**

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
  commanded target** — an explicit off is the safest directive.
- **`enable: true`** → honor the commanded `target` (modulate / full charge as usual).
- **Default when never commanded: `true`** (honor the target). The last value is
  persisted to **NVS** on change so a reboot restores it; a no-target cold start still
  pauses (#59).
- **Invalid payloads ignored / last good held / surfaced in `last_error`** — a
  malformed publish never flips charging.
- **No staleness failsafe.** The gate is a latch, not an aged input; it does not time
  out.

### Why a separate topic

Overloading the single `target` topic to mean *both* "how much" and "on/off" makes an
evcc charger map its `enable` and `maxcurrent` set-plugins onto the same topic, where
they race: whenever `enable(true)` (≈ a tiny current = pause) lands as the last write,
the box never starts. A dedicated enable topic removes the collision (#60).

---

## Inbound — measurement probe (#135)

**Topic:** `evc04/charge/probe_over` · **QoS 1** · **non-retained**

A bench/diagnostic control that lifts the served meter answer to `MAX_BOX_AMPERE +
over` for the next 60 s, to characterise the box's cut threshold on hardware without
touching the command state:

```json
{ "ampere": 2.5 }
```

- Accepted range is `0 … 3.5 A` over the ceiling (0 clears); anything else is rejected
  and surfaced in `last_error` — a typo must not push the box to the cut.
- The probe **auto-expires** after 60 s (re-publish to extend it) and is deliberately
  **not** persisted — it is a manual measurement, never a reboot survivor.
- `charge_state` stays derived from the *un-probed* value, so a probe never makes evcc
  believe the charge stopped.

---

## Outbound — status

**Topic:** `evc04/charge/status` · **QoS 1** · **publish retained**

A single retained JSON object, republished whenever a field changes. Home Assistant
reads it via one MQTT sensor using `json_attributes_topic` + value templates.

```json
{
  "online": true,
  "target_ampere": 6.5,
  "grid_power_w": -3200,
  "reported_ampere": 16,
  "last_poll_age_s": 0.4,
  "grid_age_s": 1.1,
  "grid_failsafe": false,
  "charge_state": "C",
  "enabled": true,
  "last_error": null,
  "lb_current_ampere": 7,
  "cn28_feedback_stale": false,
  "probe_over_ampere": 0
}
```

| Field                 | Type           | Meaning |
| --------------------- | -------------- | ------- |
| `online`              | bool           | Firmware running and the control loop live. Set `false` by the broker via LWT if the device drops. |
| `target_ampere`       | number         | Last commanded target (post-clamp), ampere. |
| `grid_power_w`        | number         | Last grid-power heartbeat value, raw signed watts — a pass-through diagnostic. |
| `reported_ampere`     | number         | Per-phase current the slave is serving (including any active probe). |
| `last_poll_age_s`     | number         | Seconds since the EVC04 last polled us (~1 Hz; a growing value signals a dead RS485 link). |
| `grid_age_s`          | number         | Seconds since the last grid-power heartbeat; drives `grid_failsafe`. |
| `grid_failsafe`       | bool           | `true` while paused because the heartbeat went stale (> 15 s → the controller is gone). |
| `charge_state`        | string         | evcc charge status mirroring the box's **real** control-pilot state (#148): `A` (no vehicle), `B` (connected, not charging — also forced while we pause), `C` (charging), or `""` when the pilot is unknown (post-reboot blind window, stale CN28 feed, or an `F` fault). evcc's custom-charger `status` reads this. |
| `enabled`             | bool           | The enable gate (#60): `false` while charging is hard-paused regardless of the target. `true` by default. evcc's `enabled` read maps here. |
| `last_error`          | string or null | Reason for the most recent rejected input; `null` when healthy. |
| `lb_current_ampere`   | number         | The box's own per-car grant (`lb_current`) read from the CN28 LOG — the V4 control feedback. |
| `cn28_feedback_stale` | bool           | `true` when the grant feed is > 15 s old; the firmware then pauses (blind regulation never charges). |
| `probe_over_ampere`   | number         | Active measurement-probe lift over the ceiling, ampere (0 when no probe is running). |

> **`charge_state` mirrors the box's real pilot, guarded for evcc.** Since #148 it is
> derived from the CN28 LOG `S:` line (`cp_state`), not approximated from our command:
> `A`/`B`/`C` follow the pilot, except that our hard pause (reporting the full
> `MAX + PAUSE_MARGIN` level) forces `B` even while the box still reads `C`
> mid-ramp-down — so evcc's charge-power estimate drops to 0 as soon as we cut. A
> V4 *shed* report (`MAX+1..MAX+2`) is live modulation and stays `C`: flashing `B`
> mid-shed zeroes evcc's charge-power estimate and rattles its PV loop (live
> 2026-07-05). Because `cp_state` is transition-only and
> nullable (#117), an unknown pilot — the post-reboot blind window, a stale CN28 feed,
> or an `F` fault — yields `""`: evcc's status parser errors on an empty string and the
> loadpoint **retains its previous status**, so the blind window can never
> phantom-unplug or phantom-connect.
>
> The raw observation stays available as `cp_state` on the telemetry plane
> (`evc04/cn28/telemetry`, see [`cn28-log-protocol.md`](cn28-log-protocol.md)),
> `null` while unknown. It feeds only this status derivation — never the V4 control
> path, which regulates on the grant feedback alone.

### Last Will and Testament

On connect, the firmware registers an LWT on `evc04/charge/status` (retained) so an
ungraceful disconnect flips status to offline without any client polling:

```json
{ "online": false }
```

---

## Driving from evcc (recommended)

The charging brain is **evcc** (#28); the firmware is its **custom charger**:

- evcc `maxcurrent` → the **target** topic (ampere); evcc `enable` → the **enable**
  topic (`{"enable": true|false}`). Separate topics so on/off and the current setpoint
  never race on one write path (#60).
- evcc reads the **status** topic: `charge_state` (`A`/`B`/`C`, or `""` = pilot
  unknown → evcc retains its last status), `target_ampere`, and `enabled`.
- The **grid-power** topic is the liveness heartbeat — HA/evcc republishes the live
  grid power there every ~5 s (`{"watt": N}`); the firmware does not modulate on it,
  it only watches its cadence.
- **Timing:** evcc's control interval must exceed the box's ~5 s grant-eval cadence
  (§6) or the two loops hunt; use a matching interval + hysteresis.

A working evcc charger template, the min/max-current guidance, and the
nested-loop timing live in **[evcc.md](evcc.md)**.

---

## Home Assistant wiring (reference)

```yaml
mqtt:
  number:
    # Command: a number entity publishes {"ampere": <value>} to the target topic.
    - name: "EVC04 target current"
      command_topic: "evc04/charge/target"
      command_template: '{"ampere": {{ value }}}'
      min: 0
      max: 16          # = MAX_BOX_AMPERE / DIP setting at the installation (SPECS §2)
      step: 0.5
      unit_of_measurement: "A"

  sensor:
    # State: one sensor exposes the status JSON as attributes.
    - name: "EVC04 status"
      state_topic: "evc04/charge/status"
      value_template: "{{ 'online' if value_json.online else 'offline' }}"
      json_attributes_topic: "evc04/charge/status"

# Grid-power heartbeat: republish the live grid power every ~5 s, e.g. an automation
# on your grid-power sensor:
#   topic:   evc04/charge/grid_power
#   payload: { "watt": {{ states('sensor.grid_power') }} }
```

`max` must match the `MAX_BOX_AMPERE` / DIP setting for the box
([`SPECS.md`](SPECS.md) §2).

---

## Home Assistant auto-discovery

The firmware **self-registers** the read-only **CN28 telemetry** sensors on each
broker connect (retained discovery configs; device `evc04`, prefix `homeassistant`,
both fixed in the firmware) — HA creates one device with a sensor per telemetry field.

The **charge-control status** above is **not** auto-registered yet — read it with the
manual `sensor` in the wiring reference for now (#87 folds it into the same `evc04`
device once the control plane is proven on the box). **No command entity is ever
auto-published:** setting the target from HA would make HA a second *commander*, and
there must be exactly one ([`SPECS.md`](SPECS.md) §6). To command from HA, use the
manual `number` above — and then don't also run evcc.
