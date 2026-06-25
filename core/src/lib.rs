//! Pure logic for the CN28 LOG remote prober (evc04#66).
//!
//! `no_std` so the same crate links into the Xtensa firmware; `alloc` for the
//! `Vec<u8>`/`String` the codec produces. Built and tested on the host with the
//! stable toolchain — the firmware depends on it via `path = "../core"`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod baud;
pub mod command;
pub mod control;
pub mod dump;
pub mod frame;
pub mod intake;
pub mod ota;
pub mod status;
