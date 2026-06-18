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
    reported_from_offset(max, offset, measured)
}

/// Report for an already-resolved `offset` (steady-state `max − target`, or the
/// soft-ramped value the live loop serves — #24): `clamp(offset + measured, 0, max)`.
pub(crate) fn reported_from_offset(max: Ampere, offset: Ampere, measured: Ampere) -> Ampere {
    Ampere(offset.0 + measured.0).clamp(Ampere(0.0), max)
}

/// Move `offset` toward `setpoint` by at most `max_step`, without overshooting (#24).
///
/// The EVC04's closed loop over-throttles below the car's floor when the offset jumps in
/// one step (measured on hardware); rate-limiting it keeps the loop stable. `max_step` is
/// the per-tick budget the ramp driver derives from `RAMP_RATE_AMPERE_PER_SECOND × dt`.
pub fn ramp_step(offset: Ampere, setpoint: Ampere, max_step: Ampere) -> Ampere {
    let delta = setpoint.0 - offset.0;
    if delta.abs() <= max_step.0 {
        setpoint
    } else {
        Ampere(offset.0 + max_step.0 * delta.signum())
    }
}

/// Below this measured current the loop is treated as no charge actually flowing
/// (float noise / standby), so the state reads "B" rather than "C". A coarse floor:
/// the measured input is source-agnostic (grid current today), so this is a
/// best-effort proxy, not a precise car-draw threshold.
const CHARGING_FLOOR: Ampere = Ampere(1.0);

/// evcc charge status (`api.ChargeStatus`: A idle / B connected, not charging / C
/// charging), approximated from what the meter emulation can observe.
///
/// We have **no control-pilot line**, so this is best-effort: "C" while charge is
/// allowed (`reported` below the ceiling — not a hard pause) *and* current is
/// actually flowing, otherwise "B" (connected, not charging — a paused box, or
/// enabled-but-not-yet-drawing). **"A" (no vehicle) is never asserted**: a meter
/// emulation can't tell an unplugged car from a connected-but-idle one, so evcc must
/// rely on its own vehicle detection for that (docs/evcc.md).
pub fn charge_state(reported: Ampere, max: Ampere, measured: Ampere) -> &'static str {
    let charge_allowed = reported.0 < max.0;
    let current_flowing = measured.0 > CHARGING_FLOOR.0;
    if charge_allowed && current_flowing {
        "C"
    } else {
        "B"
    }
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

    const MAX: Ampere = Ampere(16.0);

    #[test]
    fn charging_state_is_c_while_charge_is_allowed_and_current_flows() {
        // reported below the ceiling → not paused; measured current flowing → the car draws.
        assert_eq!(charge_state(Ampere(2.0), MAX, Ampere(9.0)), "C");
    }

    #[test]
    fn charging_state_is_b_when_paused_at_the_ceiling() {
        // A hard pause serves the ceiling; evcc must read "connected, not charging".
        assert_eq!(charge_state(MAX, MAX, Ampere(0.0)), "B");
    }

    #[test]
    fn charging_state_is_b_while_charge_is_allowed_but_no_current_flows() {
        // Enabled but nothing drawing yet (ramp-up, or no car) → connected, not charging.
        assert_eq!(charge_state(Ampere(0.0), MAX, Ampere(0.0)), "B");
    }
}
