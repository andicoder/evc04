# CLAUDE.md — monorepo root

`andicoder/evc04` is a monorepo for the Vestel EVC04-AC11-T2P wallbox. Each
sub-project carries its own guidance; this file is just the map.

## Public repo — do not leak the private wiki

This repository is **public**. The personal wiki (and the `private-infra-repo` repo) is
**private**. Never link to, quote, or reference the private wiki in any repo
content — README, `docs/`, code comments, commit messages, or PR/issue bodies.
Keep public docs self-contained, pointing only at in-repo material.

## Sub-projects

- [`charge/`](charge/) — the control daemon (Inepro PRO380 emulation over RS485).
  **Read [`charge/CLAUDE.md`](charge/CLAUDE.md)** for the daemon's discipline,
  stack, and safety rules; the brief is [`charge/SPECS.md`](charge/SPECS.md).
- [`core/`](core/) — `evc04-cn28-core`: pure `no_std` CN28 protocol logic,
  host-tested on the stable toolchain.
- [`firmware/`](firmware/) — `evc04-cn28-prober`: ESP32 `esp-idf` firmware tapping
  CN28 over UART, bridged to MQTT. Built and flashed locally, not in CI.
- [`docs/`](docs/) — cross-project [`overview.md`](docs/overview.md).

## CI

`ci.yml` and `release.yml` cover **`charge/`** only (paths-scoped). `firmware/`
can't build in CI; a dedicated `core/` host-test job is a follow-up.
