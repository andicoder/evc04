//! Render raw CN28 response bytes into the two human-readable MQTT views.

use alloc::format;
use alloc::string::String;

/// Lowercase, space-separated hex: `[0x0d, 0x0a]` → `"0d 0a"`, `[]` → `""`.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(3));
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// One log line as a single-line, JSON-safe string: printable ASCII verbatim,
/// `\r` / `\n` / `\t` named, everything else `\xNN`. Unlike [`to_printable`] this
/// is lossless — the 2026-09-02 fault (#3) turned on bytes a lossy view had
/// already thrown away, so the record body must be reversible.
pub fn to_escaped(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Printable ASCII (`0x20..=0x7e`) kept verbatim, every other byte shown as `.`.
pub fn to_printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_space_separated() {
        assert_eq!(to_hex(&[0x0d, 0x0a]), "0d 0a");
    }

    #[test]
    fn hex_of_empty_is_empty() {
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn printable_keeps_ascii_dots_the_rest() {
        assert_eq!(to_printable(&[0x50, 0x31, 0x09, 0x0a]), "P1..");
    }

    #[test]
    fn escaped_keeps_printable_ascii_verbatim() {
        assert_eq!(to_escaped(b"Temp: 33 C"), "Temp: 33 C");
    }

    #[test]
    fn escaped_names_the_whitespace_control_bytes() {
        assert_eq!(to_escaped(b"a\r\n\tb"), r"a\r\n\tb");
    }

    #[test]
    fn escaped_hexes_every_other_non_printable_byte() {
        assert_eq!(to_escaped(&[0x1b, 0x00, 0xff]), r"\x1b\x00\xff");
    }

    #[test]
    fn escaped_stays_json_safe() {
        assert_eq!(to_escaped(b"a\\b\"c"), r#"a\\b\"c"#);
    }
}
