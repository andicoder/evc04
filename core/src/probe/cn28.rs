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
    /// A per-phase payload whose `P<n>:` label was destroyed by a splice (#159).
    /// Carries no phase number on purpose — the number is recovered from the burst
    /// frame in [`Cn28Snapshot::apply`], or the reading is dropped. Never guess it
    /// from a surviving bare digit: the box prints `lb current:3`, so junk ending in
    /// a digit would silently mislabel a phase.
    PhaseUnlabelled {
        v_mv: u32,
        a_ma: u32,
        w: u32,
        wh: u32,
    },
    /// `S:<state><n> … Cmax:<a> …` — control-pilot state line: the CP state plus
    /// the current currently offered to the EV.
    CpStatus { state: CpState, cmax_a: u16 },
    /// `Stop Pwm<n>` — the CP PWM was stopped (a running charge is cut).
    PwmStop,
    /// `wc` — recurring and frequent, meaning still unknown (#73). Recognised
    /// so it stops inflating the parse-failure counter: 73 of the 111 failures
    /// measured on 2026-09-04 were this two-byte line, intact (`77 63`).
    /// Recognising a line is not the same as understanding it.
    Wc,
    /// `TEMP lb current: {n}` — the load-balancing limit **after thermal
    /// derating**, which the protocol doc records as a different quantity from
    /// `lb current:`. Deliberately not folded into [`LogRecord::LbCurrent`]:
    /// that value is what the box reports over MQTT.
    TempLbCurrent(u16),
    /// `lb wait for time` — load balancing holding for a schedule window.
    LbWaitForTime,
    /// `Nref: {n}` — a reference value the box prints during detection.
    Nref(u32),
    /// `DMA …` — the box's own driver chatter, not a CN28 record at all.
    /// Counting these as CN28 parse failures was simply wrong.
    DmaEvent,
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
        .or_else(|| prefixed_u16(line, "TEMP lb current:").map(LogRecord::TempLbCurrent))
        .or_else(|| prefixed_u32(line, "Nref:").map(LogRecord::Nref))
        .or_else(|| (line == "wc").then_some(LogRecord::Wc))
        .or_else(|| (line == "lb wait for time").then_some(LogRecord::LbWaitForTime))
        .or_else(|| line.starts_with("DMA ").then_some(LogRecord::DmaEvent))
        .or_else(|| parse_probe_value(line))
        // Last: the box splices unrelated output into a line it is already printing
        // (#159), so a whole record can sit behind junk. Scanning is the fallback,
        // never the first choice — a clean line must still take the strict path.
        .or_else(|| scan_phase(line))
        .or_else(|| scan_cp_status(line))
}

/// Recover a control-pilot line a splice damaged (#159/#161). Worth the extra pass
/// because the line is emitted only on transitions, so a single lost one latches the
/// CP state until the next plug/charge event (#158).
///
/// The candidate must still satisfy the strict parse — a known state letter *and* a
/// `Cmax:` field — so an ` Auth:` appearing in unrelated output cannot fabricate a
/// pilot state.
fn scan_cp_status(line: &str) -> Option<LogRecord> {
    // Anchor on ` Auth:`, not on `S:`. Measured 2026-08-16 (#161): across two
    // deliberate offered-current changes the box emitted a pilot line both times and
    // the splice ate the `S:` marker both times (`eC2 Auth:1 …`, `…b cC2 Auth:1 …`),
    // so keying on the marker recovered neither. The state letter and its sub-index
    // sit immediately before the anchor; everything ahead of them is junk.
    line.match_indices(" Auth:").find_map(|(i, _)| {
        let head = line.get(..i)?;
        let sub = head.strip_suffix(|c: char| c.is_ascii_digit())?;
        let letter = sub.chars().next_back()?;
        CpState::from_letter(letter)?;
        // Rebuild the marker and re-run the strict parse, so the `Cmax:` requirement
        // and the field rules live in exactly one place.
        parse_cp_status(&format!(
            "S:{}{}",
            &head[sub.len() - letter.len_utf8()..],
            &line[i..]
        ))
    })
}

