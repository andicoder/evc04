//! Decode the EVC04 CN28 "LOG" console — a free-running, line-oriented ASCII
//! telemetry stream (evc04#66). The box emits it continuously; any probe byte
//! only opens a capture window, so a window can begin or end mid-line. Decoding
//! is therefore per *complete* line and tolerant: a partial or unrecognised line
//! yields `None` rather than an error. Callers split the captured buffer on `\n`
//! and feed whole lines here.
//!
//! Field units mirror the box's raw integers (verified by correlation while
//! charging at 16 A, evc04#66): phase `V` is millivolts, `A` milliamps.

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
}
