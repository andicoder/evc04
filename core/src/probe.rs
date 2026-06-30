//! CN28 LOG read/explore path (evc04#66): poke the LOG console and read it back.
//!
//! Decode the line-oriented LOG telemetry stream, decode an MQTT probe command
//! into the raw bytes written to CN28, and parse the runtime-baud payload for the
//! live baud sweep. Read-only — no control, no safety criticality.

pub mod baud;
pub mod cn28;
pub mod command;
