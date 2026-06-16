//! Boundary-validation tests for the env-var config loader (SPECS.md §7).

use evc04_charge::config::Config;

/// A complete, valid set of env vars — required ones only (optionals omitted so
/// the defaults/None paths are exercised).
fn valid_vars() -> Vec<(String, String)> {
    [
        ("GATEWAY_HOST", "192.168.1.50"),
        ("GATEWAY_PORT", "4196"),
        ("FUSE_LIMIT_A", "16"),
        ("MQTT_HOST", "broker.local"),
        ("MQTT_PORT", "1883"),
        ("MQTT_TOPIC_TARGET", "evc04/target"),
        ("MQTT_TOPIC_STATUS", "evc04/status"),
        ("FAILSAFE_TARGET_A", "0"),
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
fn out_of_range_fuse_limit_is_rejected() {
    let err = Config::from_vars(with(valid_vars(), "FUSE_LIMIT_A", "0")).unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("fuse"),
        "error should mention the fuse limit, got: {err}"
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
    assert_eq!(cfg.fuse_limit_a, 16.0);
    assert_eq!(cfg.mqtt.user.as_deref(), Some("svc"));
    assert_eq!(cfg.mqtt.pass.as_deref(), Some("secret"));
    assert_eq!(cfg.poll.addr, 2);
}
