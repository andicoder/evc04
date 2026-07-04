# Driving the evc04 firmware from evcc

[evcc](https://evcc.io) is the charging **brain** (PV surplus, dynamic price,
target-by-departure). The on-box firmware is its **actuator**: a mode-agnostic
[custom charger](https://docs.evcc.io/en/docs/devices/chargers) reached over the
[MQTT contract](mqtt.md). evcc decides *how much* to charge; the firmware regulates
the box's own grant toward that current (the V4 loop, [`SPECS.md`](SPECS.md) §6).
No price/PV logic lives here.

There is **exactly one commander** for the target at a time — evcc **or** a plain
Home Assistant automation, never both. The `grid_power` topic is an independent
liveness heartbeat (HA/evcc publishes the live grid power there) and is *not* a
control surface.

## Mapping evcc ↔ our contract

| evcc charger field | our topic | mapping |
| ------------------ | --------- | ------- |
| `maxcurrent` (write) | `target` | `{"ampere": <A>}` — the throttle. evcc never sends below its own `mincurrent`. |
| `enable` (write) | `enable` | `{"enable": <bool>}` — the on/off gate, **independent** of the throttle (#60). `false` hard-pauses; `true` honors the current `maxcurrent`. |
| `enabled` (read) | `status` | our `enabled` field — reflects the gate directly. |
| `status` (read) | `status` | our `charge_state` field: `B` (connected, not charging) / `C` (charging). |

> **On/off and the current setpoint use separate topics.** Earlier the single `target`
> topic carried both, so `enable` and `maxcurrent` raced: an `enable(true)` (≈ a tiny
> current → pause) landing last would park the box paused and never start. The dedicated
> `enable` topic removes the collision (#60) — `enable` only opens/closes the gate,
> `maxcurrent` only sets the current.

> **No `A` (no vehicle) state.** A meter emulation has no control-pilot line, so we
> can't tell an unplugged car from a connected-but-idle one — `charge_state` is only
> ever `B` or `C`. evcc relies on its own vehicle/SoC detection for unplug, and any
> control-layer failure **pauses**, never charges on ([`SPECS.md`](SPECS.md) §7).

## Charger template

Drop this into your `evcc.yaml`. Replace the topic prefix (`evc04/…`) to match your
install. `${enable}` renders the boolean as `true`/`false`, so the payload is valid JSON.

```yaml
chargers:
  - name: evc04
    type: custom
    # Charging state: the firmware publishes charge_state = "B" | "C".
    status:
      source: mqtt
      topic: evc04/charge/status
      jq: .charge_state
      timeout: 90s        # > the firmware's status republish; flags a dead device
    # Charging enabled? Read the dedicated gate the enable plugin writes (#60).
    enabled:
      source: mqtt
      topic: evc04/charge/status
      jq: .enabled
      timeout: 90s
    # On/off gate — its own topic, independent of the throttle (#60).
    enable:
      source: mqtt
      topic: evc04/charge/enable
      payload: '{"enable": ${enable}}'
    # The actual throttle.
    maxcurrent:
      source: mqtt
      topic: evc04/charge/target
      payload: '{"ampere": ${maxcurrent}}'
```

## Loadpoint: min/max current

Honour the inner loop's stable band ([`SPECS.md`](SPECS.md) §6) so evcc never
commands a current the meter emulation can't hold:

```yaml
loadpoints:
  - title: Garage
    charger: evc04
    mincurrent: 6     # 3φ floor ≈ 6 A ≈ 4.1 kW; below it the box only does on/off
    maxcurrent: 16    # = MAX_BOX_AMPERE / the DIP setting (SPECS §2)
```

- The V4 grant loop was proven across the full **6–16 A staircase** on hardware;
  near the 6 A floor the box settles ~1 A high (a target of 6 holds at 7). Below
  6 A it is on/off only — that is the 3φ hardware floor, not evcc.
- `maxcurrent` must not exceed `MAX_BOX_AMPERE` (the physical DIP 4-5-6 setting):
  at or above the ceiling there is no headroom left to hand out and the box runs full.

## Nested-loop timing — the important part

Two feedback loops are stacked:

1. **Inner** (firmware ↔ EVC04): the box re-evaluates its grant every ~5 s and the
   firmware nudges it a step at a time, so a target change settles over **~30–60 s**.
2. **Outer** (evcc ↔ firmware): evcc reads its meters and re-commands `maxcurrent`.

If the **outer interval is shorter than the inner settle time, the two loops
hunt** (evcc keeps correcting before the box has settled). So:

- Set evcc's update `interval` to **≥ 60 s** (site-level `interval: 60s`), or rely
  on evcc's PV-mode **`enable`/`disable` delays** to add hysteresis:

  ```yaml
  loadpoints:
    - title: Garage
      # ...
      enable:
        delay: 60s      # wait before starting on surplus
      disable:
        delay: 90s      # wait before stopping — avoids flapping at the floor
  ```

- Keep evcc's current steps coarse (it already steps in whole ampere); avoid
  sub-amp chasing it can't observe through the inner loop anyway.

## Failsafe — the firmware always pauses (why it matters)

evcc only writes `target` on a control *decision*; it does **not** heartbeat the
target. When idle (e.g. PV mode with no surplus) it can stay quiet for minutes, and
because the target is a **latched** setpoint the firmware never ages it out — aging it
would deadlock (the box forgets → pauses → evcc never re-sends).

Liveness is carried instead by the **grid-power heartbeat**: HA/evcc republishes the
live grid power to `evc04/charge/grid_power` every ~5 s, and more than 15 s of silence
**pauses** the box (#136). A stale CN28 grant feed and an explicit `enable=false` also
pause. So any control-path fault (a **nightly router reconnect**, a dead broker, evcc
crashing) **stops** charging instead of starting it at the worst time — the safe
direction for a managed box (#52). This is unconditional; unlike the retired daemon
there is no `full_charge` opt-out. See [`SPECS.md`](SPECS.md) §7.

## Sanity check

With the firmware running and the broker reachable:

- evcc UI shows the loadpoint as **connected**; toggling the loadpoint on/off
  flips `evc04/charge/enable` between `{"enable": true}` and `{"enable": false}` (and
  `evc04/charge/status` `enabled` follows), while `evc04/charge/target` carries the throttle.
- `evc04/charge/status` `charge_state` reads `C` once current flows, `B` when paused.
- Commanding a PV-surplus current modulates across the proven 6–16 A staircase
  (settling ~1 A high near the floor); below 6 A it is on/off.

A plain Home-Assistant-only setup (number entity → `target`, sensor ← `status`)
also works for manual current + on/off; see [mqtt.md](mqtt.md).
