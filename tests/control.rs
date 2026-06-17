//! Integration tests for the control seam (SPECS.md §6/§9): the MQTT target
//! current drives the household current the meter slave serves, and a stale or
//! absent target falls back to full charge (report 0 A — the meterless box default).

use evc04_charge::mqtt::TargetError;
use evc04_charge::slave::{serve_connection, PollMatch};
use evc04_charge::{control, reported_household, Ampere};
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

const MAX: Ampere = Ampere(32.0);
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
fn cold_start_reports_full_charge() {
    // Before any command arrives the box must charge like a meterless box: report
    // 0 A household → maximum charge. "Never worse than no tool" (SPECS §9).
    let (_sink, view) = control::channel(MAX, STALE_AFTER);
    assert_eq!(view.reported_frame(), [0.0; 3]);
}

#[test]
fn explicit_target_zero_reports_full_ceiling_per_phase() {
    // An explicit target = 0 → report the whole ceiling → no headroom → pause. This is
    // a command ("don't charge now"), distinct from the absent-command failsafe.
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(0.0));
    assert_eq!(view.reported_frame(), [MAX.0; 3]);
}

#[test]
fn target_at_max_current_reports_zero() {
    // target = max_current → report 0 → box grants maximum charge.
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(MAX.0));
    assert_eq!(view.reported_frame(), [0.0; 3]);
}

#[test]
fn rejected_target_holds_the_last_valid_value() {
    // A malformed command must not disturb the effective target (docs/mqtt.md).
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(10.0));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(view.reported_frame(), [22.0; 3]);
}

#[tokio::test]
async fn served_frame_tracks_the_live_target() {
    // The slave recomputes from the live target, so a new command is reflected in
    // the very next served poll without any extra signalling.
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(16.0));
    let serving = view.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.reported_frame(),
    ));

    // target 16 on a 32 A ceiling → report 16 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    // Raise the target to the ceiling → report 0 A on the next poll.
    sink.apply(Ok(MAX.0));
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    serve.abort();
}

#[tokio::test(start_paused = true)]
async fn stale_target_falls_back_to_full_charge() {
    // SPECS §9: silence faults the box, so we keep answering — but with full charge
    // (report 0 A), the meterless-box default, not a pause.
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(20.0));

    assert_eq!(
        view.reported_frame(),
        [reported_household(MAX, Ampere(20.0)).0; 3]
    );
    assert!(!view.failsafe_active());

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;

    assert!(view.failsafe_active());
    assert_eq!(view.reported_frame(), [0.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn fresh_target_resumes_normal_control() {
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(20.0));

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    assert!(view.failsafe_active());

    sink.apply(Ok(16.0));
    assert!(!view.failsafe_active());
    assert_eq!(
        view.reported_frame(),
        [reported_household(MAX, Ampere(16.0)).0; 3]
    );
    assert_eq!(view.effective_target(), Ampere(16.0));
}

#[tokio::test(start_paused = true)]
async fn served_frame_uses_full_charge_when_target_goes_stale() {
    // Fresh target 16 A → report 16 A; once stale → report 0 A (full charge).
    let (sink, view) = control::channel(MAX, STALE_AFTER);
    sink.apply(Ok(16.0));
    let serving = view.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.reported_frame(),
    ));

    // Still fresh: target 16 A → report 16 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    // Past the staleness window → fall back to full charge (report 0 A).
    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    serve.abort();
}

// --- Measurement input (#22): the second inbound channel that closes the loop. ---

#[test]
fn measurement_channel_holds_its_initial_value() {
    let (_sink, meas) = control::measurement_channel(Ampere(0.0));
    assert_eq!(meas.measured(), Ampere(0.0));
}

#[test]
fn measurement_channel_adopts_a_fresh_value() {
    let (sink, meas) = control::measurement_channel(Ampere(0.0));
    sink.apply(Ok(9.1));
    assert_eq!(meas.measured(), Ampere(9.1));
}

#[test]
fn rejected_measurement_holds_the_last_value() {
    // A malformed measured payload must not disturb the held value (docs/mqtt.md),
    // because serving offset + a corrupt measurement is a safety problem (#25).
    let (sink, meas) = control::measurement_channel(Ampere(0.0));
    sink.apply(Ok(9.1));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(meas.measured(), Ampere(9.1));
}

#[tokio::test(start_paused = true)]
async fn measurement_age_advances_until_a_fresh_value_resets_it() {
    let (sink, meas) = control::measurement_channel(Ampere(0.0));
    let start = Instant::now();

    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(meas.age(), start.elapsed());
    assert_eq!(meas.age(), Duration::from_secs(3));

    sink.apply(Ok(7.0));
    assert_eq!(meas.age(), Duration::from_secs(0));
}
