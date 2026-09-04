#!/usr/bin/env bash
# Native esp build for the CN28 prober firmware (evc04#66).
# Sources the espup env, adds the libxml2/ICU compat shim esp-clang needs on
# rolling distros (staged by bootstrap.sh), and defaults WiFi/MQTT to
# placeholders so the build never fails on an unset env! — export real values
# before flashing (WIFI_SSID, WIFI_PASSWORD, MQTT_URL, OTLP_LOGS_URL).
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
# Full OTLP *logs* endpoint (#3) — the collector's signal URL, not its base.
: "${OTLP_LOGS_URL:=http://localhost:4318/v1/logs}"
# OTLP_LOGS_AUTH is deliberately NOT defaulted: the firmware reads it with
# option_env!, so an unset variable means "post unauthenticated" rather than a
# build failure or a bogus Authorization header.
export WIFI_SSID WIFI_PASSWORD MQTT_URL OTLP_LOGS_URL
[ -n "${OTLP_LOGS_AUTH:-}" ] && export OTLP_LOGS_AUTH

cd "$(dirname "$0")"

# The OTA partition table (#76) must be referenced by ABSOLUTE path: esp-idf-sys
# resolves CONFIG_PARTITION_TABLE_CUSTOM_FILENAME against ESP-IDF's PROJECT_DIR,
# which is the build out-dir, not this crate — a relative "partitions.csv" is not
# found there. Inject it as an extra sdkconfig.defaults layered on top of the
# committed one (esp-idf-sys splits on ';', last value wins).
FW_DIR="$(pwd)"
PART_DEFAULTS="$FW_DIR/target/sdkconfig.partition.defaults"
mkdir -p "$FW_DIR/target"
printf 'CONFIG_PARTITION_TABLE_CUSTOM_FILENAME="%s"\n' "$FW_DIR/partitions.csv" > "$PART_DEFAULTS"
export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;$PART_DEFAULTS"

exec cargo build "$@"
