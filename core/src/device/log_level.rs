//! Parse the `{"level": "..."}` payload of the runtime log-level topic (evc04#3).
//!
//! The box is sealed: the only way in is MQTT, so the verbosity has to be
//! switchable without a flash. Mirrors `docs/mqtt.md`; a malformed payload is
//! rejected and the firmware keeps its current level.

use crate::charge::intake::IntakeError;

/// The verbosity levels the device accepts. Deliberately only two: `info` is the
/// production level, `debug` adds the per-window CN28 chatter for an
/// investigation. Anything finer would out-run the exporter's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Debug,
}

/// Parse `{"level": "debug"}` / `{"level": "info"}`. The value is quoted in the
/// payload, so the quotes are stripped here — unlike the numeric intakes, this
/// field is genuinely a string.
pub fn parse_log_level(payload: &str) -> Result<LogLevel, IntakeError> {
    let raw =
        crate::charge::intake::field_value(payload, "level").ok_or(IntakeError::MissingField)?;
    let unquoted = raw
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or(IntakeError::BadType)?;
    match unquoted {
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        _ => Err(IntakeError::BadType),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_debug() {
        assert_eq!(parse_log_level(r#"{"level":"debug"}"#), Ok(LogLevel::Debug));
    }

    #[test]
    fn parses_info_with_whitespace() {
        assert_eq!(
            parse_log_level(r#"{ "level": "info" }"#),
            Ok(LogLevel::Info)
        );
    }

    #[test]
    fn rejects_an_unknown_level() {
        // `trace` would out-run the exporter queue, so it is not silently accepted.
        assert_eq!(
            parse_log_level(r#"{"level":"trace"}"#),
            Err(IntakeError::BadType)
        );
    }

    #[test]
    fn rejects_an_unquoted_value() {
        assert_eq!(
            parse_log_level(r#"{"level":debug}"#),
            Err(IntakeError::BadType)
        );
    }

    #[test]
    fn rejects_a_missing_field() {
        assert_eq!(
            parse_log_level(r#"{"enable":true}"#),
            Err(IntakeError::MissingField)
        );
    }
}
