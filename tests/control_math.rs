//! Integration tests for the public control-math API (SPECS.md §6).

use evc04_charge::reported_current;

const FUSE: f32 = 32.0;

#[test]
fn reports_full_headroom_when_target_equals_fuse_limit() {
    // target = fuse_limit → report 0 → box grants maximum charge.
    assert_eq!(reported_current(FUSE, FUSE), 0.0);
}

#[test]
fn reports_full_fuse_when_target_is_zero() {
    // target = 0 → report the whole fuse → no headroom → charging pauses.
    assert_eq!(reported_current(FUSE, 0.0), FUSE);
}

#[test]
fn reports_complement_for_partial_target() {
    assert_eq!(reported_current(FUSE, 10.0), 22.0);
}

#[test]
fn clamps_target_above_fuse_limit_to_zero_report() {
    assert_eq!(reported_current(FUSE, 100.0), 0.0);
}

#[test]
fn clamps_negative_target_to_full_fuse() {
    assert_eq!(reported_current(FUSE, -5.0), FUSE);
}
