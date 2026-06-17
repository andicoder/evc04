//! Integration tests for the public control-math API (SPECS.md §6).

use evc04_charge::{reported_household, Ampere};

const MAX: Ampere = Ampere(32.0);

#[test]
fn reports_full_headroom_when_target_equals_max() {
    // target = max → report 0 → box grants maximum charge.
    assert_eq!(reported_household(MAX, MAX), Ampere(0.0));
}

#[test]
fn reports_full_ceiling_when_target_is_zero() {
    // target = 0 → report the whole ceiling → no headroom → charging pauses.
    assert_eq!(reported_household(MAX, Ampere(0.0)), MAX);
}

#[test]
fn reports_complement_for_partial_target() {
    assert_eq!(reported_household(MAX, Ampere(10.0)), Ampere(22.0));
}

#[test]
fn clamps_target_above_max_to_zero_report() {
    assert_eq!(reported_household(MAX, Ampere(100.0)), Ampere(0.0));
}

#[test]
fn clamps_negative_target_to_full_ceiling() {
    assert_eq!(reported_household(MAX, Ampere(-5.0)), MAX);
}
