//! Integration tests for the control seam (SPECS.md §6/§9): the slave serves
//! `clamp(soft_ramped_offset + measured)`, with the min-charge cutoff and the
//! target/measurement staleness failsafes both falling back to full charge.

use evc04_charge::config::FailsafeMode;
use evc04_charge::control::{Controller, MeasurementSink, TargetSink};
use evc04_charge::mqtt::TargetError;
use evc04_charge::slave::{serve_connection, PollMatch};
use evc04_charge::{control, Ampere};
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::Instant;

const MAX: Ampere = Ampere(32.0);
const MIN: Ampere = Ampere(6.0);
const STALE_AFTER: Duration = Duration::from_secs(5);
const MEAS_STALE: Duration = Duration::from_secs(10);

/// A `Controller` wired to fresh target, measurement, and offset channels. The offset is
/// the soft-ramped value the driver normally produces (#24); tests set it directly via
/// the returned sender so the served value is deterministic. Measurement starts at 0 A.
fn controller() -> (
    TargetSink,
    MeasurementSink,
    watch::Sender<Ampere>,
    Controller,
) {
    // The historical default: both staleness failsafes fall back to full charge.
    controller_with(FailsafeMode::FullCharge, FailsafeMode::FullCharge)
}

/// Like [`controller`] but with explicit per-channel failsafe modes (#51).
fn controller_with(
    target_failsafe: FailsafeMode,
    measured_failsafe: FailsafeMode,
) -> (
    TargetSink,
    MeasurementSink,
    watch::Sender<Ampere>,
    Controller,
) {
    let (target_sink, target_view) = control::channel(MAX, STALE_AFTER);
    let (measured_sink, measured_view) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    let (offset_tx, offset_view) = control::offset_channel(Ampere(0.0));
    let ctrl = Controller::new(
        target_view,
        measured_view,
        offset_view,
        MIN,
        target_failsafe,
        measured_failsafe,
    );
    (target_sink, measured_sink, offset_tx, ctrl)
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
    // Before any command arrives the offset sits at 0 and the box charges like a meterless
    // box: report 0 A household → maximum charge. "Never worse than no tool" (SPECS §9).
    let (_sink, _msink, _offset, ctrl) = controller();
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[test]
fn explicit_target_zero_pauses() {
    // An explicit target = 0 is below the min-charge floor → report the whole ceiling →
    // no headroom → pause. A command ("don't charge now"), distinct from the failsafe, and
    // independent of the offset.
    let (sink, _msink, offset, ctrl) = controller();
    offset.send(Ampere(20.0)).unwrap();
    sink.apply(Ok(0.0));
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]);
}

#[test]
fn served_report_is_offset_plus_measured() {
    // The closed-loop answer: a live measurement raises the report above the bare offset so
    // the box modulates instead of cliffing on/off (#23), reading the ramped offset (#24).
    let (sink, msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));
    assert_eq!(ctrl.reported_frame(), [17.0; 3]); // 12 + 5
}

#[test]
fn report_clamps_to_the_ceiling() {
    // offset 30 + measured 10 = 40 → clamped to the ceiling (zero headroom).
    let (sink, msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0));
    offset.send(Ampere(30.0)).unwrap();
    msink.apply(Ok(10.0));
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]);
}

#[test]
fn rejected_target_holds_the_last_valid_value() {
    // A malformed command must not disturb the effective target (docs/mqtt.md).
    let (sink, _msink, _offset, ctrl) = controller();
    sink.apply(Ok(10.0));
    sink.apply(Err(TargetError::Malformed));
    assert_eq!(ctrl.effective_target(), Ampere(10.0));
}

#[test]
fn target_below_min_charge_pauses_regardless_of_offset_and_measurement() {
    // Below the 3-phase floor the loop can't hold; pause no matter the offset/measurement.
    let (sink, msink, offset, ctrl) = controller();
    sink.apply(Ok(4.0));
    offset.send(Ampere(28.0)).unwrap();
    msink.apply(Ok(30.0));
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn stale_measurement_falls_back_to_full_charge() {
    // #25: serving offset + a stale measurement would hold the box at the wrong current,
    // so once the measured input goes stale we revert to full charge (report 0 A), the
    // meterless-box default — never a pause (CLAUDE.md failsafe direction).
    let (sink, msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
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
    let (sink, msink, offset, ctrl) = controller();
    offset.send(Ampere(12.0)).unwrap();
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
async fn served_frame_tracks_the_live_offset() {
    // The slave recomputes from the live offset, so the ramped value is reflected in the
    // very next served poll without any extra signalling.
    let (sink, _msink, offset, ctrl) = controller();
    sink.apply(Ok(16.0));
    offset.send(Ampere(16.0)).unwrap();
    let serving = ctrl.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.reported_frame(),
    ));

    // offset 16, no measured draw → report 16 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    // Ramp the offset to 0 → report 0 A on the next poll.
    offset.send(Ampere(0.0)).unwrap();
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    serve.abort();
}

#[tokio::test(start_paused = true)]
async fn stale_target_falls_back_to_full_charge() {
    // SPECS §9: silence faults the box, so we keep answering — but with full charge
    // (report 0 A), the meterless-box default, not a pause.
    let (sink, _msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();

    assert_eq!(ctrl.reported_frame(), [12.0; 3]); // offset 12, measured 0
    assert!(!ctrl.failsafe_active());

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;

    assert!(ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [0.0; 3]);
}

#[tokio::test(start_paused = true)]
async fn fresh_target_resumes_normal_control() {
    let (sink, _msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0));

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    assert!(ctrl.failsafe_active());

    sink.apply(Ok(16.0));
    offset.send(Ampere(16.0)).unwrap();
    assert!(!ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [16.0; 3]); // offset 16, measured 0
    assert_eq!(ctrl.effective_target(), Ampere(16.0));
}

#[tokio::test(start_paused = true)]
async fn served_frame_uses_full_charge_when_target_goes_stale() {
    // Fresh target, offset 16 → report 16 A; once stale → report 0 A (full charge).
    let (sink, _msink, offset, ctrl) = controller();
    sink.apply(Ok(16.0));
    offset.send(Ampere(16.0)).unwrap();
    let serving = ctrl.clone();
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        move || serving.reported_frame(),
    ));

    // Still fresh: offset 16 → report 16 A.
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

