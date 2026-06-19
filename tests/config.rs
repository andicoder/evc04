//! Boundary-validation tests for the env-var config loader (SPECS.md §7).

use evc04_charge::config::{Config, FailsafeMode};
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
fn log_summary_never_leaks_the_broker_password() {
    let vars = with(
        with(valid_vars(), "MQTT_USER", "homeassistant"),
        "MQTT_PASS",
        "s3cr3t-broker-password",
    );
    let cfg = Config::from_vars(vars).unwrap();
    let summary = cfg.log_summary();

    assert!(
        !summary.contains("s3cr3t-broker-password"),
        "startup summary must redact MQTT_PASS, got: {summary}"
    );
    // It must still be useful: name the gateway and the box ceiling.
    assert!(summary.contains("192.168.1.50:4196"), "got: {summary}");
    assert!(summary.contains("16"), "got: {summary}");
}

#[test]
fn ha_discovery_defaults_to_opt_in() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert!(
        !cfg.discovery.enabled,
        "discovery must be off unless opted in"
    );
    assert_eq!(cfg.discovery.prefix, "homeassistant");
    assert_eq!(cfg.discovery.node_id, "evc04");
}

#[test]
fn ha_discovery_reads_its_env_vars() {
    let vars = with(
        with(
            with(valid_vars(), "HA_DISCOVERY_ENABLED", "true"),
            "HA_DISCOVERY_PREFIX",
            "ha",
        ),
        "HA_DISCOVERY_NODE_ID",
        "garage",
    );
    let cfg = Config::from_vars(vars).unwrap();
    assert!(cfg.discovery.enabled);
    assert_eq!(cfg.discovery.prefix, "ha");
    assert_eq!(cfg.discovery.node_id, "garage");
}

#[test]
fn failsafe_modes_default_to_pause() {
    // Safe-by-default for managed (evcc/HA) setups: any control-path fault stops charging
    // rather than starting it (#52). full_charge is opt-in for an HA-automation-only box.
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.target_failsafe, FailsafeMode::Pause);
    assert_eq!(cfg.measured_failsafe, FailsafeMode::Pause);
}

#[test]
fn failsafe_modes_parse_pause_and_hold_last() {
    let vars = with(
        with(valid_vars(), "TARGET_FAILSAFE", "pause"),
        "MEASURED_FAILSAFE",
        "hold_last",
    );
    let cfg = Config::from_vars(vars).unwrap();
    assert_eq!(cfg.target_failsafe, FailsafeMode::Pause);
    assert_eq!(cfg.measured_failsafe, FailsafeMode::HoldLast);
}

#[test]
fn invalid_failsafe_mode_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "TARGET_FAILSAFE", "off")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("TARGET_FAILSAFE"),
        "error should name TARGET_FAILSAFE, got: {err}"
    );
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
fn target_timeout_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.target_timeout, std::time::Duration::from_secs(60));
}

#[test]
fn parses_target_timeout_seconds() {
    let cfg = Config::from_vars(with(valid_vars(), "TARGET_TIMEOUT_SECONDS", "30")).unwrap();
    assert_eq!(cfg.target_timeout, std::time::Duration::from_secs(30));
}

#[test]
fn zero_target_timeout_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "TARGET_TIMEOUT_SECONDS", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("TARGET_TIMEOUT"),
        "error should name TARGET_TIMEOUT_SECONDS, got: {err}"
    );
}

#[test]
fn measured_timeout_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.measured_timeout, std::time::Duration::from_secs(15));
}

#[test]
fn parses_measured_timeout_seconds() {
    let cfg = Config::from_vars(with(valid_vars(), "MEASURED_TIMEOUT_SECONDS", "8")).unwrap();
    assert_eq!(cfg.measured_timeout, std::time::Duration::from_secs(8));
}

#[test]
fn zero_measured_timeout_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "MEASURED_TIMEOUT_SECONDS", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("MEASURED_TIMEOUT"),
        "error should name MEASURED_TIMEOUT_SECONDS, got: {err}"
    );
}

#[test]
fn ramp_rate_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.ramp_rate, 0.5);
}

#[test]
fn parses_ramp_rate() {
    let cfg = Config::from_vars(with(valid_vars(), "RAMP_RATE_AMPERE_PER_SECOND", "1.25")).unwrap();
    assert_eq!(cfg.ramp_rate, 1.25);
}

#[test]
fn zero_ramp_rate_is_rejected() {
    let err =
        Config::from_vars(with(valid_vars(), "RAMP_RATE_AMPERE_PER_SECOND", "0")).unwrap_err();
    assert!(
        format!("{err}").to_uppercase().contains("RAMP_RATE"),
        "error should name RAMP_RATE_AMPERE_PER_SECOND, got: {err}"
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
fn enable_topic_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.mqtt.topic_enable, "evc04/enable");
}

#[test]
fn parses_enable_topic() {
    let cfg = Config::from_vars(with(
        valid_vars(),
        "MQTT_TOPIC_ENABLE",
        "site/charge/enable",
    ))
    .unwrap();
    assert_eq!(cfg.mqtt.topic_enable, "site/charge/enable");
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
fn pause_margin_defaults_when_unset() {
    let cfg = Config::from_vars(valid_vars()).unwrap();
    assert_eq!(cfg.pause_margin, Ampere(4.0));
}

#[test]
fn parses_pause_margin_ampere() {
    let cfg = Config::from_vars(with(valid_vars(), "PAUSE_MARGIN_AMPERE", "2")).unwrap();
    assert_eq!(cfg.pause_margin, Ampere(2.0));
}

#[test]
fn zero_pause_margin_is_rejected() {
    // A pause must report *above* the ceiling to cut an active charge (#57); a zero margin
    // reports exactly the ceiling, which the box holds — so it's nonsense.
    let err = Config::from_vars(with(valid_vars(), "PAUSE_MARGIN_AMPERE", "0")).unwrap_err();
    assert!(
        format!("{err}")
            .to_uppercase()
            .contains("PAUSE_MARGIN_AMPERE"),
        "error should name PAUSE_MARGIN_AMPERE, got: {err}"
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
        "SLAVE_ADDRESS",
        "2",
    );
    let cfg = Config::from_vars(vars).unwrap();
    assert_eq!(cfg.gateway_addr(), "192.168.1.50:4196");
    assert_eq!(cfg.max_box_ampere, Ampere(16.0));
    assert_eq!(cfg.mqtt.user.as_deref(), Some("svc"));
    assert_eq!(cfg.mqtt.pass.as_deref(), Some("secret"));
    assert_eq!(cfg.poll.addr, 2);
}
