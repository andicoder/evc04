# evc04-charge

[![CI](https://github.com/andicoder/evc04-charge/actions/workflows/ci.yml/badge.svg)](https://github.com/andicoder/evc04-charge/actions/workflows/ci.yml)
[![Release](https://github.com/andicoder/evc04-charge/actions/workflows/release.yml/badge.svg)](https://github.com/andicoder/evc04-charge/actions/workflows/release.yml)
[![GHCR image](https://ghcr-badge.egpl.dev/andicoder/evc04-charge/latest_tag?label=ghcr.io&color=blue&logo=docker)](https://github.com/andicoder/evc04-charge/pkgs/container/evc04-charge)

Smart, price-aware charge control for the **Vestel EVC04-AC11-T2P** ("basic"
Home variant) wallbox — a box that ships with **no communication module**, so it
cannot be controlled over Modbus-TCP or OCPP the way the SW/Connect variants can.

This service controls the charger **indirectly**, by **emulating the Inepro
PRO380 energy meter** that the EVC04's built-in *Power Optimizer* polls over
RS485. The charger computes its available charge current as
`fuse_limit − household_current`; by reporting a fabricated household current we
make the box raise, lower, pause, or resume charging. An external controller
(e.g. Home Assistant following Tibber day-ahead prices or PV surplus) sets the
target over MQTT.

```
Home Assistant ──MQTT──▶ evc04-charge ──RS485 (Modbus RTU slave)──▶ EVC04 Power Optimizer
 (Tibber price /          (this repo:        via a TCP↔RS485 gateway      (polls us as a meter
  PV surplus → target)     meter emulator)    e.g. Waveshare RS485-TO-ETH   at 1 Hz, FC03)
```

> **Status: specification only.** This repo currently contains the design and the
> reverse-engineered protocol (`SPECS.md`). No implementation yet. A fresh
> contributor should be able to build the service from `SPECS.md` alone, with no
> outside context.

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

## License

TBD.
