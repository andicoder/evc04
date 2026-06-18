//! Home Assistant MQTT discovery (issue #46): build the retained config payloads
//! that make HA auto-create read-only sensors for the status object (docs/mqtt.md).
//!
//! **Read-only by design.** A command entity (a writable number/switch) would make
//! HA a second commander, and SPECS §6 mandates exactly one — so none is published.
//! Every entity reads the retained `status` topic, with availability riding the same
//! topic via the `online` flag/LWT, and all are grouped under one HA device.
//!
//! The builder is pure; the actual retained publish happens on `ConnAck` in
//! [`crate::mqtt::run_mqtt`].

use crate::config::DiscoveryConfig;
use serde_json::json;

/// One discovered entity, mapped from a single status field.
struct Entity {
    /// HA component: `sensor` or `binary_sensor`.
    component: &'static str,
    /// Object id in the config topic and the basis of `unique_id` (the status field).
    object_id: &'static str,
    name: &'static str,
    /// Jinja over the status JSON. Binary sensors must yield `ON`/`OFF`.
    value_template: &'static str,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
    state_class: Option<&'static str>,
    /// `true` → HA files it under the device's diagnostics, not the main controls.
    diagnostic: bool,
}

const fn sensor(
    object_id: &'static str,
    name: &'static str,
    value_template: &'static str,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
    state_class: Option<&'static str>,
    diagnostic: bool,
) -> Entity {
    Entity {
        component: "sensor",
        object_id,
        name,
        value_template,
        unit,
        device_class,
        state_class,
        diagnostic,
    }
}

const fn binary(
    object_id: &'static str,
    name: &'static str,
    value_template: &'static str,
    device_class: Option<&'static str>,
) -> Entity {
    Entity {
        component: "binary_sensor",
        object_id,
        name,
        value_template,
        unit: None,
        device_class,
        state_class: None,
        diagnostic: true,
    }
}

const AMP: Option<&str> = Some("A");
const CURRENT: Option<&str> = Some("current");
const MEASUREMENT: Option<&str> = Some("measurement");

/// The status fields we surface, one HA entity each (docs/mqtt.md "Outbound — status").
const ENTITIES: &[Entity] = &[
    sensor(
        "reported_ampere",
        "Reported current",
        "{{ value_json.reported_ampere }}",
        AMP,
        CURRENT,
        MEASUREMENT,
        false,
    ),
    sensor(
        "target_ampere",
        "Target current",
        "{{ value_json.target_ampere }}",
        AMP,
        CURRENT,
        MEASUREMENT,
        false,
    ),
    sensor(
        "measured_ampere",
        "Measured current",
        "{{ value_json.measured_ampere }}",
        AMP,
        CURRENT,
        MEASUREMENT,
        false,
    ),
    sensor(
        "charge_state",
        "Charge state",
        "{{ value_json.charge_state }}",
        None,
        None,
        None,
        false,
    ),
    sensor(
        "offset_ampere",
        "Offset current",
        "{{ value_json.offset_ampere }}",
        AMP,
        CURRENT,
        MEASUREMENT,
        true,
    ),
    sensor(
        "gateway",
        "Gateway link",
        "{{ value_json.gateway }}",
        None,
        None,
        None,
        true,
    ),
    sensor(
        "mqtt",
        "MQTT link",
        "{{ value_json.mqtt }}",
        None,
        None,
        None,
        true,
    ),
    sensor(
        "last_poll_age_s",
        "Last poll age",
        "{{ value_json.last_poll_age_s }}",
        Some("s"),
        Some("duration"),
        MEASUREMENT,
        true,
    ),
    sensor(
        "measurement_age_s",
        "Measurement age",
        "{{ value_json.measurement_age_s }}",
        Some("s"),
        Some("duration"),
        MEASUREMENT,
        true,
    ),
    sensor(
        "last_error",
        "Last error",
        "{{ value_json.last_error }}",
        None,
        None,
        None,
        true,
    ),
    binary(
        "failsafe",
        "Target failsafe",
        "{{ 'ON' if value_json.failsafe else 'OFF' }}",
        Some("problem"),
    ),
    binary(
        "measurement_failsafe",
        "Measurement failsafe",
        "{{ 'ON' if value_json.measurement_failsafe else 'OFF' }}",
        Some("problem"),
    ),
    binary(
        "ramping",
        "Ramping",
        "{{ 'ON' if value_json.ramping else 'OFF' }}",
        None,
    ),
];

/// Build the retained `(config_topic, payload)` pairs to publish for HA discovery.
///
/// Empty when discovery is disabled, so the caller can publish unconditionally.
pub fn discovery_messages(disc: &DiscoveryConfig, status_topic: &str) -> Vec<(String, String)> {
    if !disc.enabled {
        return Vec::new();
    }
    let device = json!({
        "identifiers": [disc.node_id],
        "name": "EVC04 charge",
        "manufacturer": "Vestel",
        "model": "EVC04-AC11-T2P (meter emulation)",
        "sw_version": env!("CARGO_PKG_VERSION"),
    });
    ENTITIES
        .iter()
        .map(|e| {
            let topic = format!(
                "{}/{}/{}/{}/config",
                disc.prefix, e.component, disc.node_id, e.object_id
            );
            let mut payload = json!({
                "name": e.name,
                "unique_id": format!("{}_{}", disc.node_id, e.object_id),
                "state_topic": status_topic,
                "value_template": e.value_template,
                "availability_topic": status_topic,
                "availability_template": "{{ 'online' if value_json.online else 'offline' }}",
                "device": device.clone(),
            });
            if let Some(u) = e.unit {
                payload["unit_of_measurement"] = json!(u);
            }
            if let Some(dc) = e.device_class {
                payload["device_class"] = json!(dc);
            }
            if let Some(sc) = e.state_class {
                payload["state_class"] = json!(sc);
            }
            if e.diagnostic {
                payload["entity_category"] = json!("diagnostic");
            }
            (topic, payload.to_string())
        })
        .collect()
}
