# Driving evc04-charge from evcc

[evcc](https://evcc.io) is the charging **brain** (PV surplus, dynamic price,
target-by-departure). This service is its **actuator**: a mode-agnostic
[custom charger](https://docs.evcc.io/en/docs/devices/chargers) reached over the
[MQTT contract](mqtt.md). evcc decides *how much* to charge; we translate that into
the fabricated meter current the EVC04 reads (closed loop, [`SPECS.md`](../SPECS.md)
§6). No price/PV logic lives here.

There is **exactly one commander** for the target at a time — evcc **or** a plain
Home Assistant automation, never both. The `measured` topic is independent feedback
(HA/evcc publishes the live grid current there) and is *not* a control surface.

## Mapping evcc ↔ our contract

| evcc charger field | our topic | mapping |
| ------------------ | --------- | ------- |
| `maxcurrent` (write) | `target` | `{"ampere": <A>}` — the throttle. evcc never sends below its own `mincurrent`. |
| `enable` (write) | `target` | `false` → `{"ampere": 0}` (below `MIN_CHARGE_AMPERE` → **pause**); `true` is followed by a `maxcurrent` write that lands the charging value. |
| `enabled` (read) | `status` | `target_ampere >= MIN_CHARGE_AMPERE` → charging is commanded. |
| `status` (read) | `status` | our `charge_state` field: `B` (connected, not charging) / `C` (charging). |

> **`enable=true` is transient.** With evcc's `${enable:%d}` substitution, `enable`
> can only publish `0`/`1`, so `true` momentarily commands `{"ampere": 1}` (a pause).
> evcc always issues `maxcurrent` (≥ its `mincurrent`) immediately after
> `Enable(true)`, so the charging value lands a fraction of a second later; the
> offset soft-ramp ([`SPECS.md`](../SPECS.md) §6) absorbs the blip. `enable` exists
> only to force the **pause** on `false`.

> **No `A` (no vehicle) state.** A meter emulation has no control-pilot line, so we
> can't tell an unplugged car from a connected-but-idle one — `charge_state` is only
> ever `B` or `C`. evcc relies on its own vehicle/SoC detection for unplug, and the
> failsafe direction is full charge regardless ([`SPECS.md`](../SPECS.md) §9).

## Charger template

Drop this into your `evcc.yaml`. Replace the topic prefix (`evc04/…`) and the `6`
in `enabled` with your `MIN_CHARGE_AMPERE` if you changed it.

```yaml
chargers:
  - name: evc04
    type: custom
    # Charging state: our service publishes charge_state = "B" | "C".
    status:
      source: mqtt
      topic: evc04/status
      jq: .charge_state
      timeout: 90s        # > the service's 2 s status republish; flags a dead service
    # Charging enabled? Derived from the commanded target.
    enabled:
      source: mqtt
      topic: evc04/status
      jq: .target_ampere >= 6   # = MIN_CHARGE_AMPERE
      timeout: 90s
    # Pause on disable: {"ampere": 0} is below MIN_CHARGE_AMPERE → hard pause.
    enable:
      source: mqtt
      topic: evc04/target
      payload: '{"ampere": ${enable:%d}}'
    # The actual throttle.
    maxcurrent:
      source: mqtt
      topic: evc04/target
      payload: '{"ampere": ${maxcurrent}}'
```

## Loadpoint: min/max current

Honour the inner loop's stable band ([`SPECS.md`](../SPECS.md) §6) so evcc never
commands a current the meter emulation can't hold:

```yaml
loadpoints:
  - title: Garage
    charger: evc04
    mincurrent: 6     # 3φ floor ≈ 6 A ≈ 4.1 kW; below it the box only does on/off
    maxcurrent: 16    # = MAX_BOX_AMPERE / the DIP setting (SPECS §2/§9)
```

- The **stable modulation band is ~9–15 A** with home-automation-speed measurement;
  6–8 A hunts. If evcc's PV surplus drops toward the floor, expect on/off rather
  than smooth modulation there — that is the hardware, not evcc.
- `maxcurrent` must not exceed `MAX_BOX_AMPERE` (the physical DIP 4-5-6 setting):
  above the ceiling the offset math saturates and the box just runs full.

## Nested-loop timing — the important part

Two feedback loops are stacked:

1. **Inner** (this service ↔ EVC04): settles in **~30–60 s** after a target change
   — the offset soft-rampere and the box's own optimizer re-converges.
2. **Outer** (evcc ↔ this service): evcc reads its meters and re-commands
   `maxcurrent`.

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

## Failsafe direction — `pause` (default; why it matters)

evcc only writes `target` on a control *decision*; it does **not** heartbeat. When
idle (e.g. PV mode with no surplus) it can stay quiet for minutes, and its idle
cadence is unbounded — so the target will routinely age out past
`TARGET_TIMEOUT_SECONDS`. Likewise any control-path blip (e.g. a **nightly router
reconnect**) can age out `measured`.

The service therefore defaults both failsafes to **`pause`** (#52), so any such fault
**stops** charging instead of starting it at the worst time: a stale evcc pause stays
a pause, a stale measurement stops the loop. **No env needed** — it's the default.

```
# defaults (shown for clarity; you don't need to set these for evcc)
TARGET_FAILSAFE=pause
MEASURED_FAILSAFE=pause
```

Only switch to `full_charge` for a Home-Assistant-automation-only / unmanaged box
where charging-on-fault is the desired baseline. See [`SPECS.md`](../SPECS.md) §9.

## Sanity check

With the service running and the broker reachable:

- evcc UI shows the loadpoint as **connected**; toggling the loadpoint on/off
  flips our `target` between a charging current and `{"ampere": 0}` (watch
  `evc04/target` and `evc04/status`).
- `evc04/status` `charge_state` reads `C` once current flows, `B` when paused.
- Commanding a PV-surplus current in the 9–15 A band modulates; near 6 A it
  reverts to on/off.

A plain Home-Assistant-only setup (number entity → `target`, sensor ← `status`)
also works for manual current + on/off; see [mqtt.md](mqtt.md).
