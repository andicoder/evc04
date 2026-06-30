//! Debug-only rendering of raw CN28 response bytes (the firmware gates this behind
//! its `raw-debug` feature): hex and printable-ASCII views for capture/discovery,
//! never part of the production telemetry path.

pub mod dump;
