//! Pure closed-loop control math for the meter emulation (SPECS.md §6/§9).
//!
//! Ported `no_std` from the `charge` daemon's `control.rs`/`lib.rs` so the on-box
//! firmware (evc04#86) serves the **same** hardware-proven value instead of a
//! second implementation that could drift. The daemon wires the live state through
//! tokio `watch` channels and derives staleness from `Instant`s; here that is all
//! reduced to plain inputs — [`ControlInputs`] — so this layer owns no clock and no
//! I/O. The firmware owns the clock, holds the last target/measurement/offset, and
//! decides staleness; it then calls [`reported_current`] each poll.

/// An electric current in amperes. A newtype so the unit lives in the type, not in
/// field-name suffixes; the inner `f32` is exposed only to cross the wire boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ampere(pub f32);

impl Ampere {
    /// Constrain to `[lo, hi]`. `lo`/`hi` come from validated config, so they are
    /// finite and ordered.
    pub fn clamp(self, lo: Ampere, hi: Ampere) -> Ampere {
        Ampere(self.0.clamp(lo.0, hi.0))
    }
}

impl core::ops::Sub for Ampere {
    type Output = Ampere;
    fn sub(self, rhs: Ampere) -> Ampere {
        Ampere(self.0 - rhs.0)
    }
}

/// Direction a staleness failsafe takes when an input ages out (SPECS §9, #51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailsafeMode {
    /// Serve `reported = 0` — the meterless-box default, "never worse than no tool".
    FullCharge,
    /// Keep serving the last commanded value through the loop (a stale pause stays a
    /// pause, a stale charge stays a charge).
    HoldLast,
    /// Serve a value **above** the ceiling so the box pauses — the safe direction for
    /// an evcc-managed box. Reporting exactly the ceiling does not cut an active
    /// charge (#57).
    Pause,
}

impl FailsafeMode {
    /// The forced per-phase report when this failsafe engages, or `None` for
    /// `hold_last` (serve the held value through the normal loop instead).
    /// `pause_report` is the stop value (`max + margin`, exceeding the ceiling, #57).
    pub fn forced_report(self, pause_report: Ampere) -> Option<Ampere> {
        match self {
            FailsafeMode::FullCharge => Some(Ampere(0.0)),
            FailsafeMode::Pause => Some(pause_report),
            FailsafeMode::HoldLast => None,
        }
    }
}

/// Per-phase value to report to **stop** the box: the ceiling *plus a margin*.
/// Reporting exactly `max` does not cut an actively charging car — only a value that
/// exceeds the limit forces the cut (confirmed on hardware, #57).
pub fn pause_report(max: Ampere, margin: Ampere) -> Ampere {
    Ampere(max.0 + margin.0)
}

/// Report for an already-resolved `offset` (the soft-ramped value the live loop
/// serves, #24): `clamp(offset + measured, 0, max)`.
pub fn reported_from_offset(max: Ampere, offset: Ampere, measured: Ampere) -> Ampere {
    Ampere(offset.0 + measured.0).clamp(Ampere(0.0), max)
}

/// Move `offset` toward `setpoint` by at most `max_step`, without overshooting (#24).
///
/// The EVC04's closed loop over-throttles below the car's floor when the offset jumps
/// in one step; rate-limiting keeps the loop stable. `max_step` is the per-tick budget
/// the firmware derives from `RAMP_RATE_AMPERE_PER_SECOND × dt` (so it is `>= 0`).
pub fn ramp_step(offset: Ampere, setpoint: Ampere, max_step: Ampere) -> Ampere {
    // Equivalent to the daemon's `delta.abs() <= max_step` test, but without
    // `f32::abs`/`signum` (libm-gated in no_std): step toward the setpoint by at
    // most `max_step`, snapping exactly onto it once within one step.
    let delta = setpoint.0 - offset.0;
    if delta > max_step.0 {
        Ampere(offset.0 + max_step.0)
    } else if delta < -max_step.0 {
        Ampere(offset.0 - max_step.0)
    } else {
        setpoint
    }
}

