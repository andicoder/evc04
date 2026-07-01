# CLAUDE.md — evc04 monorepo

`andicoder/evc04` controls a **Vestel EVC04-AC11-T2P** wallbox that has **no comms
module**, by **emulating the Inepro PRO380 energy meter** its Power Optimizer polls over
RS485. An external controller (Home Assistant / evcc → Tibber price / PV surplus) sets a
target charge current over MQTT; we feed the box a fabricated household current, so it
raises, lowers, or pauses charging.

The active implementation now lives on the **ESP32 inside the box** (`core/` +
`firmware/`). The original k3s daemon (`charge/`) is being **retired** — new control
work goes in core+firmware, not charge.

## Public repo — do not leak the private wiki

This repository is **public**. The personal wiki and the `private-infra-repo` repo are
**private**. Never link to, quote, or reference them in any repo content — README,
`docs/`, code comments, commit messages, or PR/issue bodies. Keep public docs
self-contained, pointing only at in-repo material.

## Sub-projects

- [`core/`](core/) — `evc04-cn28-core`: pure `no_std` CN28 protocol + meter-emulation
  control logic, host-tested on the stable toolchain. The shared brain the firmware
  links (and the daemon linked).
- [`firmware/`](firmware/) — `evc04-cn28-prober`: ESP32 `esp-idf` firmware — taps CN28
  over UART, runs the meter-emulation control loop + RS485 PRO380 slave, bridges to MQTT,
  self-updates over OTA. Built and flashed **locally**, never in CI.
- [`charge/`](charge/) — the retiring control daemon. Its brief,
  [`charge/SPECS.md`](charge/SPECS.md), stays the **canonical reference** the firmware
  mirrors: hardware, wire protocol, register map, control math, MQTT contract, verified
  frames.
- [`docs/`](docs/) — cross-project [`overview.md`](docs/overview.md).

## Mandatory discipline

The general rules live in the global skills (`tdd`, `clean-code`, `commit-conventions`,
`pr-workflow`) — load them on demand. The project deltas:

- **TDD — non-negotiable** (`tdd` skill). Red test before production code; exempt only
  docs, packaging/CI config, pure renames. The pure logic in `core/` (Modbus/Inepro
  framing, the `reported = MAX_BOX − target` control math, the CN28 LOG decode) is
  unit-tested against the verified hex frames in [`charge/SPECS.md`](charge/SPECS.md)
  §5/§11, using those exact fixtures.
- **Options first.** More than one reasonable approach → list 2–3 numbered options with
  tradeoffs and stop; let the human pick before you build.
- **No speculative abstraction; validate at boundaries only** (`clean-code` skill). Build
  only what the box actually needs — no full Inepro map "just in case", no future-proofing,
  no dead code. Validate MQTT payloads, wire bytes, and config; trust internal callers.
- **Comments explain *why*, never *what*.** The wire protocol has surprising invariants
  (content-agnostic ~1 Hz cadence, 8E1 parity, ABCD float order) — those deserve a
  comment; ordinary code does not.

## Git workflow

Follow the `commit-conventions` and `pr-workflow` skills. Project deltas:

- The default branch is `main`. Never push straight to it; branch off with a `feat/…`,
  `fix/…`, `chore/…`, or `refactor/…` prefix and open a PR. **Merge by rebase only.**
- **Never** commit/push/PR/merge unless explicitly asked.
- English only for commit subjects/bodies and PR/issue titles/bodies.

## Safety reminders (real hardware involved)

The control loop drives a **3-phase 11 kW charger**.

- **Failsafe direction is configurable** per channel (`TARGET_FAILSAFE` /
  `MEASURED_FAILSAFE`), **default `pause`**: for an evcc/HA-managed box, any control-layer
  failure (stale target or measurement, cold start past the grace window) must **stop**
  charging, not start it at the worst time. `full_charge` (`reported = 0`, the meterless
  full-11 kW baseline) is opt-in for an unmanaged box. **Fuse protection is out of scope**
  — the installation and the DIP-set limit handle it.
- **A silent meter is dangerous.** With the Power Optimizer enabled, a *silent* meter
  **hard-faults the box (solid red, no charge)** — it does **not** fall back to full
  charge. So the device must keep answering the box's ~1 Hz RS485 poll: auto-restart /
  reboot to recover, overlap rollouts (the new instance answers before the old stops), and
  never let a worker wedge the slave. See [`charge/SPECS.md`](charge/SPECS.md) §9.
- Before changing anything on the physical box (DIP switches, wiring), photograph the
  current state for a clean revert.

## CI

- **`ci.yml`** — lint + test for the **charge daemon** (`charge/**`-path-scoped).
- **`core.yml`** — host tests for **`core/`** (`core/**`-path-scoped).
- **`release.yml`** — builds/publishes the charge daemon image to GHCR on tag.
- **`firmware/`** has no CI — the Espressif Xtensa toolchain and the device dependency
  keep it local-only (`cargo make build` / `ota_push.sh`).
