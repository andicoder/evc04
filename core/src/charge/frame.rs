//! Modbus RTU framing for the emulated Inepro PRO380 meter (SPECS.md §4–§5).
//!
//! The RS485 *slave* counterpart to `command`/`dump`: the EVC04 (master) polls us
//! for three per-phase currents as `3× Float32` big-endian (ABCD byte order)
//! starting at register `0x500C`. We build the FC03 response by hand so the exact
//! bytes are verifiable against the frames captured in SPECS.md §5/§9.
//!
//! Pure and `no_std` so the on-box ESP32 firmware (evc04#85) reuses the same
//! verified logic the daemon proved on real hardware — no second implementation to
//! drift.

use alloc::vec::Vec;

/// Encode the three per-phase currents (amps) as the 12-byte FC03 payload:
/// `struct.pack('>fff', l1, l2, l3)` — big-endian float32, ABCD byte order.
pub fn encode_currents(l1: f32, l2: f32, l3: f32) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&l1.to_be_bytes());
    out[4..8].copy_from_slice(&l2.to_be_bytes());
    out[8..12].copy_from_slice(&l3.to_be_bytes());
    out
}

/// Standard Modbus RTU CRC16 (poly 0xA001, init 0xFFFF). The returned value is
/// transmitted low byte first — use `.to_le_bytes()` when appending to a frame.
pub fn modbus_crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Build the full FC03 (Read Holding Registers) response frame:
/// `addr | 0x03 | byte_count | payload… | CRC16(lo, hi)`. `payload` is the data
/// section (e.g. from [`encode_currents`]); `byte_count` is its length.
pub fn build_response(addr: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(3 + payload.len() + 2);
    frame.push(addr);
    frame.push(FC_READ_HOLDING);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&modbus_crc16(&frame).to_le_bytes());
    frame
}

/// FC code for Read Holding Registers — the only function the EVC04 ever issues.
pub const FC_READ_HOLDING: u8 = 0x03;

/// A parsed inbound Modbus-RTU read request (addr | fc | start | qty), CRC already
/// verified. Classification of *which* request this is (i.e. our poll) is the
/// caller's job via [`ParsedRequest::is_poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedRequest {
    pub addr: u8,
    pub fc: u8,
    pub start: u16,
    pub qty: u16,
}

impl ParsedRequest {
    /// True iff this is the meter poll we emulate: matching slave address, FC03,
    /// start register and quantity. Defaults live in `SPECS.md` §4 (addr 1,
    /// `0x500C`, qty 6) but are passed in so the firmware's config drives them.
    pub fn is_poll(&self, addr: u8, register: u16, qty: u16) -> bool {
        self.addr == addr && self.fc == FC_READ_HOLDING && self.start == register && self.qty == qty
    }
}

