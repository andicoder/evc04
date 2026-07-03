//! Decode the EVC04 CN28 "LOG" console — a line-oriented ASCII telemetry stream
//! the box emits *in response to a probe* (it sends nothing unprompted, evc04#66).
//! The response is captured in a bounded window, so a window can begin or end
//! mid-line. Decoding is therefore per *complete* line and tolerant: a partial or
//! unrecognised line yields `None` rather than an error. Callers split the
//! captured buffer on `\n` and feed whole lines here.
//!
//! Field units mirror the box's raw integers (verified by correlation while
//! charging at 16 A, evc04#66): phase `V` is millivolts, `A` milliamps.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// A meter-type / channel identifier the box names while probing for a meter
/// (`P1 detect start`, `KLEFR NOT DETECTED!`, …). Only the tokens seen so far are
/// modelled; an unknown token makes the line unrecognised (`None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    P1,
    P2,
    P3,
    Po,
    Klefr,
}

/// IEC-61851 control-pilot state from the `S:<state>` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpState {
    /// `A` — no vehicle connected.
    NoVehicle,
    /// `B` — vehicle connected, not charging.
    Connected,
    /// `C` — charging.
    Charging,
    /// `F` — fault / no meter.
    Fault,
}

impl CpState {
    fn from_letter(c: char) -> Option<Self> {
        Some(match c {
            'A' => CpState::NoVehicle,
            'B' => CpState::Connected,
            'C' => CpState::Charging,
            'F' => CpState::Fault,
            _ => return None,
        })
    }

    /// The single-letter code, matching evcc's charger-status convention.
    pub fn letter(self) -> char {
        match self {
            CpState::NoVehicle => 'A',
            CpState::Connected => 'B',
            CpState::Charging => 'C',
            CpState::Fault => 'F',
        }
    }
}

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
    /// `Any metering device NOT detected!` — the box found no meter at all.
    MeterNotDetected,
    /// `<PROBE> NOT DETECTED!` — a specific meter type was not found.
    ProbeNotDetected(Probe),
    /// `<probe> detect start` — the box began probing for that meter type.
    DetectStart(Probe),
    /// `<probe>_init` — the box is initialising that meter type.
    ProbeInit(Probe),
    /// `<PROBE>: {n}` — a value the box prints for a probe during detection.
    ProbeValue(Probe, u32),
    /// `ERROR: {n}` — an error code from the box.
    Error(u16),
    /// `CLEAR: {n}` — the box cleared a previously-raised error code.
    Clear(u16),
    /// `<PROBE> DETECTED` — a specific meter type *was* found (positive verdict).
    ProbeDetected(Probe),
    /// `Powercut Detected` — the box logged a mains power interruption.
    PowerCut,
    /// `S:<state><n> … Cmax:<a> …` — control-pilot state line: the CP state plus
    /// the current currently offered to the EV.
    CpStatus { state: CpState, cmax_a: u16 },
    /// `Stop Pwm<n>` — the CP PWM was stopped (a running charge is cut).
    PwmStop,
}

/// Classify and decode a single complete LOG line. Returns `None` for a blank,
/// truncated, or unrecognised line.
pub fn parse_line(line: &str) -> Option<LogRecord> {
    parse_phase(line)
        .or_else(|| parse_temp(line))
        .or_else(|| parse_no_data(line))
        .or_else(|| parse_meter_detection(line))
        .or_else(|| parse_error(line))
        .or_else(|| parse_clear(line))
        .or_else(|| parse_cp_status(line))
        .or_else(|| parse_pwm_stop(line))
        .or_else(|| prefixed_u16(line, "ev current:").map(LogRecord::EvCurrent))
        .or_else(|| prefixed_u16(line, "max_offered_current:").map(LogRecord::MaxOffered))
        .or_else(|| prefixed_u16(line, "lb current:").map(LogRecord::LbCurrent))
        .or_else(|| parse_probe_value(line))
}

