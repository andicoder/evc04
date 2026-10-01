//! evcc-commanded control-pilot interrupt (SPECS §7).
//!
//! The firmware decides nothing here: evcc's loadpoint already knows when a car is
//! stuck in `B` below its SoC limit and calls the charger's wakeup, exactly as it
//! does for its own pilot-relay chargers. This is only the pulse those chargers
//! run — fixed length, never stretched by a repeat command.

/// One pulse at a time, timed on the firmware's monotonic milliseconds.
pub struct CpWake {
    pulse_ms: u64,
    open_until_ms: Option<u64>,
    attempts: u32,
}

impl CpWake {
    pub fn new(pulse_ms: u64) -> Self {
        Self {
            pulse_ms,
            open_until_ms: None,
            attempts: 0,
        }
    }

    /// Start a pulse unless one is running; returns whether this one started.
    pub fn request(&mut self, now_ms: u64) -> bool {
        if self.is_open(now_ms) {
            return false;
        }
        self.open_until_ms = Some(now_ms + self.pulse_ms);
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
}

/// The box decodes our own open pilot as `A`, and an `A` reaching evcc is an
/// unplug — it would end the session and reset the loadpoint mid-wakeup. While the
/// line is open the car is still connected and not charging, which is `B`.
pub fn charge_state_during_pulse(letter: &str, pulse_open: bool) -> &str {
    if pulse_open {
        "B"
    } else {
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
        assert!(wake.request(1_000));
        assert!(wake.is_open(1_000));
        assert!(wake.is_open(1_000 + PULSE_MS - 1));
        assert!(!wake.is_open(1_000 + PULSE_MS));
    }

    #[test]
    fn a_command_during_a_pulse_does_not_stretch_it() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0);
        assert!(!wake.request(5_000));
        assert!(!wake.is_open(PULSE_MS));
        assert_eq!(wake.attempts(), 1);
    }

    #[test]
    fn every_finished_pulse_can_be_followed_by_the_next_command() {
        let mut wake = CpWake::new(PULSE_MS);
        wake.request(0);
        assert!(!wake.is_open(PULSE_MS));
        assert!(wake.request(60_000));
        assert!(wake.is_open(60_000));
        assert_eq!(wake.attempts(), 2);
    }

    #[test]
    fn the_pulse_reads_as_connected_not_unplugged() {
        // The box decodes our own open pilot as `A`; evcc must not see an unplug.
        assert_eq!(charge_state_during_pulse("A", true), "B");
        assert_eq!(charge_state_during_pulse("", true), "B");
    }

    #[test]
    fn outside_a_pulse_the_letter_passes_through() {
        for letter in ["A", "B", "C", ""] {
            assert_eq!(charge_state_during_pulse(letter, false), letter);
        }
    }
}
