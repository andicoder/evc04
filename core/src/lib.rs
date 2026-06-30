//! Pure logic for the CN28 LOG remote prober (evc04#66).
//!
//! `no_std` so the same crate links into the Xtensa firmware; `alloc` for the
//! `Vec<u8>`/`String` the codec produces. Built and tested on the host with the
//! stable toolchain — the firmware depends on it via `path = "../core"`.
//!
//! The modules are grouped by the firmware task they serve:
//!   - [`charge`] — meter-emulation control plane (control math, intake, status, frame).
//!   - [`probe`]  — CN28 LOG read/explore (telemetry decode, command, baud).
//!   - [`debug`]  — raw-byte dumps, feature-gated debug only.
//!   - [`device`] — management plumbing (discovery, version, OTA, WiFi backoff).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod charge;
pub mod debug;
pub mod device;
pub mod probe;