/// Map a probe/meter-type token to a [`Probe`]; unknown tokens yield `None` so
/// their line stays unrecognised rather than mis-classified.
fn parse_probe(token: &str) -> Option<Probe> {
    Some(match token {
        "P1" => Probe::P1,
        "P2" => Probe::P2,
        "P3" => Probe::P3,
        "PO" => Probe::Po,
        "KLEFR" => Probe::Klefr,
        _ => return None,
    })
}

/// Decode the meter-detection lines: the global `Any metering device NOT
/// detected!`, plus the per-probe `… NOT DETECTED!` / `… detect start` / `…_init`.
/// Case differs between the global (`detected`) and per-probe (`DETECTED`) forms,
/// matching the box's output exactly.
fn parse_meter_detection(line: &str) -> Option<LogRecord> {
    if line == "Any metering device NOT detected!" {
        return Some(LogRecord::MeterNotDetected);
    }
    if line == "Powercut Detected" {
        return Some(LogRecord::PowerCut);
    }
    // `… NOT DETECTED!` (all-caps, trailing `!`) is the negative; check it before
    // the positive `… DETECTED` so the latter can't swallow it.
    if let Some(probe) = line.strip_suffix(" NOT DETECTED!") {
        return Some(LogRecord::ProbeNotDetected(parse_probe(probe)?));
    }
    if let Some(probe) = line.strip_suffix(" DETECTED") {
        return Some(LogRecord::ProbeDetected(parse_probe(probe)?));
    }
    if let Some(probe) = line.strip_suffix(" detect start") {
        return Some(LogRecord::DetectStart(parse_probe(probe)?));
    }
    if let Some(probe) = line.strip_suffix("_init") {
        return Some(LogRecord::ProbeInit(parse_probe(probe)?));
    }
    None
}

/// Decode `<PROBE>: {n}` (e.g. `KLEFR: 0`, `PO: 1`). Phase lines are tab-delimited
/// and decoded earlier, so they never reach here.
fn parse_probe_value(line: &str) -> Option<LogRecord> {
    let (probe, value) = line.split_once(':')?;
    let probe = parse_probe(probe)?;
    let value: u32 = value.trim_start().parse().ok()?;
    Some(LogRecord::ProbeValue(probe, value))
}

/// Decode `ERROR: {n}`.
fn parse_error(line: &str) -> Option<LogRecord> {
    let code: u16 = line.strip_prefix("ERROR:")?.trim_start().parse().ok()?;
    Some(LogRecord::Error(code))
}

/// Decode `CLEAR: {n}` — the box clearing a previously-raised error code.
fn parse_clear(line: &str) -> Option<LogRecord> {
    let code: u16 = line.strip_prefix("CLEAR:")?.trim_start().parse().ok()?;
    Some(LogRecord::Clear(code))
}

/// Decode `S:<state><n> Auth:<n> D:<n> Cmax:<a> Ph:<n> Relay:<n>` — the control-pilot
/// state line. Needs a known state letter and a `Cmax:` field; the other fields are
/// diagnostic and ignored.
fn parse_cp_status(line: &str) -> Option<LogRecord> {
    let rest = line.strip_prefix("S:")?;
    let state = CpState::from_letter(rest.split_whitespace().next()?.chars().next()?)?;
    let cmax_a = rest
        .split_whitespace()
        .find_map(|t| t.strip_prefix("Cmax:"))?
        .parse()
        .ok()?;
    Some(LogRecord::CpStatus { state, cmax_a })
}