/// Advance the integral trim by one **fresh CN28 sample** (#119). The trim integrates
/// the error between the box's actual delivered current and the target: while the box
/// charges *above* target, the term grows, lifting `reported` so the box throttles
/// further — a slow floor-seeker below the band the base loop can hold.
///
/// Stepped once per fresh sample (~5 s), **not** per 1 s tick — integrating stale data
/// each tick would over-correct ~5×. `ki` is small; `trim_max` is the anti-windup
/// clamp, and the term never goes negative (it only ever *reduces* charge).
pub fn trim_step(
    trim: Ampere,
    cn28_actual: Ampere,
    target: Ampere,
    ki: f32,
    trim_max: Ampere,
) -> Ampere {
    let next = trim.0 + ki * (cn28_actual.0 - target.0);
    Ampere(next).clamp(Ampere(0.0), trim_max)
}

/// Measurement probe (#135 step 6): lift a *live* meter answer to slightly above
/// the ceiling, so the box's response in the `max..max+cut` region — invisible
/// until now because the upper clamp never reports past `max` (#134 H1) — becomes
/// observable on real hardware. A pause/failsafe report (`base > max`) is never
/// masked, and `max_over` caps the lift below the pause margin so a probe cannot
/// reach the box's cut threshold ahead of measuring it.
pub fn probe_report(base: Ampere, over: Ampere, max: Ampere, max_over: Ampere) -> Ampere {
    if over.0 <= 0.0 || base.0 > max.0 {
        return base;
    }
    // Branch instead of `f32::min` (libm-gated in no_std, same reason as `ramp_step`).
    let over = if over.0 > max_over.0 { max_over } else { over };
    Ampere(max.0 + over.0)
}

/// V4 (#135 step 5): regulate the box's *grant* (`lb_current` from the CN28 LOG)
/// directly, instead of stacking offset/measured/trim. Built on the measured box
/// response (#135 step 6): the box sheds `floor(excess)` amps per ~6 s eval once the
/// meter reads >0.5 A over its limit, and ratchets up by the reported headroom.
///
/// - grant above target → report `max + err` (at least `max + 1` to clear the dead
///   zone, capped at `max + max_over` to stay clearly below the cut threshold, and
///   capped at `lb − (min_charge + 1)` so the box is never told to shed into its
///   6 A pilot floor — a ≥2 A step landing there drops the session (flag-day
///   staircase 2026-07-03). A fully capped-out shed holds instead, so target 6
///   deliberately settles at 7 A (within the ±1 A acceptance),
/// - grant at target (±1 A — grants are whole amps) → report exactly `max` (holds,
///   #57),
/// - grant below target → report the deficit as headroom (`max − (target − lb)`),
///   which also serves the start-grant: `lb = 0` reports `max − target`, so the box
///   opens at exactly the target.
pub fn lb_tracking_report(
    max: Ampere,
    target: Ampere,
    lb: Ampere,
    max_over: Ampere,
    min_charge: Ampere,
) -> Ampere {
    let err = lb.0 - target.0;
    if err >= 1.0 {
        let floor_cap = lb.0 - (min_charge.0 + 1.0);
        let over = if err > max_over.0 { max_over.0 } else { err };
        let over = if over > floor_cap { floor_cap } else { over };
        if over < 1.0 {
            return max;
        }
        Ampere(max.0 + over)
    } else if err <= -1.0 {
        Ampere(max.0 + err).clamp(Ampere(0.0), max)
    } else {
        max
    }
}

/// Relax the trim toward zero by `step` while CN28 feedback is stale (#119): a lost
/// feedback sample decays the correction back to the hardware-proven `offset + measured`
/// loop instead of holding a value the loop can no longer see. Never crosses zero.
pub fn trim_decay(trim: Ampere, step: Ampere) -> Ampere {
    // Decay only ever lowers the term; clamp to [0, trim] floors it at zero without
    // an `f32::max` (libm-gated in no_std, same reason as `ramp_step`).
    Ampere(trim.0 - step.0).clamp(Ampere(0.0), trim)
}

