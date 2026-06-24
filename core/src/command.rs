//! Decode an MQTT command payload into the raw bytes written to CN28.
//!
//! CN28 is strictly request/response: any byte triggers exactly one response
//! frame. To probe the shell surface (`help`, `?`, Tab, Ctrl+C, NUL runs) from
//! MQTT we need to express non-printable bytes as text, hence the escapes.

use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// A trailing `\` with nothing after it.
    DanglingEscape,
    /// `\x` followed by something other than two hex digits.
    BadHex,
    /// A `\` escape whose following char is not one we recognise.
    UnknownEscape(char),
}

/// Turn a command payload into raw bytes for CN28.
///
/// Recognised escapes: `\\`, `\r`, `\n`, `\t`, `\0`, `\xHH` (two hex digits).
/// Every other character is emitted as its ASCII/UTF-8 bytes.
pub fn decode_command(input: &str) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next().ok_or(DecodeError::DanglingEscape)? {
            '\\' => out.push(b'\\'),
            'r' => out.push(b'\r'),
            'n' => out.push(b'\n'),
            't' => out.push(b'\t'),
            '0' => out.push(0),
            'x' => {
                let hi = chars.next().ok_or(DecodeError::DanglingEscape)?;
                let lo = chars.next().ok_or(DecodeError::DanglingEscape)?;
                let hi = hi.to_digit(16).ok_or(DecodeError::BadHex)?;
                let lo = lo.to_digit(16).ok_or(DecodeError::BadHex)?;
                out.push((hi * 16 + lo) as u8);
            }
            other => return Err(DecodeError::UnknownEscape(other)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_becomes_ascii_bytes() {
        assert_eq!(
            decode_command("help").unwrap(),
            vec![0x68, 0x65, 0x6c, 0x70]
        );
    }

    #[test]
    fn cr_lf_escapes() {
        assert_eq!(decode_command("\\r\\n").unwrap(), vec![0x0d, 0x0a]);
    }

    #[test]
    fn hex_escape() {
        assert_eq!(decode_command("\\x03").unwrap(), vec![0x03]);
    }

    #[test]
    fn nul_escape() {
        assert_eq!(decode_command("\\0").unwrap(), vec![0x00]);
    }

    #[test]
    fn escaped_backslash() {
        assert_eq!(decode_command("\\\\").unwrap(), vec![0x5c]);
    }

    #[test]
    fn trailing_backslash_is_dangling() {
        assert_eq!(decode_command("a\\"), Err(DecodeError::DanglingEscape));
    }

    #[test]
    fn unknown_escape_reports_the_char() {
        assert_eq!(decode_command("\\q"), Err(DecodeError::UnknownEscape('q')));
    }

    #[test]
    fn bad_hex_digits() {
        assert_eq!(decode_command("\\xZZ"), Err(DecodeError::BadHex));
    }
}
