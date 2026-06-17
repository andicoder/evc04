//! Integration tests for the public closed-loop control math (SPECS.md §6, #23).
//!
//! `reported = clamp(offset + measured, 0, max)` with `offset = max − target`, so the
//! reported "household" current rises with the car's real draw and the box modulates
//! toward `target`. Below `min_charge` the 3-phase floor can't hold, so we pause.

use evc04_charge::{ramp_step, reported_household, Ampere};

const MAX: Ampere = Ampere(32.0);
const MIN: Ampere = Ampere(6.0);
const STEP: Ampere = Ampere(0.5);

#[test]
fn offset_zero_reports_just_the_measured_current() {
    // target = max → offset 0 → the box holds the total at the ceiling: reported = measured.
    assert_eq!(
        reported_household(MAX, MAX, Ampere(10.0), MIN),
        Ampere(10.0)
    );
}

#[test]
fn reports_offset_plus_measured_for_a_partial_target() {
    // target 20 on a 32 A ceiling → offset 12; measured 5 → reported 17.
    assert_eq!(
        reported_household(MAX, Ampere(20.0), Ampere(5.0), MIN),
        Ampere(17.0)
    );
}

#[test]
fn measured_current_raises_the_report_so_the_box_modulates() {
    // Same target, more measured draw → higher report → the box backs the car off.
    let low = reported_household(MAX, Ampere(20.0), Ampere(2.0), MIN);
    let high = reported_household(MAX, Ampere(20.0), Ampere(8.0), MIN);
    assert!(high.0 > low.0, "report must rise with measured current");
}

#[test]
fn clamps_the_report_to_the_ceiling() {
    // offset 22 + measured 20 = 42 → clamped to the ceiling (zero headroom).
    assert_eq!(
        reported_household(MAX, Ampere(10.0), Ampere(20.0), MIN),
        MAX
    );
}

#[test]
fn target_below_min_charge_pauses_regardless_of_measured() {
    // Below the 3-phase floor the loop collapses, so we hard-pause (report the ceiling)
    // no matter what the measurement says.
    assert_eq!(reported_household(MAX, Ampere(4.0), Ampere(0.0), MIN), MAX);
    assert_eq!(reported_household(MAX, Ampere(4.0), Ampere(30.0), MIN), MAX);
}

#[test]
fn target_at_min_charge_still_modulates() {
    // The cutoff is strict (< min): a target exactly at the floor charges.
    assert_eq!(
        reported_household(MAX, MIN, Ampere(0.0), MIN),
        MAX - MIN // offset 26, measured 0
    );
}

#[test]
fn clamps_target_above_the_ceiling_to_offset_zero() {
    // target over the ceiling → offset 0 → reported = measured.
    assert_eq!(
        reported_household(MAX, Ampere(100.0), Ampere(7.0), MIN),
        Ampere(7.0)
    );
}

// --- Soft-ramp rate limiter (#24): move the offset toward its setpoint by a bounded step. ---

#[test]
fn ramp_steps_up_by_at_most_the_max_step() {
    // Far below the setpoint → advance by exactly one step, not the whole gap.
    assert_eq!(ramp_step(Ampere(0.0), Ampere(10.0), STEP), Ampere(0.5));
}

#[test]
fn ramp_steps_down_by_at_most_the_max_step() {
    assert_eq!(ramp_step(Ampere(10.0), Ampere(0.0), STEP), Ampere(9.5));
}

#[test]
fn ramp_snaps_to_setpoint_within_one_step() {
    // Closer than a step → land exactly on the setpoint, never overshoot.
    assert_eq!(ramp_step(Ampere(9.8), Ampere(10.0), STEP), Ampere(10.0));
    assert_eq!(ramp_step(Ampere(0.2), Ampere(0.0), STEP), Ampere(0.0));
}

#[test]
fn ramp_holds_when_already_at_the_setpoint() {
    assert_eq!(ramp_step(Ampere(5.0), Ampere(5.0), STEP), Ampere(5.0));
}
