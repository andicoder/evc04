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
  # pkgconf + libudev are needed to *compile* espflash (the serialport crate).
  if have pacman; then
    note "Installing system deps via pacman"
    $SUDO pacman -S --needed --noconfirm cmake ninja dfu-util ccache libusb python git pkgconf
  elif have apt-get; then
    note "Installing system deps via apt"
    $SUDO apt-get update
    $SUDO apt-get install -y cmake ninja-build dfu-util ccache libusb-1.0-0 python3 python3-venv git pkg-config libudev-dev
  elif have dnf; then
    note "Installing system deps via dnf"
    $SUDO dnf install -y cmake ninja-build dfu-util ccache libusbx python3 git pkgconf-pkg-config systemd-devel
  else
    warn "Unknown package manager — install manually: cmake ninja dfu-util ccache libusb python git pkg-config libudev"
  fi
}

missing_sysdeps=0
for t in cmake ninja pkg-config; do have "$t" || missing_sysdeps=1; done
if [ "$missing_sysdeps" -eq 1 ]; then
  install_system_deps
else
  note "System build deps already present (cmake, ninja, pkg-config)"
fi

# ── Serial port access (flash without sudo) ─────────────────────────────────
# espflash drives /dev/ttyUSB*; on most distros that needs membership in the
# serial group — uucp on Arch, dialout on Debian/Fedora. Takes effect on relogin.
add_serial_group() {
  local grp; if have pacman; then grp="uucp"; else grp="dialout"; fi
  getent group "$grp" >/dev/null || { warn "group '$grp' missing; skipping serial setup"; return 0; }
  if id -nG "$USER" | tr ' ' '\n' | grep -qx "$grp"; then
    note "already in '$grp' (serial access ok)"
  else
    note "adding $USER to '$grp' for serial access"
    $SUDO usermod -aG "$grp" "$USER"
    warn "log out/in (or run 'newgrp $grp') before flashing — group change needs a new session"
  fi
}
add_serial_group

# ── Cargo tools (no sudo) ───────────────────────────────────────────────────
# espup: installs the esp toolchain. ldproxy: the linker the build invokes.
# espflash: flashes + monitors. cargo-make: the `cargo make` task runner.
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

# ── libxml2/ICU compat shim for esp-clang ───────────────────────────────────
# The ESP-IDF-bundled esp-clang (installed later, by the first build — NOT here)
# links libxml2.so.2 (+ ICU 75), but rolling distros have moved to a newer soname.
# Stage matching libs into a private dir that build.sh adds to LD_LIBRARY_PATH.
# esp-clang isn't on disk yet at bootstrap time, so gate on whether the *system*
# can already resolve libxml2.so.2 rather than on the (absent) clang binary.
install_compat_libs() {
  local compat="$HOME/.espressif/compat-libs"
  if ldconfig -p 2>/dev/null | grep -q 'libxml2\.so\.2 '; then
    note "libxml2.so.2 present system-wide — no esp-clang compat shim needed"
    return 0
  fi
  if ! have pacman; then
    warn "libxml2.so.2 missing and this isn't Arch — if the later esp-clang build fails, install libxml2.so.2 + ICU 75 manually."
    return 0
  fi
  note "Staging libxml2/ICU compat libs for esp-clang into $compat"
  mkdir -p "$compat"
  local tmp; tmp=$(mktemp -d)
  ( cd "$tmp"
    curl -sSL -O "https://archive.archlinux.org/packages/l/libxml2/libxml2-2.12.7-1-x86_64.pkg.tar.zst"
    curl -sSL -O "https://archive.archlinux.org/packages/i/icu/icu-75.1-2-x86_64.pkg.tar.zst"
    for p in ./*.pkg.tar.zst; do tar --zstd -xf "$p"; done
    cp -av usr/lib/libxml2.so.2* usr/lib/libicu*.so.75* "$compat/" )
  rm -rf "$tmp"
}
install_compat_libs

cat <<EOF

$(note "Bootstrap complete.")
Creds are baked in at build time via env! — put them in ./.env (gitignored),
which flash.sh sources for you; never commit them:

  cd "$(dirname "$0")"
  cat > .env <<'ENV'
  export WIFI_SSID=...
  export WIFI_PASSWORD=...
  export MQTT_URL=mqtt://user:pass@host:1883
  ENV
  ./flash.sh            # release build, flash + monitor on the host (USB)
EOF
