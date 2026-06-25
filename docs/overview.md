# evc04 — architecture overview

This repo collects two independent workstreams against the same wallbox, the
**Vestel EVC04-AC11-T2P**. They touch different ports of the box and have separate
failure domains.

## The wallbox, briefly

The "basic" EVC04 has no communication module, so it cannot be driven over
Modbus-TCP or OCPP. But it does two things we can exploit:

- It runs an internal **Power Optimizer** that polls an external energy meter
  (Inepro PRO380) over **RS485** and ramps the charge current against what that
  meter reports.
- It exposes an internal **CN28 "LOG" header** — a 3.3 V TTL UART that responds
  with per-phase metering and state when prompted.

## `charge/` — control via meter emulation (RS485)

`charge/` emulates the PRO380 the Power Optimizer polls. By feeding a fabricated
current it closes the optimizer's feedback loop, so the box **modulates** (or gates
on/off) charging. An external brain — Home Assistant or evcc — sets the target over
MQTT; this daemon only translates that into the meter value the box reads. The
charging logic (price, PV surplus, departure) lives in the controller, never here.
See [`../charge/SPECS.md`](../charge/SPECS.md) and
[`../charge/docs/mqtt.md`](../charge/docs/mqtt.md).

## `core/` + `firmware/` — telemetry via the CN28 LOG port (UART)

The CN28 path is **read-only** and on a **separate port** from control.
[`../firmware/`](../firmware/) (ESP32) taps CN28 over UART and bridges raw frames to
MQTT for remote probing; [`../core/`](../core/) holds the pure, host-tested decode
and command logic the firmware reuses. This is discovery tooling today — a
structured CN28 parser is the next step.

## Why one repo

Both workstreams share the same hardware facts (mainboard layout, connectors, the
meter's Modbus map) and the same issue tracker. Keeping them together avoids
duplicating that knowledge across separate repos.
