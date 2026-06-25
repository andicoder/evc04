# evc04

[![CI](https://github.com/andicoder/evc04/actions/workflows/ci.yml/badge.svg)](https://github.com/andicoder/evc04/actions/workflows/ci.yml)
[![Release](https://github.com/andicoder/evc04/actions/workflows/release.yml/badge.svg)](https://github.com/andicoder/evc04/actions/workflows/release.yml)
[![GHCR image](https://ghcr-badge.egpl.dev/andicoder/evc04-charge/latest_tag?label=ghcr.io&color=blue&logo=docker)](https://github.com/andicoder/evc04/pkgs/container/evc04-charge)

Everything for the **Vestel EVC04-AC11-T2P** wallbox — the "basic" Home variant
that ships with **no communication module**, so it can't be driven over Modbus-TCP
or OCPP. This monorepo holds the control daemon and the read-side tooling that taps
and reverse-engineers the box.

See [`docs/overview.md`](docs/overview.md) for how the pieces fit together.

## Sub-projects

### [`charge/`](charge/) — price/PV-aware charge control
The production daemon. It emulates the **Inepro PRO380** energy meter the EVC04's
built-in *Power Optimizer* polls over RS485, so an external controller (Home
Assistant, or **evcc**) can modulate charging over MQTT — the box has no comms
module of its own. Ships as a container image to GHCR.

- Brief: [`charge/SPECS.md`](charge/SPECS.md)
- MQTT contract: [`charge/docs/mqtt.md`](charge/docs/mqtt.md) · evcc integration: [`charge/docs/evcc.md`](charge/docs/evcc.md)
- Image: `ghcr.io/andicoder/evc04-charge:vX.Y.Z` (image name kept across the repo rename)

### [`core/`](core/) — CN28 protocol logic (`evc04-cn28-core`)
Pure, host-tested `no_std` Rust: command decoding and byte dumps for the CN28 LOG
port. Shared by the firmware; built and tested on the stable toolchain.

### [`firmware/`](firmware/) — ESP32 CN28 remote prober (`evc04-cn28-prober`)
`esp-idf` firmware that taps the wallbox's CN28 "LOG" header over UART and bridges
raw frames to MQTT, so the LOG protocol can be probed remotely. Built and flashed
locally, not in CI.

## License

TBD.
