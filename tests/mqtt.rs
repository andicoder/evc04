//! Boundary tests for the MQTT control surface (docs/mqtt.md): the inbound
//! target-payload parser and the outbound status schema. The live rumqttc task
//! is an I/O boundary and is exercised against a broker, not here.

use evc04_charge::control::{Controller, MeasurementSink, TargetSink};
use evc04_charge::mqtt::{assemble_status, parse_target, Status, OFFLINE_PAYLOAD};
use evc04_charge::slave::LinkHealth;
use evc04_charge::{control, Ampere};
use std::time::Duration;
use tokio::time::Instant;

const MAX: Ampere = Ampere(32.0);
const MIN: Ampere = Ampere(6.0);
const STALE_AFTER: Duration = Duration::from_secs(5);
const MEAS_STALE: Duration = Duration::from_secs(10);

/// A `Controller` over fresh target + measurement channels (measurement held at 0 A, so
/// `reported_a` reflects the bare offset), plus both sinks to drive commands/measurements.
fn controller() -> (TargetSink, MeasurementSink, Controller) {
    let (target_sink, target_view) = control::channel(MAX, STALE_AFTER);
    let (measured_sink, measured_view) = control::measurement_channel(Ampere(0.0), MEAS_STALE);
    (
        target_sink,
        measured_sink,
        Controller::new(target_view, measured_view, MIN),
    )
}

#[test]
fn parses_valid_target_amps() {
    assert_eq!(parse_target(br#"{"amps": 6.5}"#).unwrap(), 6.5);
}

#[test]
fn parses_integer_amps() {
    assert_eq!(parse_target(br#"{"amps": 10}"#).unwrap(), 10.0);
}

#[test]
fn out_of_range_amps_is_accepted_not_rejected() {
    // The contract clamps in the control math (reported_current), so the parser
    // accepts over- and under-range values; staleness/range handling is downstream.
    assert_eq!(parse_target(br#"{"amps": 999}"#).unwrap(), 999.0);
    assert_eq!(parse_target(br#"{"amps": -5}"#).unwrap(), -5.0);
}

#[test]
fn additive_fields_are_ignored() {
    // The object shape leaves room for future fields; older versions ignore them.
    assert_eq!(
        parse_target(br#"{"amps": 6.5, "mode": "eco"}"#).unwrap(),
        6.5
    );
}

#[test]
fn malformed_json_is_rejected() {
    assert!(parse_target(b"not json").is_err());
}

#[test]
fn missing_amps_is_rejected() {
    assert!(parse_target(br#"{"volts": 230}"#).is_err());
}

#[test]
fn non_numeric_amps_is_rejected() {
    assert!(parse_target(br#"{"amps": "lots"}"#).is_err());
}

#[test]
fn non_finite_amps_is_rejected() {
    // JSON can't carry NaN/Inf literals, but an overflowing exponent decodes to
    // an infinity — guard against pushing that into the control math.
    assert!(parse_target(br#"{"amps": 1e400}"#).is_err());
}

#[test]
fn status_serialises_to_the_documented_schema() {
    let status = Status {
        online: true,
        target_a: 6.5,
        reported_a: 9.5,
        last_poll_age_s: 0.4,
        gateway: "connected".to_string(),
        mqtt: "connected".to_string(),
        failsafe: false,
        measurement_failsafe: false,
        measurement_age_s: 1.2,
        last_error: None,
    };
    let got: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
    let want = serde_json::json!({
        "online": true,
        "target_a": 6.5,
        "reported_a": 9.5,
        "last_poll_age_s": 0.4,
        "gateway": "connected",
        "mqtt": "connected",
        "failsafe": false,
        "measurement_failsafe": false,
        "measurement_age_s": 1.2,
        "last_error": null,
    });
    assert_eq!(got, want);
}

#[test]
fn status_last_error_serialises_as_a_string_when_set() {
    let status = Status {
        online: true,
        target_a: 0.0,
        reported_a: 16.0,
        last_poll_age_s: 1.0,
        gateway: "down".to_string(),
        mqtt: "connected".to_string(),
        failsafe: true,
        measurement_failsafe: false,
        measurement_age_s: 0.5,
        last_error: Some("malformed target payload".to_string()),
    };
    let got: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
    assert_eq!(got["last_error"], "malformed target payload");
    assert_eq!(got["failsafe"], true);
}

#[tokio::test]
async fn assembled_status_reflects_the_live_control_state() {
    let (sink, msink, ctrl) = controller();
    sink.apply(Ok(20.0)); // target 20 A on a 32 A ceiling
    msink.apply(Ok(3.0)); // fresh measurement → offset 12 + 3 = 15

    let status = assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None);

    assert!(status.online);
    assert_eq!(status.mqtt, "connected");
    assert_eq!(status.gateway, "connected");
    assert_eq!(status.target_a, 20.0);
    assert_eq!(status.reported_a, 15.0);
    assert!(!status.failsafe);
    assert!(!status.measurement_failsafe);
    assert!(status.last_error.is_none());
}

#[tokio::test]
async fn assembled_status_maps_each_gateway_health() {
    let (_sink, _msink, ctrl) = controller();
    let label = |h| assemble_status(&ctrl, h, Instant::now(), None).gateway;
    assert_eq!(label(LinkHealth::Up), "connected");
    assert_eq!(label(LinkHealth::Stalled), "reconnecting");
    assert_eq!(label(LinkHealth::Down), "down");
}

#[tokio::test(start_paused = true)]
async fn assembled_status_reports_poll_age_and_failsafe_when_stale() {
    // Once the target goes stale the status must show the failsafe and full charge.
    let (sink, _msink, ctrl) = controller();
    sink.apply(Ok(20.0));
    let last_poll = Instant::now();

    tokio::time::advance(STALE_AFTER + Duration::from_secs(2)).await;

    let status = assemble_status(&ctrl, LinkHealth::Up, last_poll, None);
    assert!(status.failsafe);
    assert_eq!(status.target_a, MAX.0); // full charge
    assert_eq!(status.reported_a, 0.0);
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
    let (sink, msink, ctrl) = controller();
    msink.apply(Ok(5.0));

    tokio::time::advance(MEAS_STALE + Duration::from_secs(1)).await;
    sink.apply(Ok(20.0)); // republish the target so only the measurement is stale

    let status = assemble_status(&ctrl, LinkHealth::Up, Instant::now(), None);
    assert!(status.measurement_failsafe);
    assert!(!status.failsafe); // target is still fresh; only the measurement is stale
    assert_eq!(status.reported_a, 0.0); // full charge
    assert!(
        (status.measurement_age_s - 11.0).abs() < 0.01,
        "got {}",
        status.measurement_age_s
    );
}

#[tokio::test]
async fn assembled_status_surfaces_the_last_error() {
    let (_sink, _msink, ctrl) = controller();
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