/// Everything the V4 per-tick decision needs, supplied by the firmware (which owns
/// the clock and judges staleness). Unlike [`ControlInputs`] there are no failsafe
/// *modes*: every failure direction under V4 is a fixed **pause**. A `FullCharge`
/// fallback (report 0) would make the box ratchet to its DIP maximum on the very
/// input failures that blind the controller — and the meterless unmanaged-box
/// baseline the mode existed for has no target publisher, so V4's cold-start pause
/// blocks it anyway (SPECS §9, #51/#52).
#[derive(Clone, Copy, Debug)]
pub struct GrantControlInputs {
    /// The box's DIP-set ceiling (`MAX_BOX_AMPERE`).
    pub max: Ampere,
    /// Below this target the car can't hold a charge, so we hard-pause (#23).
    pub min_charge: Ampere,
    /// Amps above the ceiling a pause reports so the box actually cuts (#57).
    pub pause_margin: Ampere,
    /// Cap on the over-report (see [`lb_tracking_report`]): the strongest measured
    /// shed rate while staying clearly below the box's cut threshold (#135 step 6).
    pub max_over: Ampere,
    /// Last commanded target — latched, `None` only before the first command (#59).
    pub target: Option<Ampere>,
    /// The box's current grant (`lb_current` from the CN28 LOG, ~5 s cadence).
    pub lb: Ampere,
    /// The car's live draw (max phase current from the box's MID metering, same
    /// ~5 s cadence). Below `min_charge` it pins the ramp report — see
    /// [`grant_tracking_current`].
    pub car: Ampere,
    /// CN28 feedback older than its window: the regulation is blind → pause.
    pub lb_stale: bool,
    /// The grid_power heartbeat (#136) stopped: the outside controller (HA/evcc)
    /// is gone and the latched target would charge forever → pause.
    pub grid_stale: bool,
    /// The enable gate (#60): `false` hard-pauses regardless of the target.
    pub enabled: bool,
}

/// The V4 per-tick decision: gate on enable, both staleness failsafes, the cold
/// start and the min-charge floor — all of which pause — then regulate the grant
/// via [`lb_tracking_report`]. While the car draws less than `min_charge` (the 6 A
/// pilot minimum) the report is pinned to `max − target + car` instead: per the
/// box's grant law (`lb ← car + max − reported`) that holds `lb = target` through
/// the whole contactor lag *and* the 0→6 A ramp. Reporting the ceiling any
/// earlier makes the box degrade or withdraw the grant — it only tolerates the
/// meter at the ceiling once the car draws properly (flag-day captures
/// 2026-07-03: cut at car 0 A, grant degraded 16→10 at car ~5 A).
pub fn grant_tracking_current(inputs: &GrantControlInputs) -> Ampere {
    let pause = pause_report(inputs.max, inputs.pause_margin);
    if !inputs.enabled || inputs.grid_stale || inputs.lb_stale {
        return pause;
    }
    let Some(target) = inputs.target else {
        return pause;
    };
    if target.clamp(Ampere(0.0), inputs.max).0 < inputs.min_charge.0 {
        return pause;
    }
    if inputs.car.0 < inputs.min_charge.0 {
        return Ampere(inputs.max.0 - target.0 + inputs.car.0).clamp(Ampere(0.0), inputs.max);
    }
    lb_tracking_report(
        inputs.max,
        target,
        inputs.lb,
        inputs.max_over,
        inputs.min_charge,
    )
}

