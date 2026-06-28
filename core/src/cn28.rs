//! Decode the EVC04 CN28 "LOG" console — a free-running, line-oriented ASCII
//! telemetry stream (evc04#66). The box emits it continuously; any probe byte
//! only opens a capture window, so a window can begin or end mid-line. Decoding
//! is therefore per *complete* line and tolerant: a partial or unrecognised line
//! yields `None` rather than an error. Callers split the captured buffer on `\n`
//! and feed whole lines here.
//!
//! Field units mirror the box's raw integers (verified by correlation while
//! charging at 16 A, evc04#66): phase `V` is millivolts, `A` milliamps.

use alloc::format;
use alloc::string::String;

/// One decoded CN28 LOG line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRecord {
    /// A `P{n}:` per-phase metering line: `v_mv` millivolts, `a_ma` milliamps,
    /// `w` watts, `wh` watt-hours.
    Phase {
        phase: u8,
        v_mv: u32,
        a_ma: u32,
        w: u32,
        wh: u32,
    },
    /// A `Temp: {n} C` line — internal temperature in whole degrees Celsius.
    Temp(i32),
    /// An `ev current: {n}` line — the EV-requested current in whole amps.
    EvCurrent(u16),
    /// A `max_offered_current: {n}` line — current ceiling offered to the EV (A).
    MaxOffered(u16),
    /// An `lb current:{n}` line — the load-balancing current limit (A).
    LbCurrent(u16),
    /// A `No data received from P{n}!` fault line, carrying the phase number.
    NoData(u8),
}

/// Classify and decode a single complete LOG line. Returns `None` for a blank,
/// truncated, or unrecognised line.
pub fn parse_line(line: &str) -> Option<LogRecord> {
    parse_phase(line)
        .or_else(|| parse_temp(line))
        .or_else(|| parse_no_data(line))
        .or_else(|| prefixed_u16(line, "ev current:").map(LogRecord::EvCurrent))
        .or_else(|| prefixed_u16(line, "max_offered_current:").map(LogRecord::MaxOffered))
        .or_else(|| prefixed_u16(line, "lb current:").map(LogRecord::LbCurrent))
}

/// Decode `Temp: {n} C` — the value is the first whitespace token after the
/// label (the box trails it with ` C`), parsed as a signed degree count.
fn parse_temp(line: &str) -> Option<LogRecord> {
    let value = line.strip_prefix("Temp:")?.trim_start();
    let degrees: i32 = value.split_whitespace().next()?.parse().ok()?;
    Some(LogRecord::Temp(degrees))
}

/// Decode `No data received from P{n}!` into its phase number.
fn parse_no_data(line: &str) -> Option<LogRecord> {
    let phase: u8 = line
        .strip_prefix("No data received from P")?
        .strip_suffix('!')?
        .parse()
        .ok()?;
    Some(LogRecord::NoData(phase))
}

/// Parse a `"{prefix}{ws?}{u16}"` control line, tolerating the missing space the
/// box prints after `lb current:`. A trailing non-digit (e.g. `16C`) makes it fail.
fn prefixed_u16(line: &str, prefix: &str) -> Option<u16> {
    line.strip_prefix(prefix)?.trim_start().parse().ok()
}

/// Decode a `P{n}:\tV: …\tA: …\tW: …\tWh: …` metering line. A missing or extra
/// field, or any non-numeric value, makes it `None` (a truncated line is junk).
fn parse_phase(line: &str) -> Option<LogRecord> {
    let mut fields = line.split('\t');
    let phase: u8 = fields.next()?.strip_prefix('P')?.strip_suffix(':')?.parse().ok()?;
    let v_mv = labelled(fields.next()?, "V")?;
    let a_ma = labelled(fields.next()?, "A")?;
    let w = labelled(fields.next()?, "W")?;
    let wh = labelled(fields.next()?, "Wh")?;
    if fields.next().is_some() {
        return None;
    }
    Some(LogRecord::Phase {
        phase,
        v_mv,
        a_ma,
        w,
        wh,
    })
}

