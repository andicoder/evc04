# CLAUDE.md

Guidance for Claude Code (and humans) working in this repo.

## What this is

`evc04-charge` controls a **Vestel EVC04-AC11-T2P** wallbox that has **no comms
module** by **emulating the Inepro PRO380 energy meter** its Power Optimizer polls
over RS485. An external controller (Home Assistant → Tibber price / PV surplus)
sets a target charge current over MQTT; we translate it to a fabricated household
current the charger reads, raising/lowering/pausing charging.

**Read [`SPECS.md`](SPECS.md) first** — it is the complete, self-contained brief
(hardware, wire protocol, register map, control math, service behaviour, MQTT
contract, open hardware questions). Do not start coding without it.

> Current state: **spec only, no implementation.** The first code lands against
> `SPECS.md`.

## Mandatory discipline

The general rules live in the global skills (`tdd`, `clean-code`,
`commit-conventions`, `pr-workflow`) — load them on demand. Only the
project-specific deltas are spelled out here:

- **TDD — non-negotiable** (`tdd` skill). Red test before production code; exempt
  only docs, packaging/CI config, pure renames. The Modbus framing, the Inepro
  float encoding, and the `report = fuse_limit − target` math are all pure
  functions — unit-test them against the verified frames in `SPECS.md` §5/§11,
  using those exact hex frames as fixtures.
- **Options first.** More than one reasonable approach → list 2–3 numbered options
  with tradeoffs and stop; let the human pick before you build.
- **No speculative abstraction; validate at boundaries only** (`clean-code`
  skill). Build only what the EVC04 actually polls (the 3 current registers) — no
  full Inepro map "just in case", no future-proofing, no dead code. Validate MQTT
  payloads, gateway bytes, and env config; trust internal callers.
- **Comments explain *why*, never *what*.** The wire protocol has surprising
  invariants (content-agnostic 1 Hz cadence, 8E1 parity, ABCD float order) —
  those deserve a comment; ordinary code does not.

## Intended stack & layout (open to change — propose before deviating)

- **Rust (2021 edition, stable)** on the `tokio` async runtime. Chosen for a
  single static binary (tiny `scratch`/distroless image), no-GC reliability for a
  24/7 daemon driving an 11 kW charger, and explicit byte-level control over the
  RTU framing and the ABCD float encoding. The verified frames in `SPECS.md`
  §5/§11 make the protocol a fixed target, so the discovery-speed advantage of the
  Python prototype no longer outweighs these.
- **Crates** (versions are floors; let Cargo resolve):
  - `tokio-modbus` ≥ 0.17 with the **`rtu-over-tcp-server`** feature — the EVC04
    polls us through a *transparent TCP↔RS485 gateway*, so we answer **RTU frames
    (CRC16, no MBAP header) over a plain TCP socket**. Not `tcp-server` (MBAP) and
    not `rtu-server` (only for a directly-attached serial port).
  - `rumqttc` ≥ 0.25 — async MQTT client for the target-current subscription and
    status publishing.
  - `tokio-serial` is **only** needed if the box is ever wired over a local serial
    port instead of the gateway — do not pull it in until that exists.
- Suggested layout:
  ```
  src/                     # bin + modules: modbus slave, mqtt client, control math, link/watchdog
  tests/                   # integration tests; protocol fixtures from SPECS.md §5/§11
  Cargo.toml               # deps, build config, clippy/test settings
  Dockerfile               # multi-stage: build static musl binary → scratch/distroless
  .github/workflows/       # lint + test on PR; build & push GHCR on tag
  ```
- **Lint/format:** `rustfmt` + `clippy` (deny warnings in CI). **Tests:**
  `cargo test`, with the §5/§11 hex frames as fixtures. Keep both green; CI
  enforces them.

## Configuration

Everything site-specific is an **environment variable** — see `SPECS.md` §7. No
config files, **no secrets committed** (no broker passwords, no IPs that matter).
The image must be generic enough to run at any installation.

## Deployment model

Own CI builds and pushes a container image to **GHCR**
(`ghcr.io/<owner>/evc04-charge:vX.Y.Z`). The consuming infra repo only pins a tag
and supplies env + manifests — **no application logic leaves this repo.**

## Git workflow

Follow the `commit-conventions` and `pr-workflow` skills. Project deltas:

- The default branch is `main`. Never push straight to it; branch off it with a
  `feat/…`, `fix/…`, `chore/…`, or `refactor/…` prefix and open a PR.
- **Never** commit/push/PR/merge unless explicitly asked.

## Safety reminders (real hardware involved)

- The control loop drives a **3-phase 11 kW charger**. The failsafe behaviour
  (what happens when the meter goes silent or the service crashes) is an **open
  question that must be resolved on hardware** — see `SPECS.md` §9. Treat
  fail-toward-no-charge as the default assumption until proven otherwise.
- Before changing anything on the physical box (DIP switches, wiring), photograph
  the current state for a clean revert.