/// Everything the per-poll decision needs, supplied by the firmware. Holding the last
/// good `target`/`measured`/`offset` and judging staleness is the firmware's job (it
/// owns the clock); this struct is the snapshot it hands in each poll.
#[derive(Clone, Copy, Debug)]
pub struct ControlInputs {
    /// The box's DIP-set ceiling (`MAX_BOX_AMPERE`).
    pub max: Ampere,
    /// Below this target the 3-phase loop can't hold, so we hard-pause (#23).
    pub min_charge: Ampere,
    /// Amps above the ceiling a pause reports so the box actually cuts (#57).
    pub pause_margin: Ampere,
    /// Last commanded target, or `None` before any command has ever landed — a cold
    /// start is not "charge full" (#59).
    pub target: Option<Ampere>,
    /// The current soft-ramped offset the firmware maintains via [`ramp_step`] (#24).
    pub offset: Ampere,
    /// Integral trim the firmware accumulates on ~5 s CN28 feedback to push the box
    /// below its natural ~9–15 A floor (#119), advanced via [`trim_step`] /
    /// [`trim_decay`]. Added on top of the closed loop; `0` is byte-identical to the
    /// pre-#119 path, so the proven 9–15 A behaviour is unchanged when it is idle.
    pub trim: Ampere,
    /// Latest live measured per-phase current that closes the loop (#22).
    pub measured: Ampere,
    /// The enable gate (#60): `false` hard-pauses regardless of the target.
    pub enabled: bool,
    /// Whether the last target is older than its staleness window (#7/#51).
    pub target_stale: bool,
    /// Whether the last measurement is older than its staleness window (#25).
    pub measured_stale: bool,
    /// Direction the target failsafe takes when `target_stale` (#51/#52).
    pub target_failsafe: FailsafeMode,
    /// Direction the measurement failsafe takes when `measured_stale` (#25/#51).
    pub measured_failsafe: FailsafeMode,
}

/// Combine two forced failsafe reports into the **safest** one (higher report = less
/// charge), so when several forced values engage the least-charge directive wins (#51).
fn safest(a: Option<Ampere>, b: Option<Ampere>) -> Option<Ampere> {
    match (a, b) {
        (Some(x), Some(y)) => Some(Ampere(x.0.max(y.0))),
        (only, None) | (None, only) => only,
    }
}

