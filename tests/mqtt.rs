//! Boundary tests for the MQTT control surface (docs/mqtt.md): the inbound
//! target-payload parser and the outbound status schema. The live rumqttc task
//! is an I/O boundary and is exercised against a broker, not here.

use evc04_charge::config::FailsafeMode;
use evc04_charge::control::{Controller, MeasurementSink, TargetSink};
use evc04_charge::mqtt::{assemble_status, parse_enable, parse_target, Status, OFFLINE_PAYLOAD};
use evc04_charge::slave::LinkHealth;
use evc04_charge::{control, Ampere};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

const MAX: Ampere = Ampere(32.0);
const MIN: Ampere = Ampere(6.0);
const MARGIN: Ampere = Ampere(4.0);
const STALE_AFTER: Duration = Duration::from_secs(5);
const MEAS_STALE: Duration = Duration::from_secs(10);

/// A `Controller` over fresh target, measurement, and offset channels, plus the sinks and
/// offset sender to drive them. The measurement starts at 0 A; tests set the offset
/// directly so `reported_ampere` is deterministic (the soft-ramp driver isn't run here).
fn controller() -> (
    TargetSink,
    MeasurementSink,
    watch::Sender<Ampere>,
    Controller,
) {
    let (target_sink, target_view) = control::channel(MAX, STALE_AFTER);
    let (measured_sink, measured_view) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    let (offset_tx, offset_view) = control::offset_channel(Ampere(0.0));
    let (_enable_sink, enable_view) = control::enable_channel(true);
    (
        target_sink,
        measured_sink,
        offset_tx,
        Controller::new(
            target_view,
            measured_view,
            offset_view,
            enable_view,
            MIN,
            MARGIN,
            FailsafeMode::FullCharge,
            FailsafeMode::FullCharge,
        ),
    )
}

