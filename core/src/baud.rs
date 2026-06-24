//! Parse and validate the runtime-baud MQTT payload (`evc04/cn28/baud`, #79).
//!
//! The prober can re-tune its UART rate live so the CN28 LOG baud can be swept
//! without a reflash. The payload is a plain integer rate (e.g. `"9600"`).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaudError {
    /// Payload was empty (after trimming).
    Empty,
    /// Payload was not a base-10 integer.
    NotANumber,
    /// Parsed, but outside the rates the UART will accept.
    OutOfRange(u32),
}

/// Lowest and highest UART rate we let through. The CN28 LOG candidates
/// (9600–115200) sit well inside; the bounds just reject typos and nonsense
/// before they reach `uart_set_baudrate`.
const MIN_BAUD: u32 = 300;
const MAX_BAUD: u32 = 4_000_000;

/// Parse a `evc04/cn28/baud` payload into a validated UART rate.
pub fn parse_baud(input: &str) -> Result<u32, BaudError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(BaudError::Empty);
    }
    let rate: u32 = trimmed.parse().map_err(|_| BaudError::NotANumber)?;
    if (MIN_BAUD..=MAX_BAUD).contains(&rate) {
        Ok(rate)
    } else {
        Err(BaudError::OutOfRange(rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_common_rate() {
        assert_eq!(parse_baud("9600"), Ok(9600));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_baud(" 19200\n"), Ok(19200));
    }

    #[test]
    fn empty_payload_is_empty_error() {
        assert_eq!(parse_baud("   "), Err(BaudError::Empty));
    }

    #[test]
    fn non_numeric_is_not_a_number() {
        assert_eq!(parse_baud("fast"), Err(BaudError::NotANumber));
    }

    #[test]
    fn zero_is_out_of_range() {
        assert_eq!(parse_baud("0"), Err(BaudError::OutOfRange(0)));
    }

    #[test]
    fn absurdly_high_is_out_of_range() {
        assert_eq!(parse_baud("9000000"), Err(BaudError::OutOfRange(9_000_000)));
    }
}
