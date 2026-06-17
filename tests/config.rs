//! Boundary-validation tests for the env-var config loader (SPECS.md §7).

use evc04_charge::config::Config;
use evc04_charge::Ampere;

/// A complete, valid set of env vars — required ones only (optionals omitted so
/// the defaults/None paths are exercised).
fn valid_vars() -> Vec<(String, String)> {
    [
        ("GATEWAY_HOST", "192.168.1.50"),
        ("GATEWAY_PORT", "4196"),
        ("MAX_BOX_AMPERE", "16"),
        ("MQTT_HOST", "broker.local"),
        ("MQTT_PORT", "1883"),
        ("MQTT_TOPIC_TARGET", "evc04/target"),
        ("MQTT_TOPIC_STATUS", "evc04/status"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn with(mut vars: Vec<(String, String)>, key: &str, val: &str) -> Vec<(String, String)> {
    vars.retain(|(k, _)| k != key);
    vars.push((key.to_string(), val.to_string()));
    vars
}

fn without(mut vars: Vec<(String, String)>, key: &str) -> Vec<(String, String)> {
    vars.retain(|(k, _)| k != key);
    vars
}

#[test]
fn missing_required_var_is_a_clear_startup_error() {
    let err = Config::from_vars(without(valid_vars(), "GATEWAY_HOST")).unwrap_err();
    assert!(
        format!("{err}").contains("gateway_host"),
        "error should name the missing var, got: {err}"
    );
}

#[test]
fn defaults_apply_for_optional_vars() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.poll.addr, 1);
    assert_eq!(cfg.poll.register, 0x500C);
    assert_eq!(cfg.poll.qty, 6);
    assert!(cfg.mqtt.user.is_none());
    assert!(cfg.mqtt.pass.is_none());
}

#[test]
fn out_of_range_max_box_ampere_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "MAX_BOX_AMPERE", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("MAX_BOX_AMPERE"),
        "error should name MAX_BOX_AMPERE, got: {err}"
    );
}

#[test]
fn zero_port_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "GATEWAY_PORT", "0")).unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("port"),
        "error should mention the port, got: {err}"
    );
}

#[test]
fn failsafe_after_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.failsafe_after, std::time::Duration::from_secs(60));
}

#[test]
fn parses_failsafe_after_seconds() {
    let cfg = Config::from_vars(with(valid_vars(), "FAILSAFE_AFTER_S", "30")).unwrap();
    assert_eq!(cfg.failsafe_after, std::time::Duration::from_secs(30));
}

#[test]
fn zero_failsafe_after_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "FAILSAFE_AFTER_S", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("FAILSAFE_AFTER"),
        "error should name FAILSAFE_AFTER_S, got: {err}"
    );
}

#[test]
fn meas_stale_timeout_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.meas_stale_timeout, std::time::Duration::from_secs(15));
}

#[test]
fn parses_meas_stale_timeout_seconds() {
    let cfg = Config::from_vars(with(valid_vars(), "MEAS_STALE_TIMEOUT_S", "8")).unwrap();
    assert_eq!(cfg.meas_stale_timeout, std::time::Duration::from_secs(8));
}

#[test]
fn zero_meas_stale_timeout_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "MEAS_STALE_TIMEOUT_S", "0")).unwrap_err();
    assert!(
        format!("{err}")
            .to_uppercase()
            .contains("MEAS_STALE_TIMEOUT"),
        "error should name MEAS_STALE_TIMEOUT_S, got: {err}"
    );
}

#[test]
fn ramp_rate_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.ramp_rate, 0.5);
}

#[test]
fn parses_ramp_rate() {
    let cfg = Config::from_vars(with(valid_vars(), "RAMP_RATE_AMPERE_PER_S", "1.25")).unwrap();
    assert_eq!(cfg.ramp_rate, 1.25);
}

#[test]
fn zero_ramp_rate_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "RAMP_RATE_AMPERE_PER_S", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("RAMP_RATE"),
        "error should name RAMP_RATE_AMPERE_PER_S, got: {err}"
    );
}

#[test]
fn measured_topic_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.mqtt.topic_measured, "evc04/measured");
}

#[test]
fn parses_measured_topic() {
    let cfg = Config::from_vars(with(valid_vars(), "MQTT_TOPIC_MEASURED", "site/ct/L1")).unwrap();
    assert_eq!(cfg.mqtt.topic_measured, "site/ct/L1");
}

#[test]
fn min_charge_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.min_charge, Ampere(6.0));
}

#[test]
fn parses_min_charge_ampere() {
    let cfg = Config::from_vars(with(valid_vars(), "MIN_CHARGE_AMPERE", "8")).unwrap();
    assert_eq!(cfg.min_charge, Ampere(8.0));
}

#[test]
fn min_charge_above_the_ceiling_is_rejected() {
    // MAX_BOX_AMPERE is 16 in valid_vars(); a floor above the ceiling is nonsense.
    let err = Config::from_vars(with(valid_vars(), "MIN_CHARGE_AMPERE", "20")).unwrap_err();
    assert!(
        format!("{err}")
            .to_uppercase()
            .contains("MIN_CHARGE_AMPERE"),
        "error should name MIN_CHARGE_AMPERE, got: {err}"
    );
}

#[test]
fn zero_min_charge_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "MIN_CHARGE_AMPERE", "0")).unwrap_err();
    assert!(
        format!("{err}")
            .to_uppercase()
            .contains("MIN_CHARGE_AMPERE"),
        "error should name MIN_CHARGE_AMPERE, got: {err}"
    );
}

#[test]
fn parses_a_full_valid_config() {
    let vars = with(
        with(
            with(valid_vars(), "MQTT_USER", "svc"),
            "MQTT_PASS",
            "secret",
        ),
        "SLAVE_ADDR",
        "2",
    );
    let cfg = Config::from_vars(vars).unwrap();
    assert_eq!(cfg.gateway_addr(), "192.168.1.50:4196");
    assert_eq!(cfg.max_box_ampere, Ampere(16.0));
    assert_eq!(cfg.mqtt.user.as_deref(), Some("svc"));
    assert_eq!(cfg.mqtt.pass.as_deref(), Some("secret"));
    assert_eq!(cfg.poll.addr, 2);
}