/// Parse a `"<label>: <u32>"` field, rejecting a wrong label or bad number.
fn labelled(field: &str, label: &str) -> Option<u32> {
    field
        .strip_prefix(label)?
        .strip_prefix(':')?
        .trim_start()
        .parse()
        .ok()
}

/// One phase's latest metering values. `v_mv` millivolts, `a_ma` milliamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhaseReading {
    pub v_mv: u32,
    pub a_ma: u32,
    pub w: u32,
    pub wh: u32,
}

/// The latest-seen value of each CN28 LOG field, accumulated across probe
/// windows so a truncated window's gaps stay filled from earlier ones. A field
/// that has never appeared is `None` and serialises as `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cn28Snapshot {
    pub phases: [Option<PhaseReading>; 3],
    pub temp_c: Option<i32>,
    pub ev_current_a: Option<u16>,
    pub max_offered_a: Option<u16>,
    pub lb_current_a: Option<u16>,
}

impl Cn28Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a decoded record into the snapshot, overwriting that field's value.
    /// A `NoData` fault clears the named phase — its last reading is stale.
    pub fn apply(&mut self, record: LogRecord) {
        match record {
            LogRecord::Phase {
                phase,
                v_mv,
                a_ma,
                w,
                wh,
            } => {
                if let Some(slot) = self.phase_slot(phase) {
                    *slot = Some(PhaseReading { v_mv, a_ma, w, wh });
                }
            }
            LogRecord::Temp(c) => self.temp_c = Some(c),
            LogRecord::EvCurrent(a) => self.ev_current_a = Some(a),
            LogRecord::MaxOffered(a) => self.max_offered_a = Some(a),
            LogRecord::LbCurrent(a) => self.lb_current_a = Some(a),
            LogRecord::NoData(phase) => {
                if let Some(slot) = self.phase_slot(phase) {
                    *slot = None;
                }
            }
        }
    }

    /// Decode one LOG line and apply it. Returns `true` if the line was a
    /// recognised record (and thus updated the snapshot), `false` for junk.
    pub fn apply_line(&mut self, line: &str) -> bool {
        match parse_line(line) {
            Some(record) => {
                self.apply(record);
                true
            }
            None => false,
        }
    }

    /// Render the snapshot as one flat JSON object (fixed field order so Home
    /// Assistant value templates stay stable); absent fields are `null`.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"p1\":{},\"p2\":{},\"p3\":{},\"temp_c\":{},\
             \"ev_current_a\":{},\"max_offered_a\":{},\"lb_current_a\":{}}}",
            phase_json(&self.phases[0]),
            phase_json(&self.phases[1]),
            phase_json(&self.phases[2]),
            opt_json(self.temp_c),
            opt_json(self.ev_current_a),
            opt_json(self.max_offered_a),
            opt_json(self.lb_current_a),
        )
    }

    /// 1-based phase number → its slot, or `None` for an out-of-range phase.
    fn phase_slot(&mut self, phase: u8) -> Option<&mut Option<PhaseReading>> {
        self.phases.get_mut(phase.checked_sub(1)? as usize)
    }
}

/// A phase reading as a nested JSON object, or `null` when never seen.
fn phase_json(reading: &Option<PhaseReading>) -> String {
    match reading {
        Some(r) => format!(
            "{{\"v_mv\":{},\"a_ma\":{},\"w\":{},\"wh\":{}}}",
            r.v_mv, r.a_ma, r.w, r.wh
        ),
        None => String::from("null"),
    }
}

