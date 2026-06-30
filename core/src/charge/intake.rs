//! Parse the MQTT control-intake payloads (evc04#86): `{"ampere": N}` for the
//! target and measured topics, `{"enable": bool}` for the enable gate. Mirrors
//! `charge/docs/mqtt.md` — a malformed or wrong-shaped payload is **rejected** so
//! the firmware holds its last good value and surfaces the reason, never silently
//! pushing the charger to an unintended current.
//!
//! `no_std`: a tiny flat-object field extractor, no serde. The contract's payloads
//! are flat single-purpose objects with additive optional fields, so extracting the
//! one field by name (and ignoring the rest) is all that is needed (#86).

use alloc::format;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakeError {
    /// The expected field is absent (or the payload is not a JSON object).
    MissingField,
    /// The field is present but not the expected type (a number / a bool).
    BadType,
    /// A numeric field that parsed to NaN or infinity — never applied to the box.
    NotFinite,
}

/// Parse a `{"ampere": N}` payload (target / measured topics) into amperes.
/// Out-of-range values are accepted (the control math clamps them, SPECS §6); only
/// a missing field, a non-numeric value, or a non-finite number is rejected.
pub fn parse_ampere(payload: &str) -> Result<f32, IntakeError> {
    let raw = field_value(payload, "ampere").ok_or(IntakeError::MissingField)?;
    let n: f32 = raw.parse().map_err(|_| IntakeError::BadType)?;
    if !n.is_finite() {
        return Err(IntakeError::NotFinite);
    }
    Ok(n)
}

/// Parse a `{"enable": bool}` payload (enable gate, #60).
pub fn parse_enable(payload: &str) -> Result<bool, IntakeError> {
    match field_value(payload, "enable").ok_or(IntakeError::MissingField)? {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(IntakeError::BadType),
    }
}

/// The raw, trimmed value token for `"key"` in a flat JSON object, or `None` if the
/// key is absent. The token runs from after the `:` to the next `,`/`}` — good for
/// the contract's number/bool values; a string value keeps its quotes, so it fails
/// the number/bool parse above (reported as `BadType`).
fn field_value<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let after_key = &payload[payload.find(&needle)? + needle.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let end = after_colon.find([',', '}']).unwrap_or(after_colon.len());
    Some(after_colon[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ampere_object() {
        assert_eq!(parse_ampere(r#"{"ampere": 6.5}"#), Ok(6.5));
    }

    #[test]
    fn parses_integer_ampere() {
        assert_eq!(parse_ampere(r#"{ "ampere":6 }"#), Ok(6.0));
    }

    #[test]
    fn accepts_out_of_range_ampere() {
        assert_eq!(parse_ampere(r#"{"ampere": -3}"#), Ok(-3.0));
    }

    #[test]
    fn ignores_additive_extra_fields() {
        assert_eq!(parse_ampere(r#"{"ampere": 6.5, "foo": 1}"#), Ok(6.5));
    }

    #[test]
    fn missing_ampere_field_is_error() {
        assert_eq!(
            parse_ampere(r#"{"foo": 1}"#),
            Err(IntakeError::MissingField)
        );
    }

    #[test]
    fn non_numeric_ampere_is_bad_type() {
        assert_eq!(
            parse_ampere(r#"{"ampere": "x"}"#),
            Err(IntakeError::BadType)
        );
    }

    #[test]
    fn non_finite_ampere_is_rejected() {
        assert_eq!(
            parse_ampere(r#"{"ampere": 1e40}"#),
            Err(IntakeError::NotFinite)
        );
    }

    #[test]
    fn malformed_payload_is_missing_field() {
        assert_eq!(parse_ampere("not json"), Err(IntakeError::MissingField));
    }

    #[test]
    fn parses_enable_true() {
        assert_eq!(parse_enable(r#"{"enable": true}"#), Ok(true));
    }

    #[test]
    fn parses_enable_false() {
        assert_eq!(parse_enable(r#"{"enable": false}"#), Ok(false));
    }

    #[test]
    fn missing_enable_field_is_error() {
        assert_eq!(
            parse_enable(r#"{"ampere": 1}"#),
            Err(IntakeError::MissingField)
        );
    }

    #[test]
    fn non_bool_enable_is_bad_type() {
        assert_eq!(parse_enable(r#"{"enable": 1}"#), Err(IntakeError::BadType));
    }
}
