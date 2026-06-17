//! Control math for the EVC04 meter emulation.
//!
//! The EVC04 Power Optimizer runs a *closed loop*: it measures the total main-line
//! current (the car's own draw included) and ramps charging until that total reaches the
//! box's ceiling. A static reported value never rises with the draw, so the box only ever
//! ramps to full or cuts off — on/off, not modulation (verified on hardware, #21).
//!
//! We close the loop instead: we report `offset + live_measured_current`, a value that
//! *rises with the real draw*, so the box settles at a stable current. See `SPECS.md` §6.

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

/// Household current (per phase) to report so the EVC04's closed loop settles the car at
/// `target` charge amps, given the `measured` current actually flowing right now.
///
/// `max` is the box's own ceiling, set by its DIP switches 4-5-6 (not a fuse we protect —
/// see `SPECS.md` §6). With `offset = max − target` (target clamped to `[0, max]`), we
/// serve `reported = clamp(offset + measured, 0, max)`:
/// - `target = max` → `offset 0` → report the bare `measured`: the box holds the total at
///   the ceiling, i.e. as much charge as the ceiling allows.
/// - a partial `target` → report `offset + measured`, which rises as the car draws more,
///   so the box backs off and settles around `target`.
/// - `target < min_charge` → report `max` (zero headroom → pause): below the 3-phase floor
///   the loop can't hold a stable current, so we don't try to modulate it.
pub fn reported_household(
    max: Ampere,
    target: Ampere,
    measured: Ampere,
    min_charge: Ampere,
) -> Ampere {
    if target.0 < min_charge.0 {
        return max;
    }
    let offset = max - target.clamp(Ampere(0.0), max);
    Ampere(offset.0 + measured.0).clamp(Ampere(0.0), max)
}

pub mod config;
pub mod control;
pub mod frame;
pub mod mqtt;
pub mod slave;

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Ampere = Ampere(6.0);

    #[test]
    fn offset_zero_reports_the_bare_measured_current() {
        // target = max → offset 0 → report whatever is measured (box holds at the ceiling).
        assert_eq!(
            reported_household(Ampere(16.0), Ampere(16.0), Ampere(9.0), MIN),
            Ampere(9.0)
        );
    }

    #[test]
    fn below_min_charge_reports_the_ceiling_to_pause() {
        assert_eq!(
            reported_household(Ampere(16.0), Ampere(5.0), Ampere(0.0), MIN),
            Ampere(16.0)
        );
    }
}