/// A scalar field as its JSON number, or `null` when absent.
fn opt_json<T: core::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(v) => format!("{v}"),
        None => String::from("null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_phase_metering_line() {
        assert_eq!(
            parse_line("P1:\tV: 234841\tA: 16150\tW: 3761\tWh: 2661"),
            Some(LogRecord::Phase {
                phase: 1,
                v_mv: 234841,
                a_ma: 16150,
                w: 3761,
                wh: 2661,
            })
        );
    }

    #[test]
    fn parses_a_temperature_line() {
        assert_eq!(parse_line("Temp: 52 C "), Some(LogRecord::Temp(52)));
    }

    #[test]
    fn parses_a_negative_temperature_line() {
        assert_eq!(parse_line("Temp: -5 C "), Some(LogRecord::Temp(-5)));
    }

    #[test]
    fn parses_an_ev_current_line() {
        assert_eq!(parse_line("ev current: 16"), Some(LogRecord::EvCurrent(16)));
    }

    #[test]
    fn parses_a_max_offered_current_line() {
        assert_eq!(
            parse_line("max_offered_current: 16"),
            Some(LogRecord::MaxOffered(16))
        );
    }

    #[test]
    fn parses_an_lb_current_line_without_a_space() {
        assert_eq!(parse_line("lb current:16"), Some(LogRecord::LbCurrent(16)));
    }

    #[test]
    fn parses_a_no_data_fault_line() {
        assert_eq!(
            parse_line("No data received from P1!"),
            Some(LogRecord::NoData(1))
        );
    }

    #[test]
    fn ignores_a_truncated_phase_line() {
        // A capture window can start mid-line: the "P1:\tV: …\tA: " prefix is gone.
        assert_eq!(parse_line("6136\tW: 3774\tWh: 2661"), None);
    }

    #[test]
    fn ignores_a_garbled_lb_current_line() {
        // Two log writes stitched: "lb current:16" + stray "C" — not a clean number.
        assert_eq!(parse_line("lb current:16C"), None);
    }

    #[test]
    fn ignores_a_blank_line() {
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn empty_snapshot_serializes_every_field_as_null() {
        assert_eq!(
            Cn28Snapshot::new().to_json(),
            r#"{"p1":null,"p2":null,"p3":null,"temp_c":null,"ev_current_a":null,"max_offered_a":null,"lb_current_a":null}"#
        );
    }

    #[test]
    fn apply_line_records_a_phase_reading() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("P2:\tV: 234974\tA: 16046\tW: 3739\tWh: 2624"));
        assert_eq!(
            snap.phases[1],
            Some(PhaseReading {
                v_mv: 234974,
                a_ma: 16046,
                w: 3739,
                wh: 2624,
            })
        );
    }

    #[test]
    fn apply_line_records_temp_and_control_currents() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("Temp: 52 C ");
        snap.apply_line("ev current: 16");
        snap.apply_line("max_offered_current: 14");
        snap.apply_line("lb current:10");
        assert_eq!(snap.temp_c, Some(52));
        assert_eq!(snap.ev_current_a, Some(16));
        assert_eq!(snap.max_offered_a, Some(14));
        assert_eq!(snap.lb_current_a, Some(10));
    }

    #[test]
    fn a_no_data_fault_invalidates_that_phase() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("P1:\tV: 237132\tA: 63\tW: 2\tWh: 0");
        snap.apply_line("No data received from P1!");
        assert_eq!(snap.phases[0], None);
    }

    #[test]
    fn apply_line_leaves_the_snapshot_untouched_on_junk() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("Temp: 33 C ");
        let before = snap.clone();
        assert!(!snap.apply_line("6136\tW: 3774\tWh: 2661"));
        assert_eq!(snap, before);
    }

    #[test]
    fn serializes_a_populated_snapshot() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("P1:\tV: 234841\tA: 16150\tW: 3761\tWh: 2661");
        snap.apply_line("Temp: 52 C ");
        snap.apply_line("ev current: 16");
        snap.apply_line("max_offered_current: 16");
        snap.apply_line("lb current:16");
        assert_eq!(
            snap.to_json(),
            r#"{"p1":{"v_mv":234841,"a_ma":16150,"w":3761,"wh":2661},"p2":null,"p3":null,"temp_c":52,"ev_current_a":16,"max_offered_a":16,"lb_current_a":16}"#
        );
    }
}