/// Decode `Stop Pwm{n}` — a PWM stop (charge cut). The index is not retained.
fn parse_pwm_stop(line: &str) -> Option<LogRecord> {
    line.strip_prefix("Stop Pwm")?.parse::<u8>().ok()?;
    Some(LogRecord::PwmStop)
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
    let phase: u8 = fields
        .next()?
        .strip_prefix('P')?
        .strip_suffix(':')?
        .parse()
        .ok()?;
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
    /// The box's last explicit global meter verdict: `Some(false)` once it prints
    /// `Any metering device NOT detected!`. Only the negative is currently
    /// recognised (no known positive token), so it never flips back to `true`.
    pub meter_detected: Option<bool>,
    /// The most recent `ERROR: {n}` code, or `None` if none has been seen.
    pub last_error: Option<u16>,
    /// Control-pilot state from the last `S:` line — the live plug/charge state.
    pub cp_state: Option<CpState>,
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
            LogRecord::MeterNotDetected => self.meter_detected = Some(false),
            LogRecord::ProbeDetected(_) => self.meter_detected = Some(true),
            LogRecord::Error(code) => self.last_error = Some(code),
            LogRecord::Clear(_) => self.last_error = None,
            LogRecord::CpStatus { state, .. } => self.cp_state = Some(state),
            // A cut ends the session, and the box prints no `LB current` while
            // idle — without this the last grant would stay latched and mislead
            // the V4 start-grant (#135).
            LogRecord::PowerCut | LogRecord::PwmStop => self.lb_current_a = Some(0),
            // Recognised events that carry no snapshot state.
            LogRecord::ProbeNotDetected(_)
            | LogRecord::DetectStart(_)
            | LogRecord::ProbeInit(_)
            | LogRecord::ProbeValue(_, _) => {}
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
        let cp_state = match self.cp_state {
            Some(s) => format!("\"{}\"", s.letter()),
            None => String::from("null"),
        };
        format!(
            "{{\"p1\":{},\"p2\":{},\"p3\":{},\"temp_c\":{},\
             \"ev_current_a\":{},\"max_offered_a\":{},\"lb_current_a\":{},\
             \"meter_detected\":{},\"last_error\":{},\"cp_state\":{}}}",
            phase_json(&self.phases[0]),
            phase_json(&self.phases[1]),
            phase_json(&self.phases[2]),
            opt_json(self.temp_c),
            opt_json(self.ev_current_a),
            opt_json(self.max_offered_a),
            opt_json(self.lb_current_a),
            opt_json(self.meter_detected),
            opt_json(self.last_error),
            cp_state,
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

/// Reassemble complete LOG lines from a byte stream that arrives in arbitrary
/// chunks (each probe window is one chunk, and a window can split a line — even a
/// token — across its boundary). [`push`](LineReassembler::push) returns the lines
/// that completed in this chunk and buffers the trailing partial line for the next.
/// An unterminated line longer than [`MAX_LINE`] is dropped, so a never-ending
/// stream of garbage cannot grow the buffer without bound on the ESP.
#[derive(Debug, Clone, Default)]
pub struct LineReassembler {
    buf: Vec<u8>,
    /// True after an over-length overflow: skip bytes until the next newline
    /// resyncs, so the discarded head is not emitted as a spurious line.
    discarding: bool,
}

/// Hard cap on a single buffered line; a longer unterminated run is discarded.
const MAX_LINE: usize = 512;

impl LineReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; return every line that terminated within it (CR/LF stripped),
    /// in order. The trailing partial line, if any, is kept for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if self.discarding {
                    // The newline ends an over-length discard; resync cleanly.
                    self.discarding = false;
                } else {
                    let mut line = &self.buf[..];
                    if line.last() == Some(&b'\r') {
                        line = &line[..line.len() - 1];
                    }
                    lines.push(String::from_utf8_lossy(line).into_owned());
                }
                self.buf.clear();
            } else if !self.discarding {
                self.buf.push(byte);
                if self.buf.len() > MAX_LINE {
                    self.buf.clear();
                    self.discarding = true;
                }
            }
        }
        lines
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
            r#"{"p1":null,"p2":null,"p3":null,"temp_c":null,"ev_current_a":null,"max_offered_a":null,"lb_current_a":null,"meter_detected":null,"last_error":null,"cp_state":null}"#
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
    fn a_pwm_stop_zeroes_the_grant() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("lb current:10");
        snap.apply_line("Stop Pwm1");
        assert_eq!(snap.lb_current_a, Some(0));
    }

    #[test]
    fn a_power_cut_zeroes_the_grant() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("lb current:10");
        snap.apply_line("Powercut Detected");
        assert_eq!(snap.lb_current_a, Some(0));
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
            r#"{"p1":{"v_mv":234841,"a_ma":16150,"w":3761,"wh":2661},"p2":null,"p3":null,"temp_c":52,"ev_current_a":16,"max_offered_a":16,"lb_current_a":16,"meter_detected":null,"last_error":null,"cp_state":null}"#
        );
    }

    #[test]
    fn parses_the_global_meter_not_detected_line() {
        assert_eq!(
            parse_line("Any metering device NOT detected!"),
            Some(LogRecord::MeterNotDetected)
        );
    }

    #[test]
    fn parses_a_probe_not_detected_line() {
        assert_eq!(
            parse_line("KLEFR NOT DETECTED!"),
            Some(LogRecord::ProbeNotDetected(Probe::Klefr))
        );
    }

    #[test]
    fn parses_a_reassembled_split_not_detected_line() {
        assert_eq!(
            parse_line("P1 NOT DETECTED!"),
            Some(LogRecord::ProbeNotDetected(Probe::P1))
        );
    }

    #[test]
    fn parses_a_detect_start_line() {
        assert_eq!(
            parse_line("PO detect start"),
            Some(LogRecord::DetectStart(Probe::Po))
        );
    }

    #[test]
    fn parses_a_probe_init_line() {
        assert_eq!(parse_line("P1_init"), Some(LogRecord::ProbeInit(Probe::P1)));
    }

    #[test]
    fn parses_a_probe_value_line() {
        assert_eq!(
            parse_line("KLEFR: 0"),
            Some(LogRecord::ProbeValue(Probe::Klefr, 0))
        );
        assert_eq!(
            parse_line("PO: 1"),
            Some(LogRecord::ProbeValue(Probe::Po, 1))
        );
    }

    #[test]
    fn parses_an_error_code_line() {
        assert_eq!(parse_line("ERROR: 22"), Some(LogRecord::Error(22)));
    }

    #[test]
    fn parses_a_positive_probe_detected_line() {
        assert_eq!(
            parse_line("KLEFR DETECTED"),
            Some(LogRecord::ProbeDetected(Probe::Klefr))
        );
    }

    #[test]
    fn positive_detected_does_not_swallow_the_negative() {
        // The all-caps " NOT DETECTED!" negative must still win over " DETECTED".
        assert_eq!(
            parse_line("KLEFR NOT DETECTED!"),
            Some(LogRecord::ProbeNotDetected(Probe::Klefr))
        );
    }

    #[test]
    fn parses_a_powercut_line() {
        assert_eq!(parse_line("Powercut Detected"), Some(LogRecord::PowerCut));
    }

    #[test]
    fn parses_a_clear_code_line() {
        assert_eq!(parse_line("CLEAR: 22"), Some(LogRecord::Clear(22)));
    }

    #[test]
    fn a_probe_detected_sets_the_snapshot_flag_true() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("KLEFR DETECTED"));
        assert_eq!(snap.meter_detected, Some(true));
    }

    #[test]
    fn a_clear_line_clears_last_error() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("ERROR: 22"));
        assert_eq!(snap.last_error, Some(22));
        assert!(snap.apply_line("CLEAR: 22"));
        assert_eq!(snap.last_error, None);
    }

    #[test]
    fn a_powercut_is_recognised_without_changing_state() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("Powercut Detected"));
        assert_eq!(snap.meter_detected, None);
        assert_eq!(snap.last_error, None);
    }

    #[test]
    fn parses_a_charging_cp_state_line() {
        assert_eq!(
            parse_line("S:C2 Auth:1 D:281 Cmax:16 Ph:3 Relay:7"),
            Some(LogRecord::CpStatus {
                state: CpState::Charging,
                cmax_a: 16,
            })
        );
    }

    #[test]
    fn maps_all_known_cp_state_letters() {
        let s = |line| match parse_line(line) {
            Some(LogRecord::CpStatus { state, .. }) => Some(state),
            _ => None,
        };
        assert_eq!(
            s("S:A1 Auth:1 D:0 Cmax:0 Ph:3 Relay:7"),
            Some(CpState::NoVehicle)
        );
        assert_eq!(
            s("S:B1 Auth:1 D:211 Cmax:0 Ph:3 Relay:7"),
            Some(CpState::Connected)
        );
        assert_eq!(
            s("S:F1 Auth:1 D:0 Cmax:0 Ph:3 Relay:7"),
            Some(CpState::Fault)
        );
    }

    #[test]
    fn an_unknown_cp_state_letter_is_not_recognised() {
        assert_eq!(parse_line("S:Z9 Auth:1 D:0 Cmax:0 Ph:3 Relay:7"), None);
    }

    #[test]
    fn parses_a_pwm_stop_line() {
        assert_eq!(parse_line("Stop Pwm1"), Some(LogRecord::PwmStop));
    }

    #[test]
    fn a_cp_state_line_sets_the_snapshot_and_serializes_the_letter() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("S:C2 Auth:1 D:281 Cmax:16 Ph:3 Relay:7"));
        assert_eq!(snap.cp_state, Some(CpState::Charging));
        let json = snap.to_json();
        assert!(json.contains(r#""cp_state":"C""#), "{json}");
        // A later connected-idle line flips it to B.
        assert!(snap.apply_line("S:B1 Auth:1 D:0 Cmax:0 Ph:3 Relay:7"));
        assert_eq!(snap.cp_state, Some(CpState::Connected));
    }

    #[test]
    fn a_phase_line_still_wins_over_probe_value() {
        assert_eq!(
            parse_line("P1:\tV: 1\tA: 2\tW: 3\tWh: 4"),
            Some(LogRecord::Phase {
                phase: 1,
                v_mv: 1,
                a_ma: 2,
                w: 3,
                wh: 4,
            })
        );
    }

    #[test]
    fn an_unknown_probe_token_is_not_recognised() {
        assert_eq!(parse_line("XYZ detect start"), None);
    }

    #[test]
    fn a_global_meter_not_detected_sets_the_snapshot_flag_false() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("Any metering device NOT detected!"));
        assert_eq!(snap.meter_detected, Some(false));
    }

    #[test]
    fn an_error_line_sets_last_error() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("ERROR: 22"));
        assert_eq!(snap.last_error, Some(22));
    }

    #[test]
    fn detect_process_lines_are_recognised_but_do_not_change_state() {
        let mut snap = Cn28Snapshot::new();
        assert!(snap.apply_line("PO detect start"));
        assert!(snap.apply_line("P1_init"));
        assert!(snap.apply_line("KLEFR NOT DETECTED!"));
        assert_eq!(snap.meter_detected, None);
        assert_eq!(snap.last_error, None);
    }

    #[test]
    fn serializes_meter_detection_and_error_fields() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("Any metering device NOT detected!");
        snap.apply_line("ERROR: 22");
        let json = snap.to_json();
        assert!(json.contains(r#""meter_detected":false"#), "{json}");
        assert!(json.contains(r#""last_error":22"#), "{json}");
    }

    #[test]
    fn reassembler_emits_a_complete_line() {
        let mut r = LineReassembler::new();
        assert_eq!(r.push(b"Temp: 33 C\n"), vec!["Temp: 33 C".to_string()]);
    }

    #[test]
    fn reassembler_buffers_a_token_split_across_chunks() {
        let mut r = LineReassembler::new();
        assert!(r.push(b"P1 NOT DETECTE").is_empty());
        assert_eq!(r.push(b"D!\n"), vec!["P1 NOT DETECTED!".to_string()]);
    }

    #[test]
    fn reassembler_emits_each_line_and_keeps_the_partial_tail() {
        let mut r = LineReassembler::new();
        assert_eq!(r.push(b"a\nb\nc"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.push(b"\n"), vec!["c".to_string()]);
    }

    #[test]
    fn reassembler_strips_a_trailing_carriage_return() {
        let mut r = LineReassembler::new();
        assert_eq!(r.push(b"x\r\n"), vec!["x".to_string()]);
    }

    #[test]
    fn reassembler_discards_an_overlong_unterminated_line() {
        let mut r = LineReassembler::new();
        let huge = alloc::vec![b'a'; MAX_LINE + 100];
        assert!(r.push(&huge).is_empty());
        // The overflowed head is dropped; the next newline resyncs and the
        // following line comes through clean.
        assert_eq!(r.push(b"junk-tail\nok\n"), vec!["ok".to_string()]);
    }
}
