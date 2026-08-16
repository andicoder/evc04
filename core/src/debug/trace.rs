//! Per-read instrumentation for one CN28 probe window (evc04#159).
//!
//! The read loop hands the driver a scratch buffer and trusts the byte count it
//! returns. A capture on 2026-08-16 showed a frozen 16-byte block appearing
//! exactly twice in every window, which is either the box interleaving two output
//! streams or the read over-reporting — the raw bytes alone cannot tell the two
//! apart, because nothing records what each individual `read()` claimed.
//!
//! This does. The caller poisons the scratch buffer with [`POISON`] before every
//! read; any poison byte still standing *inside* the range the driver claims to
//! have filled is a byte it did not write. The LOG stream is ASCII, so `0xFF`
//! cannot be real payload and a surviving poison byte is unambiguous.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// Scratch-buffer poison. Outside ASCII, so it can never collide with real LOG
/// payload — a poison byte inside a read's claimed range was never written.
pub const POISON: u8 = 0xFF;

/// One `read()` call's accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    /// Bytes the driver claimed to have read.
    pub claimed: usize,
    /// Of those, how many are still poison — i.e. were never written.
    pub unwritten: usize,
}

/// Every read in one probe window, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadTrace {
    reads: Vec<ReadRecord>,
}

impl ReadTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account for one read, given the slice the driver claims it filled.
    pub fn record(&mut self, filled: &[u8]) {
        self.reads.push(ReadRecord {
            claimed: filled.len(),
            unwritten: filled.iter().filter(|&&b| b == POISON).count(),
        });
    }

    pub fn reads(&self) -> &[ReadRecord] {
        &self.reads
    }

    /// `{"reads":[{"n":<claimed>,"unwritten":<n>},…],"total":<sum>,"unwritten":<sum>}`
    /// — flat and fixed-order so a capture can be diffed across windows.
    pub fn to_json(&self) -> String {
        let mut s = String::from("{\"reads\":[");
        for (i, r) in self.reads.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "{{\"n\":{},\"unwritten\":{}}}", r.claimed, r.unwritten);
        }
        let total: usize = self.reads.iter().map(|r| r.claimed).sum();
        let unwritten: usize = self.reads.iter().map(|r| r.unwritten).sum();
        s.push_str(&format!("],\"total\":{total},\"unwritten\":{unwritten}}}"));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_window_serialises_to_zero_totals() {
        assert_eq!(
            ReadTrace::new().to_json(),
            r#"{"reads":[],"total":0,"unwritten":0}"#
        );
    }

    #[test]
    fn a_fully_written_read_reports_no_unwritten_bytes() {
        let mut t = ReadTrace::new();
        t.record(b"P2:\tV: 1");
        assert_eq!(
            t.reads(),
            [ReadRecord {
                claimed: 8,
                unwritten: 0
            }]
        );
    }

    #[test]
    fn poison_left_inside_the_claimed_range_counts_as_unwritten() {
        // The decisive case: the driver claims 8 bytes but only wrote 3, so the
        // caller would append 5 bytes of whatever the buffer held before.
        let mut t = ReadTrace::new();
        t.record(&[b'P', b'2', b':', POISON, POISON, POISON, POISON, POISON]);
        assert_eq!(
            t.reads(),
            [ReadRecord {
                claimed: 8,
                unwritten: 5
            }]
        );
    }

    #[test]
    fn every_read_of_a_window_is_kept_in_order() {
        let mut t = ReadTrace::new();
        t.record(b"ab");
        t.record(&[b'c', POISON]);
        t.record(b"defg");
        assert_eq!(
            t.to_json(),
            r#"{"reads":[{"n":2,"unwritten":0},{"n":2,"unwritten":1},{"n":4,"unwritten":0}],"total":8,"unwritten":1}"#
        );
    }
}
