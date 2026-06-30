//! Capped exponential backoff for retrying a flaky operation (the WiFi join, #103)
//! before the firmware falls back to a reboot. Pure so the retry schedule is
//! host-tested; the firmware supplies the timing and owns the esp-idf glue.

use core::time::Duration;

/// The delay to wait *before* retry `attempt` (0-based): `base` doubled each step,
/// clamped to `cap`. Overflow-safe — a large `attempt` simply lands on `cap`.
pub fn capped_exponential(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let scaled = (base.as_millis() as u64).saturating_mul(factor);
    Duration::from_millis(scaled.min(cap.as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(500);
    const CAP: Duration = Duration::from_secs(8);

    #[test]
    fn first_attempt_waits_the_base() {
        assert_eq!(capped_exponential(0, BASE, CAP), Duration::from_millis(500));
    }

    #[test]
    fn doubles_each_step() {
        assert_eq!(
            capped_exponential(1, BASE, CAP),
            Duration::from_millis(1000)
        );
        assert_eq!(
            capped_exponential(2, BASE, CAP),
            Duration::from_millis(2000)
        );
        assert_eq!(
            capped_exponential(3, BASE, CAP),
            Duration::from_millis(4000)
        );
    }

    #[test]
    fn clamps_to_the_cap() {
        // 500ms << 4 == 8000ms hits the cap exactly; beyond stays there.
        assert_eq!(capped_exponential(4, BASE, CAP), Duration::from_secs(8));
        assert_eq!(capped_exponential(5, BASE, CAP), Duration::from_secs(8));
    }

    #[test]
    fn huge_attempt_does_not_overflow() {
        assert_eq!(capped_exponential(100, BASE, CAP), Duration::from_secs(8));
        assert_eq!(
            capped_exponential(u32::MAX, BASE, CAP),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn base_above_cap_yields_cap() {
        assert_eq!(
            capped_exponential(0, Duration::from_secs(10), CAP),
            Duration::from_secs(8)
        );
    }
}
