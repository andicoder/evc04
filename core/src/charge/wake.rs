//! evcc-commanded control-pilot interrupt (SPECS §7).
//!
//! The firmware decides nothing here: evcc's loadpoint already knows when a car is
//! stuck in `B` below its SoC limit and calls the charger's wakeup, exactly as it
//! does for its own pilot-relay chargers. This is only the pulse those chargers
//! run — one length per command, never stretched by a repeat.

/// The longest pulse a command may ask for. A pilot held open longer than a
/// deliberate unplug-and-replug is no longer a wake.
pub const MAX_PULSE_S: u64 = 180;

/// How long after the relay closes the box may still decode the pilot as `A`
/// (measured: ~4 s). Bounded, so an unplug during the pulse still gets through.
pub const SETTLE_MS: u64 = 10_000;

/// One pulse at a time, timed on the firmware's monotonic milliseconds.
pub struct CpWake {
    pulse_ms: u64,
    open_until_ms: Option<u64>,
    settled: bool,
    attempts: u32,
}

impl CpWake {
    pub fn new(pulse_ms: u64) -> Self {
        Self {
            pulse_ms,
            open_until_ms: None,
            settled: true,
            attempts: 0,
        }
    }

    /// Start a pulse of `pulse_ms`, or the default length, unless one is running;
    /// returns whether this one started.
    pub fn request(&mut self, now_ms: u64, pulse_ms: Option<u64>) -> bool {
        if self.is_open(now_ms) {
            return false;
        }
        self.open_until_ms = Some(now_ms + pulse_ms.unwrap_or(self.pulse_ms));
        self.settled = false;
        self.attempts += 1;
        true
    }

    pub fn is_open(&self, now_ms: u64) -> bool {
        self.open_until_ms.is_some_and(|until| now_ms < until)
    }

    /// Pulses started since boot.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The box decodes our own open pilot as `A`, and keeps doing so for a few
    /// seconds after the relay closes; an `A` reaching evcc is an unplug that would
    /// end the session mid-wakeup. Until the box reads the car again the car is
    /// still connected and not charging, which is `B`.
    pub fn charge_state<'a>(&mut self, letter: &'a str, now_ms: u64) -> &'a str {
        let Some(until) = self.open_until_ms else {
            return letter;
        };
        if now_ms < until {
            return "B";
        }
        if self.settled || now_ms >= until + SETTLE_MS {
            self.settled = true;
            return letter;
        }
        if letter == "A" || letter.is_empty() {
            return "B";
        }
        self.settled = true;
        letter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PULSE_MS: u64 = 12_000;

    #[test]
    fn idle_until_commanded() {
        let wake = CpWake::new(PULSE_MS);
        assert!(!wake.is_open(0));
        assert_eq!(wake.attempts(), 0);
    }

    #[test]
    fn a_command_opens_the_pilot_for_exactly_one_pulse() {
        let mut wake = CpWake::new(PULSE_MS);
        assert!(wake.request(1_000, None));
        assert!(wake.is_open(1_000));
        assert!(wake.is_open(1_000 + PULSE_MS - 1));
        assert!(!wake.is_open(1_000 + PULSE_MS));
    }

    #[test]
    fn a_command_can_name_its_own_pulse_length() {
        let mut wake = CpWake::new(PULSE_MS);
        assert!(wake.request(0, Some(60_000)));
        assert!(wake.is_open(59_999));
        assert!(!wake.is_open(60_000));
    }

    #[test]
    fn a_named_length_applies_to_that_pulse_only() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, Some(60_000));
        assert!(wake.request(100_000, None));
        assert!(!wake.is_open(100_000 + PULSE_MS));
    }

    #[test]
    fn a_command_during_a_pulse_does_not_stretch_it() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert!(!wake.request(5_000, Some(60_000)));
        assert!(!wake.is_open(PULSE_MS));
        assert_eq!(wake.attempts(), 1);
    }

    #[test]
    fn every_finished_pulse_can_be_followed_by_the_next_command() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert!(!wake.is_open(PULSE_MS));
        assert!(wake.request(60_000, None));
        assert!(wake.is_open(60_000));
        assert_eq!(wake.attempts(), 2);
    }

    #[test]
    fn the_pulse_reads_as_connected_not_unplugged() {
        // The box decodes our own open pilot as `A`; evcc must not see an unplug.
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert_eq!(wake.charge_state("A", 2_000), "B");
        assert_eq!(wake.charge_state("", 3_000), "B");
    }

    #[test]
    fn the_box_still_decoding_a_after_the_relay_closed_reads_as_connected() {
        // Measured: the box reports `A` for ~4 s after the pilot is back.
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert_eq!(wake.charge_state("A", PULSE_MS + 4_000), "B");
    }

    #[test]
    fn once_the_box_reads_the_car_again_a_later_a_is_a_real_unplug() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert_eq!(wake.charge_state("B", PULSE_MS + 4_000), "B");
        assert_eq!(wake.charge_state("A", PULSE_MS + 5_000), "A");
    }

    #[test]
    fn an_a_that_outlasts_the_settle_window_is_a_real_unplug() {
        // Unplugged during the pulse: the mask must not hide it for long.
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        assert_eq!(wake.charge_state("A", PULSE_MS + SETTLE_MS), "A");
    }

    #[test]
    fn a_new_pulse_masks_again_after_an_earlier_one_settled() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0, None);
        wake.charge_state("B", PULSE_MS + 1_000);
        wake.request(60_000, None);
        assert_eq!(wake.charge_state("A", 60_000 + PULSE_MS + 1_000), "B");
    }

    #[test]
    fn without_a_pulse_the_letter_passes_through() {
        let mut wake = CpWake::new(PULSE_MS);
        for letter in ["A", "B", "C", ""] {
            assert_eq!(wake.charge_state(letter, 1_000), letter);
        }
    }
}