/// Recover a per-phase record that a splice pushed off the start of the line (#159).
///
/// Anchors on the payload — `:\tV: <u32>\tA: <u32>\tW: <u32>\tWh: <u32>` — which is
/// distinctive enough that junk cannot imitate it, then reads the label *backwards*
/// from the anchor. A label counts only as the full `P<n>`; a lone surviving digit is
/// rejected, because the box prints lines like `lb current:3` whose trailing digit
/// would otherwise be read as a phase number.
fn scan_phase(line: &str) -> Option<LogRecord> {
    let anchor = line.find(":\tV: ")?;
    let (head, payload) = line.split_at(anchor);
    let mut fields = payload.strip_prefix(':')?.split('\t').skip(1);
    let v_mv = labelled(fields.next()?, "V")?;
    let a_ma = labelled(fields.next()?, "A")?;
    let w = labelled(fields.next()?, "W")?;
    let wh = labelled(fields.next()?, "Wh")?;
    if fields.next().is_some() {
        return None;
    }
    let phase = head
        .strip_suffix(|c: char| c.is_ascii_digit())
        .filter(|rest| rest.ends_with('P'))
        .and_then(|_| head.as_bytes().last())
        .map(|d| d - b'0');
    Some(match phase {
        Some(phase) => LogRecord::Phase {
            phase,
            v_mv,
            a_ma,
            w,
            wh,
        },
        None => LogRecord::PhaseUnlabelled { v_mv, a_ma, w, wh },
    })
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

fn prefixed_u32(line: &str, prefix: &str) -> Option<u32> {
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

/// A fault the box raised and has not explicitly retracted (#3). It survives an
/// unrelated `CLEAR:` on purpose: the box's own error field is edge-triggered, so
/// without stickiness a fault that ended before anyone looked is unreportable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub code: u16,
    /// Milliseconds (caller's clock) when this code was first seen.
    pub first_seen_ms: u64,
    /// How often the box has raised this code since then.
    pub count: u32,
}

/// What one LOG line was. `Blank` exists so the empty lines the box pads its
/// bursts with never count as parse failures — the counter (#3) is only useful
/// if every increment means real wreckage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOutcome {
    Applied,
    Blank,
    Unparsed,
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
    /// The standing fault (#3): raised by `ERROR: {n}`, cleared only by a
    /// `CLEAR:` naming the *same* code.
    pub fault: Option<Fault>,
    /// Control-pilot state from the last `S:` line — the live plug/charge state.
    pub cp_state: Option<CpState>,
    /// A phase payload whose label a splice destroyed, held until the next record
    /// says whether it was the burst head (#159). Never serialised — it is either
    /// promoted to phase 1 or dropped.
    pending_unlabelled: Option<PhaseReading>,
}

