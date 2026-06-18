//! Tests for the Home Assistant MQTT discovery payload builder (issue #46): the
//! retained config topics + payloads are pure, so they are unit-tested here; the
//! actual publish is an I/O boundary in `run_mqtt`.

use evc04_charge::config::DiscoveryConfig;
use evc04_charge::discovery::discovery_messages;
use serde_json::Value;

fn cfg(enabled: bool) -> DiscoveryConfig {
    DiscoveryConfig {
        enabled,
        prefix: "homeassistant".to_string(),
        node_id: "evc04".to_string(),
    }
}

fn payload_for<'a>(msgs: &'a [(String, String)], suffix: &str) -> (&'a str, Value) {
    let (topic, payload) = msgs
        .iter()
        .find(|(t, _)| t.ends_with(suffix))
        .unwrap_or_else(|| panic!("no discovery message ending in {suffix}"));
    (topic, serde_json::from_str(payload).unwrap())
}

#[test]
fn disabled_produces_no_messages() {
    assert!(discovery_messages(&cfg(false), "evc04/status").is_empty());
}

#[test]
fn current_sensor_targets_its_topic_and_reads_the_status_topic() {
    let msgs = discovery_messages(&cfg(true), "evc04/status");
    let (topic, v) = payload_for(&msgs, "/reported_ampere/config");
    assert_eq!(topic, "homeassistant/sensor/evc04/reported_ampere/config");
    assert_eq!(v["state_topic"], "evc04/status");
    assert_eq!(v["value_template"], "{{ value_json.reported_ampere }}");
    assert_eq!(v["unit_of_measurement"], "A");
    assert_eq!(v["device_class"], "current");
    assert_eq!(v["unique_id"], "evc04_reported_ampere");
    // Availability rides the same retained status topic via the `online` flag/LWT.
    assert_eq!(v["availability_topic"], "evc04/status");
    // Grouped under one device so HA shows a single wallbox.
    assert_eq!(v["device"]["identifiers"][0], "evc04");
}

#[test]
fn failsafe_is_a_binary_sensor_with_an_on_off_template() {
    let msgs = discovery_messages(&cfg(true), "evc04/status");
    let (topic, v) = payload_for(&msgs, "/failsafe/config");
    assert_eq!(topic, "homeassistant/binary_sensor/evc04/failsafe/config");
    assert_eq!(
        v["value_template"],
        "{{ 'ON' if value_json.failsafe else 'OFF' }}"
    );
}

#[test]
fn no_command_entity_is_published() {
    // SPECS §6: read-only only — a command entity would be a second commander.
    let msgs = discovery_messages(&cfg(true), "evc04/status");
    assert!(
        msgs.iter().all(|(_, p)| !p.contains("command_topic")),
        "discovery must not publish any command entity"
    );
}

#[test]
fn unique_ids_are_unique() {
    let msgs = discovery_messages(&cfg(true), "evc04/status");
    let ids: Vec<String> = msgs
        .iter()
        .map(|(_, p)| {
            serde_json::from_str::<Value>(p).unwrap()["unique_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut dedup = ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(ids.len(), dedup.len(), "duplicate unique_id in {ids:?}");
}

#[test]
fn prefix_and_node_id_are_honoured() {
    let custom = DiscoveryConfig {
        enabled: true,
        prefix: "ha".to_string(),
        node_id: "garage".to_string(),
    };
    let msgs = discovery_messages(&custom, "garage/status");
    assert!(!msgs.is_empty());
    assert!(msgs
        .iter()
        .all(|(t, _)| t.starts_with("ha/") && t.contains("/garage/")));
}
