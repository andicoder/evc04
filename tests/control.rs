//! Integration tests for the control seam (SPECS.md §6/§9): the MQTT target
//! current drives the household current the meter slave serves, and a stale target
//! falls back to `FAILSAFE_TARGET_A`.

use evc04_charge::control;
use evc04_charge::mqtt::TargetError;
use evc04_charge::reported_current;
use evc04_charge::slave::{serve_connection, PollMatch};
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

const FUSE: f32 = 32.0;
/// Fail toward no charge unless a test needs a distinguishable failsafe value.
const NO_CHARGE: f32 = 0.0;
const STALE_AFTER: Duration = Duration::from_secs(5);

// SPECS.md §4 verified poll frame.
const SPEC_POLL: [u8; 8] = [0x01, 0x03, 0x50, 0x0c, 0x00, 0x06, 0x14, 0xcb];

// SPECS.md §5 verified 0 A response (report 0 → box grants maximum charge).
const ZERO_AMP_RESPONSE: [u8; 17] = [
    0x01, 0x03, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x93, 0x70,
];

// SPECS.md §5 verified 16 A response (report 16 → 16 A headroom left for the car).
const SIXTEEN_AMP_RESPONSE: [u8; 17] = [
    0x01, 0x03, 0x0c, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x97,
    0xae,
];

#[test]
fn target_zero_reports_full_fuse_per_phase() {
    // target = 0 → report the whole fuse → no headroom → charging pauses.
    let (_sink, view) = control::channel(FUSE, NO_CHARGE, STALE_AFTER, 0.0);
    assert_eq!(view.currents(), [FUSE; 3]);
}

#[test]
fn target_at_fuse_limit_reports_zero() {
    // target = fuse_limit → report 0 → box grants maximum charge.
    let (sink, view) = control::channel(FUSE, NO_CHARGE, STALE_AFTER, 0.0);
    sink.apply(Ok(FUSE));
    assert_eq!(view.currents(), [0.0; 3]);
}

#[test]
fn rejected_target_holds_the_last_valid_value() {
    // A malformed command must not disturb the effective target (docs/mqtt.md).
    let (sink, view) = control::channel(FUSE, NO_CHARGE, STALE_AFTER, 0.0);
    sink.apply(Ok(10.0));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(view.currents(), [22.0; 3]);
}

#[tokio::test]
async fn served_frame_tracks_the_live_target() {
    // The slave recomputes from the live target, so a new command is reflected in
    // the very next served poll without any extra signalling.
    let (sink, view) = control::channel(FUSE, NO_CHARGE, STALE_AFTER, 16.0);
    let serving = view.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.currents(),
    ));

    // target 16 on a 32 A fuse → report 16 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    // Raise the target to the fuse limit → report 0 A on the next poll.
    sink.apply(Ok(FUSE));
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    serve.abort();
}

#[tokio::test(start_paused = true)]
async fn stale_target_falls_back_to_failsafe() {
    // Distinguishable failsafe (8 A) so the fallback is unmistakable from the
    // commanded 20 A. SPECS §9: silence faults the box, so we keep answering with
    // the safe target rather than going quiet.
    let failsafe = 8.0;
    let (sink, view) = control::channel(FUSE, failsafe, STALE_AFTER, 20.0);

    assert_eq!(view.currents(), [reported_current(FUSE, 20.0); 3]);
    assert!(!view.failsafe_active());

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;

    assert!(view.failsafe_active());
    assert_eq!(view.currents(), [reported_current(FUSE, failsafe); 3]);
    let _ = sink;
}

#[tokio::test(start_paused = true)]
async fn fresh_target_resumes_normal_control() {
    let failsafe = 8.0;
    let (sink, view) = control::channel(FUSE, failsafe, STALE_AFTER, 20.0);

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    assert!(view.failsafe_active());

    sink.apply(Ok(16.0));
    assert!(!view.failsafe_active());
    assert_eq!(view.currents(), [reported_current(FUSE, 16.0); 3]);
    assert_eq!(view.effective_target_a(), 16.0);
}

#[tokio::test(start_paused = true)]
async fn served_frame_uses_failsafe_when_target_goes_stale() {
    // failsafe 16 A on a 32 A fuse → report 16 A once stale; initial 32 A → 0 A.
    let (_sink, view) = control::channel(FUSE, 16.0, STALE_AFTER, FUSE);
    let serving = view.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.currents(),
    ));

    // Still fresh: initial target 32 A → report 0 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    // Past the staleness window → serve the failsafe-derived frame.
    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    serve.abort();
}