impl Cn28Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a decoded record into the snapshot, overwriting that field's value.
    /// A `NoData` fault clears the named phase — its last reading is stale.
    /// `now_ms` stamps a newly-opened fault; it is ignored by every other record.
    pub fn apply(&mut self, record: LogRecord, now_ms: u64) {
        // A held unlabelled payload survives exactly one record: the burst prints
        // P1, P2, P3 back to back (protocol doc §1), so it is phase 1 if and only if
        // a labelled P2 follows it immediately. Anything else and it is dropped —
        // this must never invent a phase label.
        let pending = self.pending_unlabelled.take();
        if let (Some(reading), LogRecord::Phase { phase: 2, .. }) = (pending, &record) {
            self.phases[0] = Some(reading);
        }

        match record {
            LogRecord::PhaseUnlabelled { v_mv, a_ma, w, wh } => {
                self.pending_unlabelled = Some(PhaseReading { v_mv, a_ma, w, wh });
            }
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
            LogRecord::Error(code) => self.raise_fault(code, now_ms),
            // Only the code the box names is retracted; a CLEAR for anything else
            // leaves the standing fault alone (#3).
            LogRecord::Clear(code) => {
                if self.fault.is_some_and(|f| f.code == code) {
                    self.fault = None;
                }
            }
            LogRecord::CpStatus { state, .. } => self.cp_state = Some(state),
            // A cut ends the session, and the box prints no `LB current` while
            // idle — without this the last grant would stay latched and mislead
            // the V4 start-grant (#135).
            LogRecord::PowerCut | LogRecord::PwmStop => self.lb_current_a = Some(0),
            // Recognised events that carry no snapshot state.
            LogRecord::ProbeNotDetected(_)
            | LogRecord::DetectStart(_)
            | LogRecord::ProbeInit(_)
            | LogRecord::ProbeValue(_, _)
            // Recognised but not yet interpreted. TempLbCurrent in particular
            // is a real measurement worth keeping one day; folding it into
            // lb_current_a today would change what goes out over MQTT.
            | LogRecord::Wc
            | LogRecord::TempLbCurrent(_)
            | LogRecord::LbWaitForTime
            | LogRecord::Nref(_)
            | LogRecord::DmaEvent => {}
        }
    }

    /// Decode one LOG line and apply it, reporting what the line turned out to be
    /// so the caller can log a parse failure without re-deciding it here.
    pub fn apply_line(&mut self, line: &str, now_ms: u64) -> LineOutcome {
        match parse_line(line) {
            Some(record) => {
                self.apply(record, now_ms);
                LineOutcome::Applied
            }
            None if line.trim().is_empty() => LineOutcome::Blank,
            None => LineOutcome::Unparsed,
        }
    }

    /// Open a fault, or count another sighting of the one already standing. A
    /// different code replaces it: the newest fault is the one worth reporting.
    fn raise_fault(&mut self, code: u16, now_ms: u64) {
        match self.fault {
            Some(ref mut f) if f.code == code => f.count += 1,
            _ => {
                self.fault = Some(Fault {
                    code,
                    first_seen_ms: now_ms,
                    count: 1,
                })
            }
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
             \"meter_detected\":{},\"fault\":{},\"cp_state\":{}}}",
            phase_json(&self.phases[0]),
            phase_json(&self.phases[1]),
            phase_json(&self.phases[2]),
            opt_json(self.temp_c),
            opt_json(self.ev_current_a),
            opt_json(self.max_offered_a),
            opt_json(self.lb_current_a),
            opt_json(self.meter_detected),
            fault_json(&self.fault),
            cp_state,
        )
    }

    /// 1-based phase number → its slot, or `None` for an out-of-range phase.
    fn phase_slot(&mut self, phase: u8) -> Option<&mut Option<PhaseReading>> {
        self.phases.get_mut(phase.checked_sub(1)? as usize)
    }
}