// --- Configurable failsafe direction (#51): full_charge | hold_last | pause ---

#[tokio::test(start_paused = true)]
async fn target_stale_pause_reports_the_ceiling() {
    // With TARGET_FAILSAFE=pause a stale target must STOP charging (report the ceiling) —
    // the safe direction for an evcc-managed box (a stale evcc pause stays a pause), not the
    // full-charge default that would start charging at the worst time.
    let (sink, msink, offset, ctrl) =
        controller_with(FailsafeMode::Pause, FailsafeMode::FullCharge);
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));
    assert_eq!(ctrl.reported_frame(), [17.0; 3]); // fresh → closed loop

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    msink.apply(Ok(5.0)); // keep the measurement fresh; only the target is stale
    assert!(ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]); // pause
}

#[tokio::test(start_paused = true)]
async fn target_stale_hold_last_keeps_the_last_command() {
    // hold_last keeps serving the last commanded value through the closed loop.
    let (sink, msink, offset, ctrl) =
        controller_with(FailsafeMode::HoldLast, FailsafeMode::FullCharge);
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));

    tokio::time::advance(STALE_AFTER + Duration::from_millis(1)).await;
    msink.apply(Ok(5.0));
    assert!(ctrl.failsafe_active());
    assert_eq!(ctrl.reported_frame(), [17.0; 3]); // held target 20 → offset 12 + measured 5
}

#[tokio::test(start_paused = true)]
async fn measurement_stale_pause_reports_the_ceiling() {
    let (sink, msink, offset, ctrl) =
        controller_with(FailsafeMode::FullCharge, FailsafeMode::Pause);
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_millis(1)).await;
    sink.apply(Ok(20.0)); // keep the target fresh; only the measurement is stale
    assert!(ctrl.measurement_failsafe_active());
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]); // pause
}

#[tokio::test(start_paused = true)]
async fn measurement_stale_hold_last_keeps_the_held_measurement() {
    let (sink, msink, offset, ctrl) =
        controller_with(FailsafeMode::FullCharge, FailsafeMode::HoldLast);
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_millis(1)).await;
    sink.apply(Ok(20.0)); // target fresh; measurement stale but held at 5
    assert!(ctrl.measurement_failsafe_active());
    assert_eq!(ctrl.reported_frame(), [17.0; 3]); // offset 12 + held measured 5
}

#[tokio::test(start_paused = true)]
async fn safest_mode_wins_when_both_failsafes_are_active() {
    // Mixed modes both engaged → serve the least-charge directive (pause beats full charge).
    let (sink, msink, offset, ctrl) =
        controller_with(FailsafeMode::Pause, FailsafeMode::FullCharge);
    sink.apply(Ok(20.0));
    offset.send(Ampere(12.0)).unwrap();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_millis(1)).await;
    assert!(ctrl.failsafe_active() && ctrl.measurement_failsafe_active());
    assert_eq!(ctrl.reported_frame(), [MAX.0; 3]); // target=pause wins over measured=full_charge
}

// --- Soft-ramp driver (#24): the offset converges to its setpoint over time. ---

#[tokio::test(start_paused = true)]
async fn ramp_driver_moves_the_offset_toward_the_setpoint() {
    let (tsink, tview) = control::channel(MAX, STALE_AFTER);
    let (offset_tx, offset_view) = control::offset_channel(Ampere(0.0));
    tsink.apply(Ok(12.0)); // setpoint = max − 12 = 20 A
    let driver = tokio::spawn(control::run_ramp(
        tview,
        1.0, // 1 A/s
        Duration::from_secs(1),
        offset_tx,
    ));

    // The controller republishes the target each tick (otherwise it would go stale and the
    // setpoint would fall back to 0). A few 1 s ticks at 1 A/s: the offset climbs toward 20
    // without jumping there at once.
    for _ in 0..3 {
        tsink.apply(Ok(12.0));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    let mid = offset_view.offset().0;
    assert!(
        mid > 0.0 && mid < 20.0,
        "offset should be mid-ramp, got {mid}"
    );

    // Given enough ticks it settles exactly on the setpoint.
    for _ in 0..40 {
        tsink.apply(Ok(12.0));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(offset_view.offset(), Ampere(20.0));

    driver.abort();
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
