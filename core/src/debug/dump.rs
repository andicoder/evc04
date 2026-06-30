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
}
