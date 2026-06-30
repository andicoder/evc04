//! Meter-emulation control plane (SPECS §4–§9): the safety-relevant current path.
//!
//! The closed-loop control math, the MQTT control-input parsing, the retained
//! charge status, and the Modbus PRO380 framing the RS485 slave answers with —
//! all ported `no_std` from the `charge` daemon so the on-box firmware serves the
//! same hardware-proven value (evc04#85/#86).

pub mod control;
pub mod frame;
pub mod intake;
pub mod status;
