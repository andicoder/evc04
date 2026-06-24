#!/usr/bin/env bash
# Build the CN28 prober firmware inside the pinned esp toolchain image (evc04#66).
# No host esp toolchain needed — identical on any machine (Manjaro, Ubuntu, CI).
#
# Usage:
#   export WIFI_SSID=... WIFI_PASSWORD=... MQTT_URL=mqtt://user:pass@host:1883
#   ./docker-build.sh                 # debug build (placeholders if creds unset)
#   ./docker-build.sh --release
#
# Output ELF lands on the host at:
#   target/xtensa-esp32-espidf/<profile>/evc04-cn28-prober
# Flashing stays on the host (USB device): espflash flash --monitor <elf>.
set -euo pipefail

IMAGE="${IDF_RUST_IMAGE:-espressif/idf-rust:esp32_1.95.0.0}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE="$REPO_ROOT/firmware/.docker-cache"
mkdir -p "$CACHE/cargo" "$CACHE/home"

# WiFi/MQTT creds are baked into the binary at compile time (env!); placeholders
# still produce a valid compile check.
: "${WIFI_SSID:=placeholder}"
: "${WIFI_PASSWORD:=placeholder}"
: "${MQTT_URL:=mqtt://localhost:1883}"

# RUSTUP_HOME stays the image's (read-only esp toolchain); HOME + CARGO_HOME are
# redirected into the gitignored cache so deps/ESP-IDF tools persist across runs
# and the build works regardless of the host uid.
exec docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e HOME=/project/firmware/.docker-cache/home \
  -e CARGO_HOME=/project/firmware/.docker-cache/cargo \
  -e RUSTUP_HOME=/home/esp/.rustup \
  -e WIFI_SSID -e WIFI_PASSWORD -e MQTT_URL \
  -v "$REPO_ROOT":/project \
  -w /project/firmware \
  "$IMAGE" \
  cargo build "$@"
