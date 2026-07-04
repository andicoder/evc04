# evc04

[![core](https://github.com/andicoder/evc04/actions/workflows/core.yml/badge.svg)](https://github.com/andicoder/evc04/actions/workflows/core.yml)

Everything for the **Vestel EVC04-AC11-T2P** wallbox — the "basic" Home variant
that ships with **no communication module**, so it can't be driven over Modbus-TCP
or OCPP. This monorepo holds the on-box control firmware and the read-side tooling that
taps and reverse-engineers the box.

See [`docs/overview.md`](docs/overview.md) for how the pieces fit together.

## Sub-projects

### [`core/`](core/) — CN28 protocol + control logic (`evc04-cn28-core`)
Pure, host-tested `no_std` Rust: the CN28 LOG decode plus the meter-emulation
control math (`reported = MAX_BOX − target`) and the Modbus/Inepro PRO380 framing.
The shared brain the firmware links; built and tested on the stable toolchain.

### [`firmware/`](firmware/) — ESP32 in-box controller (`evc04-cn28-prober`)
`esp-idf` firmware running on the ESP32 inside the wallbox. It taps CN28 over UART,
runs the control loop, and answers the *Power Optimizer*'s RS485 poll as an emulated
**Inepro PRO380** meter, so an external controller (Home Assistant / evcc) can
modulate charging over MQTT — the box has no comms module of its own. Bridges to
MQTT and self-updates over OTA. Built and flashed locally, not in CI.

## Reference

- Hardware, wire protocol, register map, control math: [`docs/SPECS.md`](docs/SPECS.md)
- MQTT contract: [`docs/mqtt.md`](docs/mqtt.md) · evcc integration: [`docs/evcc.md`](docs/evcc.md)

## License

TBD.