/// Parse an inbound 8-byte RTU read request: `addr | fc | start_hi | start_lo |
/// qty_hi | qty_lo | crc_lo | crc_hi`. Returns the fields only when the length is
/// exact and the trailing CRC16 checks out; a bad CRC or wrong length yields
/// `None` (the frame is dropped — see the why-note below).
pub fn parse_request(frame: &[u8]) -> Option<ParsedRequest> {
    // A read request is exactly 8 bytes: 6-byte body + 2-byte CRC.
    let [body @ .., crc_lo, crc_hi] = frame else {
        return None;
    };
    if body.len() != 6 || modbus_crc16(body).to_le_bytes() != [*crc_lo, *crc_hi] {
        // Drop on bad framing/CRC. The poll cadence is content-agnostic (SPECS.md
        // §4) — staying silent costs nothing, and we never want to act on a frame
        // we can't trust. Frames not addressed to us are likewise dropped by the
        // caller via `is_poll`, matching Modbus's "silence on foreign address".
        return None;
    }
    Some(ParsedRequest {
        addr: body[0],
        fc: body[1],
        start: u16::from_be_bytes([body[2], body[3]]),
        qty: u16::from_be_bytes([body[4], body[5]]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Append a correct CRC16 (low byte first) to a request body.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut frame = body.to_vec();
        frame.extend_from_slice(&modbus_crc16(body).to_le_bytes());
        frame
    }

    // SPECS.md §4 verified poll frame.
    const SPEC_POLL: [u8; 8] = [0x01, 0x03, 0x50, 0x0c, 0x00, 0x06, 0x14, 0xcb];

    #[test]
    fn parses_the_spec_poll_frame() {
        assert_eq!(
            parse_request(&SPEC_POLL),
            Some(ParsedRequest {
                addr: 1,
                fc: 0x03,
                start: 0x500C,
                qty: 6,
            })
        );
    }

    #[test]
    fn recognises_the_spec_poll_frame_as_the_poll() {
        let req = parse_request(&SPEC_POLL).expect("spec frame parses");
        assert!(req.is_poll(1, 0x500C, 6));
    }

    #[test]
    fn rejects_frame_with_corrupted_crc() {
        let mut frame = SPEC_POLL;
        frame[7] ^= 0xff; // flip the high CRC byte
        assert_eq!(parse_request(&frame), None);
    }

    #[test]
    fn rejects_frame_of_wrong_length() {
        assert_eq!(parse_request(&[0x01, 0x03, 0x50, 0x0c]), None);
    }

    #[test]
    fn wrong_function_code_is_not_the_poll() {
        let req = parse_request(&framed(&[0x01, 0x04, 0x50, 0x0c, 0x00, 0x06])).unwrap();
        assert!(!req.is_poll(1, 0x500C, 6));
    }

    #[test]
    fn wrong_start_register_is_not_the_poll() {
        let req = parse_request(&framed(&[0x01, 0x03, 0x50, 0x00, 0x00, 0x06])).unwrap();
        assert!(!req.is_poll(1, 0x500C, 6));
    }

    #[test]
    fn wrong_quantity_is_not_the_poll() {
        let req = parse_request(&framed(&[0x01, 0x03, 0x50, 0x0c, 0x00, 0x02])).unwrap();
        assert!(!req.is_poll(1, 0x500C, 6));
    }

    #[test]
    fn wrong_slave_address_is_not_the_poll() {
        let req = parse_request(&framed(&[0x02, 0x03, 0x50, 0x0c, 0x00, 0x06])).unwrap();
        assert!(!req.is_poll(1, 0x500C, 6));
    }

    // SPECS.md §5 verified payloads (the 12 data bytes, without addr/FC/count/CRC).
    #[test]
    fn encodes_zero_amps_as_all_zero_payload() {
        assert_eq!(encode_currents(0.0, 0.0, 0.0), [0u8; 12]);
    }

    #[test]
    fn encodes_16_amps_per_phase() {
        let expected = [
            0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00,
        ];
        assert_eq!(encode_currents(16.0, 16.0, 16.0), expected);
    }

    #[test]
    fn encodes_63_amps_per_phase() {
        let expected = [
            0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00, 0x00,
        ];
        assert_eq!(encode_currents(63.0, 63.0, 63.0), expected);
    }

    // SPECS.md §5/§9: CRC is transmitted low byte first, hence `.to_le_bytes()`.
    #[test]
    fn crc16_of_poll_frame_matches_spec() {
        // 01 03 50 0c 00 06 → 14 cb
        let body = [0x01, 0x03, 0x50, 0x0c, 0x00, 0x06];
        assert_eq!(modbus_crc16(&body).to_le_bytes(), [0x14, 0xcb]);
    }

    #[test]
    fn crc16_of_zero_amp_response_matches_spec() {
        // 01 03 0c 00000000 00000000 00000000 → 93 70
        let body = [0x01, 0x03, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(modbus_crc16(&body).to_le_bytes(), [0x93, 0x70]);
    }

    #[test]
    fn crc16_of_16_amp_response_matches_spec() {
        // 01 03 0c 41800000×3 → 97 ae
        let body = [
            0x01, 0x03, 0x0c, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00,
            0x00,
        ];
        assert_eq!(modbus_crc16(&body).to_le_bytes(), [0x97, 0xae]);
    }

    #[test]
    fn crc16_of_63_amp_response_matches_spec() {
        // 01 03 0c 427c0000×3 → 13 97
        let body = [
            0x01, 0x03, 0x0c, 0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00,
            0x00,
        ];
        assert_eq!(modbus_crc16(&body).to_le_bytes(), [0x13, 0x97]);
    }

    // SPECS.md §5 full verified response frames (addr 01, FC 03, count 0x0c, +CRC16).
    #[test]
    fn builds_zero_amp_response_frame() {
        let frame = build_response(1, &encode_currents(0.0, 0.0, 0.0));
        let expected = [
            0x01, 0x03, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x93, 0x70,
        ];
        assert_eq!(frame, expected);
    }

    #[test]
    fn builds_16_amp_response_frame() {
        let frame = build_response(1, &encode_currents(16.0, 16.0, 16.0));
        let expected = [
            0x01, 0x03, 0x0c, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00,
            0x00, 0x97, 0xae,
        ];
        assert_eq!(frame, expected);
    }

    #[test]
    fn builds_63_amp_response_frame() {
        let frame = build_response(1, &encode_currents(63.0, 63.0, 63.0));
        let expected = [
            0x01, 0x03, 0x0c, 0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00, 0x00, 0x42, 0x7c, 0x00,
            0x00, 0x13, 0x97,
        ];
        assert_eq!(frame, expected);
    }
}