/// The per-phase household current to report this poll (SPECS §6/§9). Mirrors the
/// daemon's `Controller::reported_frame`, reduced to a pure function:
///
/// 1. The enable gate (#60), when off, forces a pause.
/// 2. A stale target / measurement engages its [`FailsafeMode`] (#51): `full_charge`
///    → 0, `pause` → above the ceiling (#57), `hold_last` → no override (the held
///    value flows through the loop). When several force a value, the safest wins.
/// 3. Cold start (no target yet) holds the box paused — never open at full charge
///    before the controller speaks (#59).
/// 4. Below the min-charge floor, hard-pause (#23).
/// 5. Otherwise close the loop: `clamp(offset + measured, 0, max)` (#23/#24).
pub fn reported_current(inputs: &ControlInputs) -> Ampere {
    let pause = pause_report(inputs.max, inputs.pause_margin);

    let mut forced: Option<Ampere> = None;
    if !inputs.enabled {
        forced = safest(forced, Some(pause));
    }
    if inputs.target_stale {
        forced = safest(forced, inputs.target_failsafe.forced_report(pause));
    }
    if inputs.measured_stale {
        forced = safest(forced, inputs.measured_failsafe.forced_report(pause));
    }
    if let Some(report) = forced {
        return report;
    }

    // Cold start: no command has landed yet — never open at full charge before the
    // controller speaks (#59). Independent of the min-charge floor below.
    let Some(target) = inputs.target else {
        return pause;
    };
    if target.clamp(Ampere(0.0), inputs.max).0 < inputs.min_charge.0 {
        return pause;
    }
    // #119: the integral trim rides on top of the offset — `clamp(offset + measured +
    // trim, 0, max)`. It is 0 outside the floor-seek, so this is a no-op for the
    // hardware-proven 9–15 A path.
    reported_from_offset(
        inputs.max,
        Ampere(inputs.offset.0 + inputs.trim.0),
        inputs.measured,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Ampere = Ampere(32.0);
    const MIN: Ampere = Ampere(6.0);
    const MARGIN: Ampere = Ampere(4.0);
    /// What a pause reports: the ceiling plus the margin, so the box actually cuts (#57).
    const PAUSE: Ampere = Ampere(36.0);
    const STEP: Ampere = Ampere(0.5);

    /// A baseline closed-loop snapshot: primed target, fresh inputs, gate open, both
    /// failsafes `full_charge`. Reports `offset 12 + measured 5 = 17`.
    fn base() -> ControlInputs {
        ControlInputs {
            max: MAX,
            min_charge: MIN,
            pause_margin: MARGIN,
            target: Some(Ampere(20.0)),
            offset: Ampere(12.0),
            trim: Ampere(0.0),
            measured: Ampere(5.0),
            enabled: true,
            target_stale: false,
            measured_stale: false,
            target_failsafe: FailsafeMode::FullCharge,
            measured_failsafe: FailsafeMode::FullCharge,
        }
    }

    // --- probe_report (#135 step 6) ---

    #[test]
    fn probe_lifts_a_live_report_just_over_the_ceiling() {
        // Base 12 is live modulation; over 1 → the box must read MAX + 1.
        assert_eq!(
            probe_report(Ampere(12.0), Ampere(1.0), MAX, Ampere(3.5)),
            Ampere(33.0)
        );
    }

    #[test]
    fn probe_lift_is_capped() {
        assert_eq!(
            probe_report(Ampere(12.0), Ampere(9.0), MAX, Ampere(3.5)),
            Ampere(35.5)
        );
    }

    #[test]
    fn probe_zero_is_inactive() {
        assert_eq!(
            probe_report(Ampere(12.0), Ampere(0.0), MAX, Ampere(3.5)),
            Ampere(12.0)
        );
    }

    #[test]
    fn probe_never_masks_a_pause() {
        // A forced pause (above the ceiling) must pass through untouched.
        assert_eq!(probe_report(PAUSE, Ampere(1.0), MAX, Ampere(3.5)), PAUSE);
    }

    #[test]
    fn probe_lifts_a_report_sitting_exactly_at_the_ceiling() {
        // The pinned case (#134): reported clamped to exactly MAX is live
        // modulation (#57) and is precisely where the probe is needed.
        assert_eq!(
            probe_report(MAX, Ampere(0.5), MAX, Ampere(3.5)),
            Ampere(32.5)
        );
    }

    // --- V4 per-tick decision (grant tracking + failsafes) ---

    /// A healthy V4 snapshot: enabled, fresh inputs, car drawing, grant at the
    /// 20 A target.
    fn grant_base() -> GrantControlInputs {
        GrantControlInputs {
            max: MAX,
            min_charge: MIN,
            pause_margin: MARGIN,
            max_over: Ampere(2.0),
            target: Some(Ampere(20.0)),
            lb: Ampere(20.0),
            car: Ampere(20.0),
            lb_stale: false,
            grid_stale: false,
            enabled: true,
        }
    }

    #[test]
    fn grant_tracking_holds_the_start_posture_while_the_car_is_idle() {
        // Flag-day capture 2026-07-03: the box granted, V4 snapped to the ceiling
        // hold, the box cut one eval later because the car (still in its 10–30 s
        // contactor lag) drew nothing. An idle car must keep the deficit report.
        let i = GrantControlInputs {
            car: Ampere(0.0),
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), Ampere(MAX.0 - 20.0));
    }

    #[test]
    fn grant_tracking_pins_the_grant_through_the_ramp() {
        // Mid-ramp (car past the contactor lag but below the 6 A pilot minimum):
        // report `max − target + car`, which per the box's grant law holds
        // `lb = target` — reporting the ceiling here made the box degrade the
        // grant (flag-day capture 2026-07-03, lb 16→10 at car ~5 A).
        let i = GrantControlInputs {
            car: Ampere(3.0),
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), Ampere(MAX.0 - 20.0 + 3.0));
    }

    #[test]
    fn grant_tracking_leaves_the_start_posture_once_the_car_draws() {
        let i = GrantControlInputs {
            car: Ampere(6.5),
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), MAX);
    }

    #[test]
    fn lb_tracking_never_sheds_into_the_pilot_floor() {
        // Flag-day staircase 2026-07-03 (t=522): target 6, lb 8, reported 18 → the
        // box cut instead of shedding 8 → 6. The over-report must cap at
        // `lb − (min_charge + 1)`: from 8 shed one amp (report 17), at 7 hold —
        // target 6 settles at 7 A, inside the ±1 A acceptance.
        assert_eq!(
            lb_tracking_report(MAX, Ampere(6.0), Ampere(8.0), Ampere(2.0), MIN),
            Ampere(MAX.0 + 1.0)
        );
        assert_eq!(
            lb_tracking_report(MAX, Ampere(6.0), Ampere(7.0), Ampere(2.0), MIN),
            MAX
        );
    }

    #[test]
    fn grant_tracking_delegates_to_the_lb_report() {
        assert_eq!(grant_tracking_current(&grant_base()), MAX);
        let over = GrantControlInputs {
            lb: Ampere(26.0),
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&over), Ampere(34.0));
    }

    #[test]
    fn grant_tracking_enable_false_pauses() {
        let i = GrantControlInputs {
            enabled: false,
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), PAUSE);
    }

    #[test]
    fn grant_tracking_stale_grid_heartbeat_pauses() {
        let i = GrantControlInputs {
            grid_stale: true,
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), PAUSE);
    }

    #[test]
    fn grant_tracking_stale_lb_feedback_pauses() {
        let i = GrantControlInputs {
            lb_stale: true,
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), PAUSE);
    }

    #[test]
    fn grant_tracking_cold_start_pauses() {
        let i = GrantControlInputs {
            target: None,
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), PAUSE);
    }

    #[test]
    fn grant_tracking_target_below_min_charge_pauses() {
        let i = GrantControlInputs {
            target: Some(Ampere(4.0)),
            ..grant_base()
        };
        assert_eq!(grant_tracking_current(&i), PAUSE);
    }

    // --- V4 direct grant tracking (#135 step 5) ---

    #[test]
    fn lb_tracking_holds_at_the_ceiling_when_the_grant_matches_the_target() {
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(20.0), Ampere(2.0), MIN),
            MAX
        );
    }

    #[test]
    fn lb_tracking_reports_the_excess_above_the_ceiling() {
        // Grant 6 A over → the box sheds proportionally, but the report is capped
        // at max_over so it can never approach the cut threshold.
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(26.0), Ampere(2.0), MIN),
            Ampere(34.0)
        );
    }

    #[test]
    fn lb_tracking_one_amp_over_clears_the_dead_zone() {
        // The box ignores ≤0.5 over (#135 step 6); a 1 A error must land at
        // max+1, inside the measured shed region.
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(21.0), Ampere(2.0), MIN),
            Ampere(33.0)
        );
    }

    #[test]
    fn lb_tracking_sub_amp_error_holds() {
        // CN28 grants are whole amps; anything below 1 A error is "at target".
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(20.5), Ampere(2.0), MIN),
            MAX
        );
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(19.5), Ampere(2.0), MIN),
            MAX
        );
    }

    #[test]
    fn lb_tracking_reports_the_deficit_as_headroom() {
        // Grant 6 A short → headroom 6, the box ratchets up to the target.
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(14.0), Ampere(2.0), MIN),
            Ampere(26.0)
        );
    }

    #[test]
    fn lb_tracking_start_headroom_equals_the_target() {
        // No grant yet: report max − target, so the start-grant is the target.
        assert_eq!(
            lb_tracking_report(MAX, Ampere(20.0), Ampere(0.0), Ampere(2.0), MIN),
            Ampere(12.0)
        );
    }

    #[test]
    fn lb_tracking_deficit_report_floors_at_zero() {
        assert_eq!(
            lb_tracking_report(MAX, Ampere(32.0), Ampere(0.0), Ampere(2.0), MIN),
            Ampere(0.0)
        );
    }

    // --- building blocks ---

    #[test]
    fn pause_report_is_the_ceiling_plus_the_margin() {
        assert_eq!(pause_report(MAX, MARGIN), PAUSE);
    }

    #[test]
    fn forced_report_maps_each_mode() {
        assert_eq!(
            FailsafeMode::FullCharge.forced_report(PAUSE),
            Some(Ampere(0.0))
        );
        assert_eq!(FailsafeMode::Pause.forced_report(PAUSE), Some(PAUSE));
        assert_eq!(FailsafeMode::HoldLast.forced_report(PAUSE), None);
    }

    #[test]
    fn reported_from_offset_adds_offset_and_measured() {
        assert_eq!(
            reported_from_offset(MAX, Ampere(12.0), Ampere(5.0)),
            Ampere(17.0)
        );
    }

    #[test]
    fn reported_from_offset_clamps_to_the_ceiling() {
        assert_eq!(reported_from_offset(MAX, Ampere(30.0), Ampere(10.0)), MAX);
    }

    #[test]
    fn reported_from_offset_clamps_up_to_zero() {
        // A negative offset must never report below zero.
        assert_eq!(
            reported_from_offset(MAX, Ampere(-5.0), Ampere(0.0)),
            Ampere(0.0)
        );
    }

    #[test]
    fn ramp_steps_up_by_at_most_the_max_step() {
        assert_eq!(ramp_step(Ampere(0.0), Ampere(10.0), STEP), Ampere(0.5));
    }

    #[test]
    fn ramp_steps_down_by_at_most_the_max_step() {
        assert_eq!(ramp_step(Ampere(10.0), Ampere(0.0), STEP), Ampere(9.5));
    }

    #[test]
    fn ramp_snaps_to_setpoint_within_one_step() {
        assert_eq!(ramp_step(Ampere(9.8), Ampere(10.0), STEP), Ampere(10.0));
        assert_eq!(ramp_step(Ampere(0.2), Ampere(0.0), STEP), Ampere(0.0));
    }

    #[test]
    fn ramp_holds_when_already_at_the_setpoint() {
        assert_eq!(ramp_step(Ampere(5.0), Ampere(5.0), STEP), Ampere(5.0));
    }

    // --- #119 integral trim (advanced once per fresh CN28 sample) ---

    #[test]
    fn trim_grows_when_the_box_charges_above_target() {
        // actual 10 A, target 6 A → err 4 A, ki 0.5 → +2 A onto the trim.
        assert_eq!(
            trim_step(Ampere(0.0), Ampere(10.0), Ampere(6.0), 0.5, Ampere(8.0)),
            Ampere(2.0)
        );
    }

    #[test]
    fn trim_shrinks_when_the_box_charges_below_target() {
        // actual 5 A, target 6 A → err -1 A, ki 0.5 → -0.5 A off the held trim.
        assert_eq!(
            trim_step(Ampere(2.0), Ampere(5.0), Ampere(6.0), 0.5, Ampere(8.0)),
            Ampere(1.5)
        );
    }

    #[test]
    fn trim_saturates_at_trim_max_anti_windup() {
        assert_eq!(
            trim_step(Ampere(7.9), Ampere(100.0), Ampere(6.0), 0.5, Ampere(8.0)),
            Ampere(8.0)
        );
    }

    #[test]
    fn trim_never_goes_negative() {
        // A big undershoot can't drive the term below zero — it only ever cuts charge.
        assert_eq!(
            trim_step(Ampere(0.2), Ampere(0.0), Ampere(6.0), 0.5, Ampere(8.0)),
            Ampere(0.0)
        );
    }

    #[test]
    fn trim_decays_toward_zero_while_stale() {
        assert_eq!(trim_decay(Ampere(5.0), Ampere(1.0)), Ampere(4.0));
    }

    #[test]
    fn trim_decay_stops_at_zero() {
        assert_eq!(trim_decay(Ampere(0.5), Ampere(1.0)), Ampere(0.0));
    }

    #[test]
    fn trim_decay_holds_at_zero() {
        assert_eq!(trim_decay(Ampere(0.0), Ampere(1.0)), Ampere(0.0));
    }

    // --- the decision (mirrors charge/tests/control.rs, pure) ---

    #[test]
    fn closed_loop_reports_offset_plus_measured() {
        assert_eq!(reported_current(&base()), Ampere(17.0));
    }

    #[test]
    fn closed_loop_adds_the_trim_on_top() {
        // base offset 12 + measured 5 = 17, plus a trim of 3 = 20 (below the ceiling).
        let i = ControlInputs {
            trim: Ampere(3.0),
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(20.0));
    }

    #[test]
    fn a_large_trim_is_clamped_into_the_ceiling() {
        let i = ControlInputs {
            trim: Ampere(100.0),
            ..base()
        };
        assert_eq!(reported_current(&i), MAX);
    }

    #[test]
    fn trim_does_not_leak_into_a_pause() {
        // The trim rides only on the live closed loop; a cold-start pause ignores it,
        // so a stale/absent target can never be nudged toward charging by a stale trim.
        let i = ControlInputs {
            target: None,
            trim: Ampere(5.0),
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn report_clamps_to_the_ceiling() {
        let i = ControlInputs {
            offset: Ampere(30.0),
            measured: Ampere(10.0),
            ..base()
        };
        assert_eq!(reported_current(&i), MAX);
    }

    #[test]
    fn cold_start_pauses_until_the_first_target() {
        // #59: before any command, never open the box at full charge.
        let i = ControlInputs {
            target: None,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn explicit_target_zero_pauses() {
        let i = ControlInputs {
            target: Some(Ampere(0.0)),
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn target_below_min_charge_pauses_regardless_of_offset_and_measurement() {
        let i = ControlInputs {
            target: Some(Ampere(4.0)),
            offset: Ampere(28.0),
            measured: Ampere(30.0),
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn stale_target_full_charge_reports_zero() {
        let i = ControlInputs {
            target_stale: true,
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(0.0));
    }

    #[test]
    fn stale_target_pause_reports_above_the_ceiling() {
        let i = ControlInputs {
            target_stale: true,
            target_failsafe: FailsafeMode::Pause,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn stale_target_hold_last_keeps_the_closed_loop() {
        let i = ControlInputs {
            target_stale: true,
            target_failsafe: FailsafeMode::HoldLast,
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(17.0)); // held target → offset 12 + measured 5
    }

    #[test]
    fn stale_measurement_full_charge_reports_zero() {
        let i = ControlInputs {
            measured_stale: true,
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(0.0));
    }

    #[test]
    fn stale_measurement_pause_reports_above_the_ceiling() {
        let i = ControlInputs {
            measured_stale: true,
            measured_failsafe: FailsafeMode::Pause,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn stale_measurement_hold_last_keeps_the_held_measurement() {
        let i = ControlInputs {
            measured_stale: true,
            measured_failsafe: FailsafeMode::HoldLast,
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(17.0)); // offset 12 + held measured 5
    }

    #[test]
    fn safest_mode_wins_when_both_failsafes_are_active() {
        // target=pause beats measured=full_charge (least charge wins).
        let i = ControlInputs {
            target_stale: true,
            measured_stale: true,
            target_failsafe: FailsafeMode::Pause,
            measured_failsafe: FailsafeMode::FullCharge,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn enable_false_pauses_an_active_charge() {
        let i = ControlInputs {
            enabled: false,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn enable_false_overrides_a_full_charge_target_failsafe() {
        // An explicit off must win even over a stale-target full-charge fallback (#60).
        let i = ControlInputs {
            enabled: false,
            target_stale: true,
            target_failsafe: FailsafeMode::FullCharge,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn cold_start_hold_last_still_pauses_with_no_target_ever() {
        // hold_last has nothing to hold on a cold start → keep pausing, never full charge.
        let i = ControlInputs {
            target: None,
            target_stale: true,
            target_failsafe: FailsafeMode::HoldLast,
            ..base()
        };
        assert_eq!(reported_current(&i), PAUSE);
    }

    #[test]
    fn cold_start_full_charge_failsafe_opens_after_the_grace_window() {
        // Unmanaged-box baseline: an un-commanded box falls back to full charge once stale.
        let i = ControlInputs {
            target: None,
            target_stale: true,
            target_failsafe: FailsafeMode::FullCharge,
            ..base()
        };
        assert_eq!(reported_current(&i), Ampere(0.0));
    }
}
