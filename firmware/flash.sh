#!/usr/bin/env bash
# Build + flash + monitor the CN28 prober firmware on the host (evc04#66).
# Sources ./.env so the WiFi/MQTT creds are baked into the binary (env!), then
# builds a release binary via build.sh and flashes over USB with espflash.
# Release only — the prober runs unattended on the box, no debug build is shipped.
#
#   ./flash.sh                # release build, flash + monitor
set -euo pipefail

cd "$(dirname "$0")"

# Creds are compiled in via env!; pull them from .env if present. Without it the
# build falls back to placeholders (build.sh) and the box won't join WiFi.
if [ -f ./.env ]; then
  set -a; . ./.env; set +a
else
  echo "!! no ./.env — building with placeholder creds; the firmware won't connect." >&2
fi

# env! is evaluated at compile time and Cargo does NOT track env var changes, so
# a rebuild after editing .env would otherwise reuse the stale baked-in values.
# Touching the source forces main.rs (and its env!s) to recompile.
touch src/main.rs

./build.sh --release

exec espflash flash --monitor "target/xtensa-esp32-espidf/release/evc04-cn28-prober"
