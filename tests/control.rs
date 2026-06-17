//! Integration tests for the control seam (SPECS.md §6/§9): the MQTT target
//! current drives the household current the meter slave serves, and a stale or
//! absent target falls back to full charge (report 0 A — the meterless box default).

use evc04_charge::control::{Controller, MeasurementSink, TargetSink};
use evc04_charge::mqtt::TargetError;
use evc04_charge::slave::{serve_connection, PollMatch};
use evc04_charge::{control, Ampere};
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

const MAX: Ampere = Ampere(32.0);
const MIN: Ampere = Ampere(6.0);
const STALE_AFTER: Duration = Duration::from_secs(5);
const MEAS_STALE: Duration = Duration::from_secs(10);

/// A `Controller` wired to fresh target + measurement channels, the measurement held at
/// 0 A (so a test that sets only the target sees the open-loop offset).
fn controller() -> (TargetSink, MeasurementSink, Controller) {
    let (target_sink, target_view) = control::channel(MAX, STALE_AFTER);
    let (measured_sink, measured_view) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    let ctrl = Controller::new(target_view, measured_view, MIN);
    (target_sink, measured_sink, ctrl)
}

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
    let (_sink, _msink, ctrl) = controller();
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[test]
fn explicit_target_zero_reports_full_ceiling_per_phase() {
    // An explicit target = 0 is below the min-charge floor → report the whole ceiling →
    // no headroom → pause. A command ("don't charge now"), distinct from the failsafe.
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(0.0));
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]);
}

#[test]
fn target_at_max_current_reports_zero() {
    // target = max_current, no measured draw yet → offset 0 → report 0 → maximum charge.
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(MAX.0));
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[test]
fn rejected_target_holds_the_last_valid_value() {
    // A malformed command must not disturb the effective target (docs/mqtt.md).
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(10.0));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(ctrl.reported_frame(), [22.0; 3]); // offset 22, measured 0
}

#[test]
fn served_report_closes_the_loop_on_the_measurement() {
    // The defining #23 behaviour: a live measurement raises the report above the bare
    // offset so the box modulates instead of cliffing on/off.
    let (sink, msink, ctrl) = controller();
    sink.apply(Ok(20.0)); // offset 12
    msink.apply(Ok(5.0));
    assert_eq!(ctrl.reported_frame(), [17.0; 3]); // 12 + 5
}

#[test]
fn target_below_min_charge_pauses_regardless_of_measurement() {
    // Below the 3-phase floor the loop can't hold; pause no matter the measured current.
    let (sink, msink, ctrl) = controller();
    sink.apply(Ok(4.0));
    msink.apply(Ok(30.0));
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn stale_measurement_falls_back_to_full_charge() {
    // #25: serving offset + a stale measurement would hold the box at the wrong current,
    // so once the measured input goes stale we revert to full charge (report 0 A), the
    // meterless-box default — never a pause (CLAUDE.md failsafe direction).
    let (sink, msink, ctrl) = controller();
    sink.apply(Ok(20.0)); // offset 12
    msink.apply(Ok(5.0));

    assert_eq!(ctrl.reported_frame(), [17.0; 3]);
    assert!(!ctrl.measurement_failsafe_active());

    tokio::time::advance(MEAS_STALE + Duration::from_millis(1)).await;
    sink.apply(Ok(20.0)); // keep the target fresh so only the measurement is stale

    assert!(!ctrl.failsafe_active());
    assert!(ctrl.measurement_failsafe_active());
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn fresh_measurement_resumes_the_closed_loop() {
    let (sink, msink, ctrl) = controller();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_millis(1)).await;
    assert!(ctrl.measurement_failsafe_active());

    // Republish target (kept fresh) + a fresh measurement → the closed loop resumes.
    sink.apply(Ok(20.0));
    msink.apply(Ok(5.0));
    assert!(!ctrl.measurement_failsafe_active());
    assert_eq!(ctrl.reported_frame(), [17.0; 3]);
}

#[tokio::test]
async fn served_frame_tracks_the_live_target() {
    // The slave recomputes from the live target, so a new command is reflected in
    // the very next served poll without any extra signalling.
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(16.0));
    let serving = ctrl.clone();
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
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(20.0));

    assert_eq!(ctrl.reported_frame(), [12.0; 3]); // offset 12, measured 0
    assert!(!ctrl.failsafe_active());

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;

    assert!(ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn fresh_target_resumes_normal_control() {
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(20.0));

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    assert!(ctrl.failsafe_active());

    sink.apply(Ok(16.0));
    assert!(!ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [16.0; 3]); // offset 16, measured 0
    assert_eq!(ctrl.effective_target(), Ampere(16.0));
}

#[tokio::test(start_paused = true)]
async fn served_frame_uses_full_charge_when_target_goes_stale() {
    // Fresh target 16 A → report 16 A; once stale → report 0 A (full charge).
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(16.0));
    let serving = ctrl.clone();
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
    let (_sink, meas) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    assert_eq!(meas.measured(), Ampere(0.0));
}

#[test]
fn measurement_channel_adopts_a_fresh_value() {
    let (sink, meas) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    sink.apply(Ok(9.1));
    assert_eq!(meas.measured(), Ampere(9.1));
}

#[test]
fn rejected_measurement_holds_the_last_value() {
    // A malformed measured payload must not disturb the held value (docs/mqtt.md),
    // because serving offset + a corrupt measurement is a safety problem (#25).
    let (sink, meas) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    sink.apply(Ok(9.1));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(meas.measured(), Ampere(9.1));
}

#[tokio::test(start_paused = true)]
async fn measurement_age_advances_until_a_fresh_value_resets_it() {
    let (sink, meas) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    let start = Instant::now();

    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(meas.age(), start.elapsed());
    assert_eq!(meas.age(), Duration::from_secs(3));

    sink.apply(Ok(7.0));
    assert_eq!(meas.age(), Duration::from_secs(0));
}
