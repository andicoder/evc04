#!/usr/bin/env bash
# Open the serial monitor on the flashed CN28 prober (evc04#66) — no rebuild.
# Streams the firmware's UART0 log over USB. In the monitor: CTRL+R resets the
# chip (replays the boot log), CTRL+C exits. espflash hard-resets on connect, so
# you see a fresh boot by default.
#
#   ./monitor.sh                 # auto-detect the port
#   ./monitor.sh /dev/ttyUSB1    # pin a specific port
#
# Run this in a real terminal (it needs a TTY for the keyboard controls).
set -euo pipefail

cd "$(dirname "$0")"

args=(monitor)
[ -n "${1:-}" ] && args+=(--port "$1")
exec espflash "${args[@]}"
