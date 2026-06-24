#!/usr/bin/env bash
# Bootstrap the local toolchain for building the CN28 prober firmware (evc04#66).
#
# Installs, idempotently:
#   - system build deps for ESP-IDF (cmake, ninja, dfu-util, ccache, ...)
#   - the cargo tools: espup, ldproxy, espflash
#   - Espressif's Xtensa Rust toolchain via `espup install`
#
# It does NOT build or flash — after it finishes, source the export file and run
# the build (see the printed next steps). Firmware is built/flashed locally only,
# never in CI.
set -euo pipefail

note() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# espup drives rustup; a distro-packaged rustc without rustup will not work.
have rustup || die "rustup not found. Install Rust via https://rustup.rs first."

# ── System build dependencies (need sudo) ───────────────────────────────────
SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
install_system_deps() {
  if have pacman; then
    note "Installing system deps via pacman"
    $SUDO pacman -S --needed --noconfirm cmake ninja dfu-util ccache libusb python git
  elif have apt-get; then
    note "Installing system deps via apt"
    $SUDO apt-get update
    $SUDO apt-get install -y cmake ninja-build dfu-util ccache libusb-1.0-0 python3 python3-venv git
  elif have dnf; then
    note "Installing system deps via dnf"
    $SUDO dnf install -y cmake ninja-build dfu-util ccache libusbx python3 git
  else
    warn "Unknown package manager — install manually: cmake ninja dfu-util ccache libusb python git"
  fi
}

missing_sysdeps=0
for t in cmake ninja; do have "$t" || missing_sysdeps=1; done
if [ "$missing_sysdeps" -eq 1 ]; then
  install_system_deps
else
  note "System build deps already present (cmake, ninja)"
fi

# ── Cargo tools (no sudo) ───────────────────────────────────────────────────
# espup: installs the esp toolchain. ldproxy: the linker the build invokes.
# espflash: flashes + monitors. cargo-make: the `cargo make` task runner.
# (For the Docker build path you only need cargo-make + espflash + docker.)
for tool in espup ldproxy espflash cargo-make; do
  if have "$tool"; then
    note "$tool already installed"
  else
    note "cargo install $tool"
    cargo install "$tool"
  fi
done

# ── Espressif Xtensa Rust toolchain ─────────────────────────────────────────
EXPORT_FILE="$HOME/export-esp.sh"
note "Running espup install (downloads the Xtensa rustc/LLVM fork — several GB)"
espup install

[ -f "$EXPORT_FILE" ] || die "espup finished but $EXPORT_FILE is missing."

cat <<EOF

$(note "Bootstrap complete.")
Next steps (the export must be sourced in every shell that builds firmware):

  . "$EXPORT_FILE"
  export WIFI_SSID=... WIFI_PASSWORD=... MQTT_URL=mqtt://user:pass@host:1883
  cd "$(dirname "$0")" && cargo build      # or: cargo run  (flash + monitor)

WIFI_SSID/WIFI_PASSWORD/MQTT_URL are baked in at build time (env!), so they must
be exported before the build. They are never committed.
EOF
