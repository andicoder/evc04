# evc04-charge

[![CI](https://github.com/andicoder/evc04-charge/actions/workflows/ci.yml/badge.svg)](https://github.com/andicoder/evc04-charge/actions/workflows/ci.yml)
[![Release](https://github.com/andicoder/evc04-charge/actions/workflows/release.yml/badge.svg)](https://github.com/andicoder/evc04-charge/actions/workflows/release.yml)
[![GHCR image](https://ghcr-badge.egpl.dev/andicoder/evc04-charge/latest_tag?label=ghcr.io&color=blue&logo=docker)](https://github.com/andicoder/evc04-charge/pkgs/container/evc04-charge)

Smart, price-aware charge control for the **Vestel EVC04-AC11-T2P** ("basic"
Home variant) wallbox — a box that ships with **no communication module**, so it
cannot be controlled over Modbus-TCP or OCPP the way the SW/Connect variants can.

This service controls the charger **indirectly**, by **emulating the Inepro
PRO380 energy meter** that the EVC04's built-in *Power Optimizer* polls over
RS485. The optimizer runs a **closed feedback loop** that ramps the charge current
until the measured total reaches a fuse limit; by feeding it a value that tracks
the **live measured current** we close that loop and the box **modulates**, and by
feeding a static value we get **on/off** gating (see [`SPECS.md`](SPECS.md) §6). An
external controller (Home Assistant, or **evcc** as a custom charger) sets the
target and publishes the live measured current over MQTT — the charging brain
(price / PV / departure) stays there, not in this mode-agnostic service.

```
Home Assistant / evcc ──MQTT──▶ evc04-charge ──RS485 (Modbus RTU slave)──▶ EVC04 Power Optimizer
 (price / PV brain;             (this repo:        via a TCP↔RS485 gateway      (polls us as a meter
  target + measured current)     meter emulator)    e.g. Waveshare RS485-TO-ETH   at 1 Hz, FC03)
```

> **Status: in development.** The meter emulation, gateway link + watchdog, MQTT
> control surface, env config, and target-staleness failsafe are implemented (v0.1,
> currently **on/off** via the static model). **Closed-loop current modulation** —
> feeding a live measured current so the box modulates — is in progress (#21).
> `SPECS.md` remains the complete, self-contained brief.

## What this is (and isn't)

- ✅ A small, **generic, env-configured** service: Modbus-RTU slave + MQTT client
  + watchdog. No site-specific secrets or infrastructure assumptions baked in.
- ✅ Intended to ship as a container image (own CI → GHCR), deployed elsewhere by
  pinning an image tag.
- ❌ Not an OCPP server, not a Modbus-TCP *control* client (the basic box has no
  module for either), and not specific to one home's setup.

## Start here

Read **[`SPECS.md`](SPECS.md)** — it is the complete, self-contained brief:
hardware facts, the full reverse-engineered RS485 protocol and register map, the
control math, the service behaviour, the MQTT contract, configuration, and the
open questions that still need answering on real hardware (with a car plugged in).

The finalised **MQTT control contract** (target/status payload schemas, retention,
failsafe semantics, Home Assistant wiring) lives in **[`docs/mqtt.md`](docs/mqtt.md)**.

## License

TBD.