/// The standing fault as a nested JSON object, or `null` while healthy.
fn fault_json(fault: &Option<Fault>) -> String {
    match fault {
        Some(f) => format!(
            "{{\"code\":{},\"first_seen_ms\":{},\"count\":{}}}",
            f.code, f.first_seen_ms, f.count
        ),
        None => String::from("null"),
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
    /// in order, as raw bytes — a line carrying wreckage must survive verbatim
    /// for the forensic hex dump (#3). The trailing partial line, if any, is kept
    /// for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
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
                    lines.push(line.to_vec());
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
            r#"{"p1":null,"p2":null,"p3":null,"temp_c":null,"ev_current_a":null,"max_offered_a":null,"lb_current_a":null,"meter_detected":null,"fault":null,"cp_state":null}"#
        );
    }

    #[test]
    fn apply_line_records_a_phase_reading() {
        let mut snap = Cn28Snapshot::new();
        assert_eq!(
            snap.apply_line("P2:\tV: 234974\tA: 16046\tW: 3739\tWh: 2624", 0),
            LineOutcome::Applied
        );
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
        snap.apply_line("Temp: 52 C ", 0);
        snap.apply_line("ev current: 16", 0);
        snap.apply_line("max_offered_current: 14", 0);
        snap.apply_line("lb current:10", 0);
        assert_eq!(snap.temp_c, Some(52));
        assert_eq!(snap.ev_current_a, Some(16));
        assert_eq!(snap.max_offered_a, Some(14));
        assert_eq!(snap.lb_current_a, Some(10));
    }

    #[test]
    fn a_pwm_stop_zeroes_the_grant() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("lb current:10", 0);
        snap.apply_line("Stop Pwm1", 0);
        assert_eq!(snap.lb_current_a, Some(0));
    }

    #[test]
    fn a_power_cut_zeroes_the_grant() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("lb current:10", 0);
        snap.apply_line("Powercut Detected", 0);
        assert_eq!(snap.lb_current_a, Some(0));
    }

    #[test]
    fn a_no_data_fault_invalidates_that_phase() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("P1:\tV: 237132\tA: 63\tW: 2\tWh: 0", 0);
        snap.apply_line("No data received from P1!", 0);
        assert_eq!(snap.phases[0], None);
    }

    // --- Spliced-line recovery (#159) ---------------------------------------
    // The box writes an unrelated status block on top of whatever it is printing.
    // Measured live over two captures: the splice always lands on the head of the
    // burst, so `P1:`'s label is destroyed in 13 of 13 bursts while `P2:`/`P3:`
    // arrive clean. The V/A/W/Wh payload itself is intact every time. These
    // fixtures are verbatim lines from those captures.

    #[test]
    fn a_labelled_phase_line_is_decoded_through_leading_splice_noise() {
        assert_eq!(
            parse_line("MP lb current: 6P2:\tV: 232934\tA: 18\tW: 0\tWh: 147965"),
            Some(LogRecord::Phase {
                phase: 2,
                v_mv: 232934,
                a_ma: 18,
                w: 0,
                wh: 147965,
            })
        );
    }

    #[test]
    fn a_phase_payload_whose_label_was_eaten_decodes_as_unlabelled() {
        // Verbatim from the 2026-08-16 capture: `\nP1` fully overwritten.
        assert_eq!(
            parse_line("MP lb current: 6b c:\tV: 239202\tA: 45\tW: 3\tWh: 150190"),
            Some(LogRecord::PhaseUnlabelled {
                v_mv: 239202,
                a_ma: 45,
                w: 3,
                wh: 150190,
            })
        );
    }

    #[test]
    fn a_bare_digit_left_by_the_splice_is_not_taken_as_a_phase_label() {
        // The box prints `lb current:3`; if that junk ends up in front of a payload
        // the trailing '3' must not be read as "phase 3" — a mislabelled phase is
        // worse than a dropped reading.
        assert_eq!(
            parse_line("lb current:3:\tV: 239202\tA: 45\tW: 3\tWh: 150190"),
            Some(LogRecord::PhaseUnlabelled {
                v_mv: 239202,
                a_ma: 45,
                w: 3,
                wh: 150190,
            })
        );
    }

    // Both verbatim from a 2026-08-16 capture taken across two deliberate offered-
    // current changes (#161): the box does emit a pilot line when only `Cmax` moves,
    // but the splice ate the `S:` marker itself both times. Anchoring the scan on
    // `S:` therefore recovered neither — the scan has to key on the payload.

    #[test]
    fn a_cp_line_whose_s_marker_was_eaten_is_still_recovered() {
        assert_eq!(
            parse_line("eC2 Auth:1 D:176 Cmax:10 Ph:3 Relay:0"),
            Some(LogRecord::CpStatus {
                state: CpState::Charging,
                cmax_a: 10,
            })
        );
    }

    #[test]
    fn a_cp_line_behind_a_long_splice_is_still_recovered() {
        assert_eq!(
            parse_line(
                "ev rrent_without_dlm_without_unplugged16b cC2 Auth:1 D:281 Cmax:16 Ph:3 Relay:0"
            ),
            Some(LogRecord::CpStatus {
                state: CpState::Charging,
                cmax_a: 16,
            })
        );
    }

    #[test]
    fn an_auth_field_without_a_valid_state_letter_is_not_a_pilot_line() {
        // The anchor alone must not conjure a pilot state out of unrelated output.
        assert_eq!(parse_line("xx9 Auth:1 D:176 Cmax:10 Ph:3 Relay:0"), None);
        assert_eq!(parse_line("Auth:1 D:176 Cmax:10"), None);
    }

    #[test]
    fn a_cp_status_line_is_decoded_through_leading_splice_noise() {
        // The pilot line is emitted only on transitions, so one lost to a splice
        // latches the CP state until the next transition — the #158 freeze.
        assert_eq!(
            parse_line("MP lb current: 6S:C2 Auth:1 D:281 Cmax:16 Ph:3 Relay:7"),
            Some(LogRecord::CpStatus {
                state: CpState::Charging,
                cmax_a: 16,
            })
        );
    }

    #[test]
    fn a_stray_s_colon_without_a_cp_payload_is_not_a_status_line() {
        assert_eq!(parse_line("ERRORS: 12 lb current: 6"), None);
    }

    #[test]
    fn an_unlabelled_payload_followed_by_p2_is_recorded_as_p1() {
        // The documented burst frame is P1, P2, P3 back to back (protocol doc §1),
        // so an unlabelled payload immediately before a labelled P2 is P1. Observed
        // in 13 of 13 bursts.
        let mut snap = Cn28Snapshot::new();
        snap.apply_line(
            "MP lb current: 6b c:\tV: 239202\tA: 45\tW: 3\tWh: 150190",
            0,
        );
        snap.apply_line("P2:\tV: 232934\tA: 18\tW: 0\tWh: 147965", 0);
        assert_eq!(
            snap.phases[0],
            Some(PhaseReading {
                v_mv: 239202,
                a_ma: 45,
                w: 3,
                wh: 150190,
            })
        );
    }

    #[test]
    fn an_unlabelled_payload_on_its_own_is_never_recorded() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line(
            "MP lb current: 6b c:\tV: 239202\tA: 45\tW: 3\tWh: 150190",
            0,
        );
        assert_eq!(snap.phases[0], None);
    }

    #[test]
    fn an_unlabelled_payload_not_followed_by_p2_is_dropped() {
        // Anything but the documented successor means this was not the burst head;
        // guessing would fabricate a phase label.
        let mut snap = Cn28Snapshot::new();
        snap.apply_line(
            "MP lb current: 6b c:\tV: 239202\tA: 45\tW: 3\tWh: 150190",
            0,
        );
        snap.apply_line("P3:\tV: 238729\tA: 18\tW: 0\tWh: 149400", 0);
        assert_eq!(snap.phases[0], None);
        assert!(snap.phases[2].is_some(), "P3 itself must still be recorded");
    }

    #[test]
    fn a_clean_p1_line_still_takes_the_direct_path() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("P1:\tV: 234841\tA: 16150\tW: 3761\tWh: 2661", 0);
        assert_eq!(
            snap.phases[0],
            Some(PhaseReading {
                v_mv: 234841,
                a_ma: 16150,
                w: 3761,
                wh: 2661,
            })
        );
    }

    #[test]
    fn apply_line_leaves_the_snapshot_untouched_on_junk() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("Temp: 33 C ", 0);
        let before = snap.clone();
        assert_eq!(
            snap.apply_line("6136\tW: 3774\tWh: 2661", 0),
            LineOutcome::Unparsed
        );
        assert_eq!(snap, before);
    }

    #[test]
    fn serializes_a_populated_snapshot() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("P1:\tV: 234841\tA: 16150\tW: 3761\tWh: 2661", 0);
        snap.apply_line("Temp: 52 C ", 0);
        snap.apply_line("ev current: 16", 0);
        snap.apply_line("max_offered_current: 16", 0);
        snap.apply_line("lb current:16", 0);
        assert_eq!(
            snap.to_json(),
            r#"{"p1":{"v_mv":234841,"a_ma":16150,"w":3761,"wh":2661},"p2":null,"p3":null,"temp_c":52,"ev_current_a":16,"max_offered_a":16,"lb_current_a":16,"meter_detected":null,"fault":null,"cp_state":null}"#
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
        assert_eq!(snap.apply_line("KLEFR DETECTED", 0), LineOutcome::Applied);
        assert_eq!(snap.meter_detected, Some(true));
    }

    #[test]
    fn a_clear_of_the_same_code_closes_the_fault() {
        let mut snap = Cn28Snapshot::new();
        assert_eq!(snap.apply_line("ERROR: 22", 1_000), LineOutcome::Applied);
        assert_eq!(snap.fault.map(|f| f.code), Some(22));
        assert_eq!(snap.apply_line("CLEAR: 22", 2_000), LineOutcome::Applied);
        assert_eq!(snap.fault, None);
    }

    #[test]
    fn a_clear_of_another_code_leaves_the_fault_standing() {
        // The 2026-09-02 incident (#3): any CLEAR wiped the error field, so the
        // box could not report a fault it was still carrying.
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("ERROR: 22", 1_000);
        snap.apply_line("CLEAR: 7", 2_000);
        assert_eq!(snap.fault.map(|f| f.code), Some(22));
    }

    #[test]
    fn a_repeating_error_keeps_the_first_sighting_and_counts() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("ERROR: 22", 1_000);
        snap.apply_line("ERROR: 22", 9_000);
        assert_eq!(
            snap.fault,
            Some(Fault {
                code: 22,
                first_seen_ms: 1_000,
                count: 2,
            })
        );
    }

    #[test]
    fn another_error_code_replaces_the_fault_and_restarts_the_count() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("ERROR: 22", 1_000);
        snap.apply_line("ERROR: 7", 9_000);
        assert_eq!(
            snap.fault,
            Some(Fault {
                code: 7,
                first_seen_ms: 9_000,
                count: 1,
            })
        );
    }

    #[test]
    fn a_powercut_is_recognised_without_changing_state() {
        let mut snap = Cn28Snapshot::new();
        assert_eq!(
            snap.apply_line("Powercut Detected", 0),
            LineOutcome::Applied
        );
        assert_eq!(snap.meter_detected, None);
        assert_eq!(snap.fault, None);
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
        assert_eq!(
            snap.apply_line("S:C2 Auth:1 D:281 Cmax:16 Ph:3 Relay:7", 0),
            LineOutcome::Applied
        );
        assert_eq!(snap.cp_state, Some(CpState::Charging));
        let json = snap.to_json();
        assert!(json.contains(r#""cp_state":"C""#), "{json}");
        // A later connected-idle line flips it to B.
        assert_eq!(
            snap.apply_line("S:B1 Auth:1 D:0 Cmax:0 Ph:3 Relay:7", 0),
            LineOutcome::Applied
        );
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
        assert_eq!(
            snap.apply_line("Any metering device NOT detected!", 0),
            LineOutcome::Applied
        );
        assert_eq!(snap.meter_detected, Some(false));
    }

    #[test]
    fn an_error_line_opens_a_sticky_fault() {
        let mut snap = Cn28Snapshot::new();
        assert_eq!(snap.apply_line("ERROR: 22", 1_000), LineOutcome::Applied);
        assert_eq!(
            snap.fault,
            Some(Fault {
                code: 22,
                first_seen_ms: 1_000,
                count: 1,
            })
        );
    }

    #[test]
    fn a_blank_line_is_not_a_parse_failure() {
        // The LOG stream carries empty lines between bursts; counting them as
        // failures would bury the signal the parse-failure counter exists for.
        let mut snap = Cn28Snapshot::new();
        assert_eq!(snap.apply_line("", 0), LineOutcome::Blank);
        assert_eq!(snap.apply_line("   ", 0), LineOutcome::Blank);
    }

    #[test]
    fn detect_process_lines_are_recognised_but_do_not_change_state() {
        let mut snap = Cn28Snapshot::new();
        assert_eq!(snap.apply_line("PO detect start", 0), LineOutcome::Applied);
        assert_eq!(snap.apply_line("P1_init", 0), LineOutcome::Applied);
        assert_eq!(
            snap.apply_line("KLEFR NOT DETECTED!", 0),
            LineOutcome::Applied
        );
        assert_eq!(snap.meter_detected, None);
        assert_eq!(snap.fault, None);
    }

    #[test]
    fn serializes_meter_detection_and_error_fields() {
        let mut snap = Cn28Snapshot::new();
        snap.apply_line("Any metering device NOT detected!", 0);
        snap.apply_line("ERROR: 22", 1_000);
        let json = snap.to_json();
        assert!(json.contains(r#""meter_detected":false"#), "{json}");
        assert!(
            json.contains(r#""fault":{"code":22,"first_seen_ms":1000,"count":1}"#),
            "{json}"
        );
    }

    #[test]
    fn reassembler_emits_a_complete_line() {
        let mut r = LineReassembler::new();
        assert_eq!(r.push(b"Temp: 33 C\n"), alloc::vec![b"Temp: 33 C".to_vec()]);
    }

    #[test]
    fn reassembler_buffers_a_token_split_across_chunks() {
        let mut r = LineReassembler::new();
        assert!(r.push(b"P1 NOT DETECTE").is_empty());
        assert_eq!(r.push(b"D!\n"), alloc::vec![b"P1 NOT DETECTED!".to_vec()]);
    }

    #[test]
    fn reassembler_emits_each_line_and_keeps_the_partial_tail() {
        let mut r = LineReassembler::new();
        assert_eq!(
            r.push(b"a\nb\nc"),
            alloc::vec![b"a".to_vec(), b"b".to_vec()]
        );
        assert_eq!(r.push(b"\n"), alloc::vec![b"c".to_vec()]);
    }

    #[test]
    fn reassembler_strips_a_trailing_carriage_return() {
        let mut r = LineReassembler::new();
        assert_eq!(r.push(b"x\r\n"), alloc::vec![b"x".to_vec()]);
    }

    #[test]
    fn reassembler_hands_out_the_raw_bytes_of_a_line() {
        // The forensic case (#3): a line carrying wreckage must reach the log
        // record byte-for-byte, so lossy UTF-8 replacement cannot happen here.
        let mut r = LineReassembler::new();
        assert_eq!(
            r.push(b"Temp: \xff15042 C\n"),
            alloc::vec![b"Temp: \xff15042 C".to_vec()]
        );
    }

    #[test]
    fn reassembler_discards_an_overlong_unterminated_line() {
        let mut r = LineReassembler::new();
        let huge = alloc::vec![b'a'; MAX_LINE + 100];
        assert!(r.push(&huge).is_empty());
        // The overflowed head is dropped; the next newline resyncs and the
        // following line comes through clean.
        assert_eq!(r.push(b"junk-tail\nok\n"), alloc::vec![b"ok".to_vec()]);
    }
}

/// Lines the box really emits that the parser did not know.
///
/// Measured on the live stream 2026-09-04: 111 of 2000 records in two hours
/// (5.5%) were `cn28 line did not parse`, and the hex dumps showed every one of
/// them intact — `54 45 4d 50 20 6c 62 ...` for `TEMP lb current: 0`, `77 63`
/// for `wc`. No truncation, no splice, no missing delimiter. They were simply
/// unimplemented, and each one inflated a counter whose whole purpose is that
/// "every increment means real wreckage".
///
/// All four are already catalogued in docs/cn28-log-protocol.md.
#[cfg(test)]
mod unimplemented_line_tests {
    use super::*;

    #[test]
    fn wc_is_recognised_rather_than_counted_as_wreckage() {
        // 73 of the 111 failures. Meaning still unknown (#73) — recognising a
        // line is not the same as understanding it.
        assert_eq!(parse_line("wc"), Some(LogRecord::Wc));
    }

    #[test]
    fn thermally_derated_lb_current_is_its_own_record() {
        assert_eq!(
            parse_line("TEMP lb current: 16"),
            Some(LogRecord::TempLbCurrent(16))
        );
    }

    #[test]
    fn the_derated_limit_is_not_folded_into_lb_current() {
        // The protocol doc calls it the limit *after thermal derating* — a
        // different quantity. Folding it into lb_current_a would quietly
        // corrupt what the box reports over MQTT.
        let mut snap = Cn28Snapshot::default();
        assert_eq!(snap.apply_line("lb current:16", 0), LineOutcome::Applied);
        assert_eq!(
            snap.apply_line("TEMP lb current: 0", 0),
            LineOutcome::Applied
        );
        assert_eq!(snap.lb_current_a, Some(16), "derated value overwrote it");
    }

    #[test]
    fn load_balancing_wait_is_recognised() {
        assert_eq!(
            parse_line("lb wait for time"),
            Some(LogRecord::LbWaitForTime)
        );
    }

    #[test]
    fn nref_is_recognised() {
        assert_eq!(parse_line("Nref: 448"), Some(LogRecord::Nref(448)));
    }

    #[test]
    fn dma_chatter_is_not_a_cn28_record() {
        // The box's own driver messages. They are not CN28 data at all, so
        // counting them as CN28 parse failures is simply wrong.
        for line in ["DMA Starting", "DMA disabled", "DMA Reset"] {
            assert_eq!(parse_line(line), Some(LogRecord::DmaEvent), "{line}");
        }
    }

    #[test]
    fn genuinely_unknown_lines_are_still_unparsed() {
        // The counter has to keep meaning something. If recognising these four
        // shapes turned the parser permissive, the next real framing break
        // would arrive silently.
        let mut snap = Cn28Snapshot::default();
        assert_eq!(
            snap.apply_line("totally unexpected garbage", 0),
            LineOutcome::Unparsed
        );
    }

    #[test]
    fn the_known_lb_current_line_still_parses() {
        assert_eq!(parse_line("lb current:3"), Some(LogRecord::LbCurrent(3)));
    }
}
