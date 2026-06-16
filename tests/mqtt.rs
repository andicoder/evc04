//! Boundary tests for the MQTT control surface (docs/mqtt.md): the inbound
//! target-payload parser and the outbound status schema. The live rumqttc task
//! is an I/O boundary and is exercised against a broker, not here.

use evc04_charge::control;
use evc04_charge::mqtt::{assemble_status, parse_target, Status, OFFLINE_PAYLOAD};
use evc04_charge::slave::LinkHealth;
use std::time::Duration;
use tokio::time::Instant;

const FUSE: f32 = 32.0;
const STALE_AFTER: Duration = Duration::from_secs(5);

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
        last_error: Some("malformed target payload".to_string()),
    };
    let got: serde_json::Value = serde_json::from_str(&status.to_json()).unwrap();
    assert_eq!(got["last_error"], "malformed target payload");
    assert_eq!(got["failsafe"], true);
}

#[tokio::test]
async fn assembled_status_reflects_the_live_control_state() {
    let (sink, view) = control::channel(FUSE, 0.0, STALE_AFTER, 0.0);
    sink.apply(Ok(20.0)); // target 20 A on a 32 A fuse → report 12 A

    let status = assemble_status(&view, LinkHealth::Up, Instant::now(), None);

    assert!(status.online);
    assert_eq!(status.mqtt, "connected");
    assert_eq!(status.gateway, "connected");
    assert_eq!(status.target_a, 20.0);
    assert_eq!(status.reported_a, 12.0);
    assert!(!status.failsafe);
    assert!(status.last_error.is_none());
}

#[tokio::test]
async fn assembled_status_maps_each_gateway_health() {
    let (_sink, view) = control::channel(FUSE, 0.0, STALE_AFTER, 0.0);
    let label = |h| assemble_status(&view, h, Instant::now(), None).gateway;
    assert_eq!(label(LinkHealth::Up), "connected");
    assert_eq!(label(LinkHealth::Stalled), "reconnecting");
    assert_eq!(label(LinkHealth::Down), "down");
}

#[tokio::test(start_paused = true)]
async fn assembled_status_reports_poll_age_and_failsafe_when_stale() {
    // failsafe 8 A; once the target goes stale the status must show it.
    let (_sink, view) = control::channel(FUSE, 8.0, STALE_AFTER, 20.0);
    let last_poll = Instant::now();

    tokio::time::advance(STALE_AFTER + Duration::from_secs(2)).await;

    let status = assemble_status(&view, LinkHealth::Up, last_poll, None);
    assert!(status.failsafe);
    assert_eq!(status.target_a, 8.0);
    assert_eq!(status.reported_a, 24.0);
    assert!(
        (status.last_poll_age_s - 7.0).abs() < 0.01,
        "got {}",
        status.last_poll_age_s
    );
}

#[tokio::test]
async fn assembled_status_surfaces_the_last_error() {
    let (_sink, view) = control::channel(FUSE, 0.0, STALE_AFTER, 0.0);
    let status = assemble_status(
        &view,
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