#[test]
fn parses_valid_target_ampere() {
    assert_eq!(parse_target(br#"{"ampere": 6.5}"#).unwrap(), 6.5);
}

#[test]
fn parses_integer_ampere() {
    assert_eq!(parse_target(br#"{"ampere": 10}"#).unwrap(), 10.0);
}

#[test]
fn out_of_range_ampere_is_accepted_not_rejected() {
    // The contract clampere in the control math (reported_current), so the parser
    // accepts over- and under-range values; staleness/range handling is downstream.
    assert_eq!(parse_target(br#"{"ampere": 999}"#).unwrap(), 999.0);
    assert_eq!(parse_target(br#"{"ampere": -5}"#).unwrap(), -5.0);
}

#[test]
fn additive_fields_are_ignored() {
    // The object shape leaves room for future fields; older versions ignore them.
    assert_eq!(
        parse_target(br#"{"ampere": 6.5, "mode": "eco"}"#).unwrap(),
        6.5
    );
}

#[test]
fn malformed_json_is_rejected() {
    assert!(parse_target(b"not json").is_err());
}

#[test]
fn missing_ampere_is_rejected() {
    assert!(parse_target(br#"{"volts": 230}"#).is_err());
}

#[test]
fn non_numeric_ampere_is_rejected() {
    assert!(parse_target(br#"{"ampere": "lots"}"#).is_err());
}

#[test]
fn non_finite_ampere_is_rejected() {
    // JSON can't carry NaN/Inf literals, but an overflowing exponent decodes to
    // an infinity — guard against pushing that into the control math.
    assert!(parse_target(br#"{"ampere": 1e400}"#).is_err());
}

#[test]
fn parses_enable_true_and_false() {
    assert!(parse_enable(br#"{"enable": true}"#).unwrap());
    assert!(!parse_enable(br#"{"enable": false}"#).unwrap());
}

#[test]
fn enable_additive_fields_are_ignored() {
    assert!(parse_enable(br#"{"enable": true, "source": "evcc"}"#).unwrap());
}

#[test]
fn malformed_enable_is_rejected() {
    assert!(parse_enable(b"not json").is_err());
    assert!(parse_enable(br#"{"on": true}"#).is_err()); // wrong key
    assert!(parse_enable(br#"{"enable": "yes"}"#).is_err()); // non-boolean
}

#[test]
fn status_serialises_to_the_documented_schema() {
    let status = Status {
        online: true,
        target_ampere: 6.5,
        measured_ampere: 5.2,
        offset_ampere: 1.3,
        reported_ampere: 9.5,
        last_poll_age_s: 0.4,
        gateway: "connected".to_string(),
        mqtt: "connected".to_string(),
        ramping: false,
        failsafe: false,
        measurement_failsafe: false,
        measurement_age_s: 1.2,
        charge_state: "C".to_string(),
        enabled: true,
        last_error: None,
    };
    let got: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
    let want = serde_json::json!({
        "online": true,
        "target_ampere": 6.5,
        "measured_ampere": 5.2,
        "offset_ampere": 1.3,
        "reported_ampere": 9.5,
        "last_poll_age_s": 0.4,
        "gateway": "connected",
        "mqtt": "connected",
        "ramping": false,
        "failsafe": false,
        "measurement_failsafe": false,
        "measurement_age_s": 1.2,
        "charge_state": "C",
        "enabled": true,
        "last_error": null,
    });
    assert_eq!(got, want);
}

#[test]
fn status_last_error_serialises_as_a_string_when_set() {
    let status = Status {
        online: true,
        target_ampere: 0.0,
        measured_ampere: 0.0,
        offset_ampere: 16.0,
        reported_ampere: 16.0,
        last_poll_age_s: 1.0,
        gateway: "down".to_string(),
        mqtt: "connected".to_string(),
        ramping: false,
        failsafe: true,
        measurement_failsafe: false,
        measurement_age_s: 0.5,
        charge_state: "B".to_string(),
        enabled: true,
        last_error: Some("malformed target payload".to_string()),
    };
    let got: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
    assert_eq!(got["last_error"], "malformed target payload");
    assert_eq!(got["failsafe"], true);
}

#[tokio::test]
async fn assembled_status_reflects_the_live_control_state() {
    let (sink, msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0)); // target 20 A on a 32 A ceiling
    offset.send(Ampere(12.0)).unwrap(); // ramped offset = max − target → settled
    msink.apply(Ok(3.0)); // fresh measurement → offset 12 + 3 = 15

    let status = assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None);

    assert!(status.online);
    assert_eq!(status.mqtt, "connected");
    assert_eq!(status.gateway, "connected");
    assert_eq!(status.target_ampere, 20.0);
    assert_eq!(status.measured_ampere, 3.0);
    assert_eq!(status.offset_ampere, 12.0);
    assert_eq!(status.reported_ampere, 15.0);
    assert_eq!(status.charge_state, "C"); // charge allowed (15 < 32) and 3 A flowing
    assert!(!status.ramping); // offset == setpoint (max − target)
    assert!(!status.failsafe);
    assert!(!status.measurement_failsafe);
    assert!(status.last_error.is_none());
}

#[tokio::test]
async fn assembled_status_reports_charge_state_b_when_paused() {
    // A target below MIN_CHARGE serves a hard pause (reported above the ceiling, #57), which
    // evcc must read as "connected, not charging" (B) so its enable=false is seen as effective.
    let (sink, _msink, _offset, ctrl) = controller();
    sink.apply(Ok(3.0)); // below MIN (6 A) → pause
    let status = assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None);
    assert_eq!(status.reported_ampere, MAX.0 + MARGIN.0); // hard pause above the ceiling
    assert_eq!(status.charge_state, "B");
}

#[tokio::test]
async fn assembled_status_flags_ramping_until_the_offset_settles() {
    let (sink, _msink, offset, ctrl) = controller();
    sink.apply(Ok(20.0)); // setpoint offset = max − 20 = 12

    // Offset still short of the setpoint → ramping.
    offset.send(Ampere(5.0)).unwrap();
    assert!(assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None).ramping);

    // Offset reaches the setpoint → settled.
    offset.send(Ampere(12.0)).unwrap();
    assert!(!assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None).ramping);
}

#[tokio::test]
async fn assembled_status_maps_each_gateway_health() {
    let (_sink, _msink, _offset, ctrl) = controller();
    let label = |h| assemble_status(&ctrl, h, Instant::now(), None).gateway;
    assert_eq!(label(LinkHealth::Up), "connected");
    assert_eq!(label(LinkHealth::Stalled), "reconnecting");
    assert_eq!(label(LinkHealth::Down), "down");
}

#[tokio::test(start_paused = true)]
async fn assembled_status_reports_poll_age_and_failsafe_when_stale() {
    // Once the target goes stale the status flags the failsafe and (default full_charge)
    // serves 0 A. `target_ampere` keeps reporting the last commanded value (#51) — the
    // failsafe flag, not a target jump, signals the override.
    let (sink, _msink, _offset, ctrl) = controller();
    sink.apply(Ok(20.0));
    let last_poll = Instant::now();

    tokio::time::advance(STALE_AFTER + Duration::from_secs(2)).await;

    let status = assemble_status(&ctrl, LinkHealth::Up, last_poll, None);
    assert!(status.failsafe);
    assert_eq!(status.target_ampere, 20.0); // last command held, not remapped to MAX
    assert_eq!(status.reported_ampere, 0.0); // full_charge failsafe
    assert!(
        (status.last_poll_age_s - 7.0).abs() < 0.01,
        "got {}",
        status.last_poll_age_s
    );
}

#[tokio::test(start_paused = true)]
async fn assembled_status_reports_the_measurement_failsafe_and_age() {
    // Once the measured input goes stale the status must show the measurement failsafe
    // and full charge, independently of the target failsafe (#25).
    let (sink, msink, _offset, ctrl) = controller();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_secs(1)).await;
    sink.apply(Ok(20.0)); // republish the target so only the measurement is stale

    let status = assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None);
    assert!(status.measurement_failsafe);
    assert!(!status.failsafe); // target is still fresh; only the measurement is stale
    assert_eq!(status.reported_ampere, 0.0); // full charge
    assert!(
        (status.measurement_age_s - 11.0).abs() < 0.01,
        "got {}",
        status.measurement_age_s
    );
}

#[tokio::test]
async fn assembled_status_reports_the_enable_gate() {
    // #60: the status must surface whether charging is gated off, so HA/evcc can see the
    // override independently of the target. Default gate is open (enabled).
    let (esink, enable_view) = control::enable_channel(true);
    let (_tsink, target_view) = control::channel(MAX, STALE_AFTER);
    let (_msink, measured_view) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    let (_otx, offset_view) = control::offset_channel(Ampere(0.0));
    let ctrl = Controller::new(
        target_view,
        measured_view,
        offset_view,
        enable_view,
        MIN,
        MARGIN,
        FailsafeMode::FullCharge,
        FailsafeMode::FullCharge,
    );

    assert!(assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None).enabled);
    esink.apply(Ok(false));
    assert!(!assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None).enabled);
}

#[tokio::test]
async fn assembled_status_surfaces_the_last_error() {
    let (_sink, _msink, _offset, ctrl) = controller();
    let status = assemble_status(
        &ctrl,
        LinkHealth::Up,
        Instant::now(),
        Some("malformed target payload".to_string()),
    );
    assert_eq!(
        status.last_error.as_deref(),
        Some("malformed target payload")
    );
}

#[test]
fn offline_lwt_payload_marks_the_service_offline() {
    let got: serde_json::Value = serde_json::from_slice(OFFLINE_PAYLOAD).unwrap();
    assert_eq!(got, serde_json::json!({ "online": false }));
}
