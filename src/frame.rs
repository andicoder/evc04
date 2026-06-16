//! Modbus RTU framing for the emulated Inepro PRO380 meter (SPECS.md §4–§5).
//!
//! The EVC04 reads three per-phase currents as `3× Float32` big-endian (ABCD byte
//! order) starting at register `0x500C`. We build the FC03 response by hand so the
//! exact bytes are verifiable against the frames captured in SPECS.md §5/§11.

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
    frame.push(0x03);
    frame.push(payload.len() as u8);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&modbus_crc16(&frame).to_le_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // SPECS.md §5/§11: CRC is transmitted low byte first, hence `.to_le_bytes()`.
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
