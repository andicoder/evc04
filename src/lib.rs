//! Control math for the EVC04 meter emulation.
//!
//! The EVC04 Power Optimizer allows `charge_current ≤ max − household_current`.
//! We don't measure a household — we fabricate the "household current" we report so the
//! box leaves exactly the headroom we want the car to draw. See `SPECS.md` §6.

/// An electric current in amperes. A newtype (like `std::time::Duration` for time) so the
/// unit lives in the type, not in field-name suffixes, and amps can't be mixed up with
/// other scalars. The inner `f32` is exposed only to cross the wire boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ampere(pub f32);

impl Ampere {
    /// Constrain to `[lo, hi]`. `lo`/`hi` come from validated config, so they are finite
    /// and ordered (no `f32::clamp` panic).
    pub fn clamp(self, lo: Ampere, hi: Ampere) -> Ampere {
        Ampere(self.0.clamp(lo.0, hi.0))
    }
}

impl std::ops::Sub for Ampere {
    type Output = Ampere;
    fn sub(self, rhs: Ampere) -> Ampere {
        Ampere(self.0 - rhs.0)
    }
}

/// Household current (per phase) to report so the EVC04 permits `target` charge amps.
///
/// `max` is the box's own current ceiling, set by its DIP switches 4-5-6 (not a fuse we
/// protect — see `SPECS.md` §6). `reported = max − target`, with `target` clamped to
/// `[0, max]`:
/// - `target = max` → report `0` → maximum charge.
/// - `target = 0` → report `max` → zero headroom → charging pauses.
pub fn reported_household(max: Ampere, target: Ampere) -> Ampere {
    max - target.clamp(Ampere(0.0), max)
}

pub mod config;
pub mod control;
pub mod frame;
pub mod mqtt;
pub mod slave;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_zero_reports_the_full_ceiling() {
        assert_eq!(reported_household(Ampere(16.0), Ampere(0.0)), Ampere(16.0));
    }

    #[test]
    fn zero_report_means_max_charge() {
        assert_eq!(reported_household(Ampere(16.0), Ampere(16.0)), Ampere(0.0));
    }
}
