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
    reported_from_offset(inputs.max, inputs.offset, inputs.measured)
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
            measured: Ampere(5.0),
            enabled: true,
            target_stale: false,
            measured_stale: false,
            target_failsafe: FailsafeMode::FullCharge,
            measured_failsafe: FailsafeMode::FullCharge,
        }
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

    // --- the decision (mirrors charge/tests/control.rs, pure) ---

    #[test]
    fn closed_loop_reports_offset_plus_measured() {
        assert_eq!(reported_current(&base()), Ampere(17.0));
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
