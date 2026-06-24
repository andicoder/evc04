#!/usr/bin/env bash
# Native esp build for the CN28 prober firmware (evc04#66).
# Sources the espup env, adds the libxml2/ICU compat shim esp-clang needs on
# rolling distros (staged by bootstrap.sh), and defaults WiFi/MQTT to
# placeholders so the build never fails on an unset env! — export real values
# before flashing.
#
#   ./build.sh                # debug build
#   ./build.sh --release
set -euo pipefail

[ -f "$HOME/export-esp.sh" ] || {
  echo "Missing $HOME/export-esp.sh — run ./bootstrap.sh first." >&2
  exit 1
}
# shellcheck disable=SC1091
. "$HOME/export-esp.sh"

COMPAT="$HOME/.espressif/compat-libs"
[ -d "$COMPAT" ] && export LD_LIBRARY_PATH="$COMPAT:${LD_LIBRARY_PATH:-}"

: "${WIFI_SSID:=placeholder}"
: "${WIFI_PASSWORD:=placeholder}"
: "${MQTT_URL:=mqtt://localhost:1883}"
export WIFI_SSID WIFI_PASSWORD MQTT_URL

cd "$(dirname "$0")"
exec cargo build "$@"
