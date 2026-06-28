#!/usr/bin/env bash
# One-shot over-the-air firmware push for the CN28 prober (evc04#76).
#
# The image is NOT hosted anywhere permanent: this script builds the release
# binary, serves it from a *temporary* local HTTP server, tells the device to
# pull it (MQTT topic evc04/device/ota), waits for the result on
# evc04/device/ota/status,
# then tears the server down. Nothing is left running.
#
#   ./ota_push.sh                # build, serve, push, wait, clean up
#
# Requirements: espflash, python3, mosquitto_pub/mosquitto_sub (mosquitto-clients).
# Creds + broker come from ./.env (same file flash.sh uses): WIFI_SSID,
# WIFI_PASSWORD, MQTT_URL are baked into the image; MQTT_URL also locates the
# broker for the trigger/status messages.
#
# Overrides: OTA_HOST_IP (address the ESP reaches us on), OTA_PORT (default 8000),
# OTA_TIMEOUT (seconds to wait for a result, default 180).
set -euo pipefail
cd "$(dirname "$0")"

for tool in espflash python3 mosquitto_pub mosquitto_sub; do
  command -v "$tool" >/dev/null || {
    echo "!! missing '$tool' (mosquitto_* come from the mosquitto-clients package)" >&2
    exit 1
  }
done

if [ -f ./.env ]; then
  set -a; . ./.env; set +a
else
  echo "!! no ./.env — need MQTT_URL (and WiFi/MQTT creds to bake in)." >&2
  exit 1
fi
: "${MQTT_URL:?MQTT_URL must be set in ./.env}"

# Parse mqtt://[user:pass@]host[:port] into broker coordinates for mosquitto_*.
rest="${MQTT_URL#*://}"
creds="${rest%@*}"; hostport="${rest##*@}"
MQ_HOST="${hostport%%:*}"
MQ_PORT="${hostport##*:}"; [ "$MQ_PORT" = "$hostport" ] && MQ_PORT=1883
MQ_AUTH=()
if [ "$creds" != "$rest" ]; then
  MQ_AUTH+=(-u "${creds%%:*}" -P "${creds#*:}")
fi

OTA_PORT="${OTA_PORT:-8000}"
OTA_TIMEOUT="${OTA_TIMEOUT:-180}"
# The IP the ESP can reach us on: first global IPv4, override with OTA_HOST_IP.
OTA_HOST_IP="${OTA_HOST_IP:-$(ip -4 -o addr show scope global | awk '{print $4}' | cut -d/ -f1 | head -1)}"
[ -n "$OTA_HOST_IP" ] || { echo "!! could not detect a LAN IP; set OTA_HOST_IP" >&2; exit 1; }
# Bind the server to all interfaces, not just OTA_HOST_IP: on a multi-homed host
# (docker/bridge IPs alongside the LAN address) binding to one IP can leave the
# ESP's SYN unanswered, so the device silently never starts the download. The URL
# still advertises OTA_HOST_IP — only the listen address is widened.
OTA_BIND="${OTA_BIND:-0.0.0.0}"

# env! is compile-time and Cargo does not track env changes, so force a rebuild
# of the baked creds (same reasoning as flash.sh).
touch src/main.rs
./build.sh --release

ELF="target/xtensa-esp32-espidf/release/evc04-cn28-prober"
SERVE_DIR="$(mktemp -d)"
STATUS_LOG="$(mktemp)"
HTTP_PID=""; SUB_PID=""
cleanup() {
  [ -n "$HTTP_PID" ] && kill "$HTTP_PID" 2>/dev/null || true
  [ -n "$SUB_PID" ] && kill "$SUB_PID" 2>/dev/null || true
  rm -rf "$SERVE_DIR" "$STATUS_LOG"
}
trap cleanup EXIT

# Application image for OTA (app segment only — the bootloader/partition table
# already live on the device).
espflash save-image --chip esp32 "$ELF" "$SERVE_DIR/fw.bin"
URL="http://$OTA_HOST_IP:$OTA_PORT/fw.bin"

python3 -m http.server "$OTA_PORT" --bind "$OTA_BIND" --directory "$SERVE_DIR" >/dev/null 2>&1 &
HTTP_PID=$!

# Subscribe to OTA status BEFORE triggering, so the non-retained progress messages
# aren't missed; give the subscription a moment to register.
mosquitto_sub -h "$MQ_HOST" -p "$MQ_PORT" "${MQ_AUTH[@]}" -t evc04/device/ota/status > "$STATUS_LOG" &
SUB_PID=$!
sleep 1

echo ">> serving $(du -h "$SERVE_DIR/fw.bin" | cut -f1) image at $URL"
echo ">> triggering OTA via $MQ_HOST:$MQ_PORT"
mosquitto_pub -h "$MQ_HOST" -p "$MQ_PORT" "${MQ_AUTH[@]}" -t evc04/device/ota -m "$URL"

deadline=$(( $(date +%s) + OTA_TIMEOUT ))
result=2
while [ "$(date +%s)" -lt "$deadline" ]; do
  if grep -q '^ok$' "$STATUS_LOG"; then result=0; break; fi
  if line=$(grep -m1 '^failed' "$STATUS_LOG"); then echo ">> $line"; result=1; break; fi
  sleep 1
done

case "$result" in
  0) echo ">> OTA ok — device rebooting into the new image" ;;
  1) echo "!! OTA failed (see message above)" >&2 ;;
  *) echo "!! timed out after ${OTA_TIMEOUT}s with no result" >&2 ;;
esac
exit "$result"
