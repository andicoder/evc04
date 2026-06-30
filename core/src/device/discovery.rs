//! Home Assistant MQTT discovery payloads for the CN28 telemetry (#98), built
//! pure in `core` so the firmware just publishes them. Mirrors the charge daemon's
//! discovery (issue #46) but `no_std` + `alloc` with hand-built JSON (no serde).
//!
//! Every sensor reads the retained telemetry topic. They share **one** HA device
//! (`device.identifiers = [device_id]`), so the CN28 meter and the charge
//! controller — once it moves onto the ESP (#87) — group under a single `evc04`
//! device by reusing the same `device_id` with a distinct `node_id` namespace.
//!
//! Templates use single quotes only, so the payloads need no JSON string escaping.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Shared discovery context for one publisher (cn28 now, charge-on-ESP later).
pub struct DiscoveryMeta<'a> {
    /// HA discovery prefix (default `homeassistant`).
    pub prefix: &'a str,
    /// Config-topic + `unique_id` namespace, unique per publisher (`evc04_cn28`).
    pub node_id: &'a str,
    /// HA device identifier — the same value across publishers groups them under
    /// one device (`evc04`).
    pub device_id: &'a str,
    pub device_name: &'a str,
    pub device_model: &'a str,
    pub sw_version: &'a str,
    /// Retained state topic the sensors read (`evc04/cn28/telemetry`).
    pub state_topic: &'a str,
}

/// One HA entity mapped from a telemetry field.
pub struct Entity<'a> {
    /// `sensor` or `binary_sensor`.
    pub component: &'a str,
    /// Object id in the config topic and basis of `unique_id`.
    pub object_id: &'a str,
    pub name: &'a str,
    /// Jinja over the telemetry JSON (single-quote only, kept JSON-safe).
    pub value_template: &'a str,
    pub unit: Option<&'a str>,
    pub device_class: Option<&'a str>,
    pub state_class: Option<&'a str>,
    pub diagnostic: bool,
    /// When set, the entity gains an `availability_topic` (= the state topic) and
    /// this `availability_template`, so HA renders it *unavailable* (not a stale
    /// value) when the keyed field is absent — used for cp_state, which is null
    /// after boot until the first CP transition (#117).
    pub availability_template: Option<&'a str>,
}

/// Build the retained `(config_topic, payload)` pair for one entity.
pub fn entity_message(meta: &DiscoveryMeta, e: &Entity) -> (String, String) {
    let topic = format!(
        "{}/{}/{}/{}/config",
        meta.prefix, e.component, meta.node_id, e.object_id
    );
    let mut payload = format!(
        "{{\"name\":\"{}\",\"unique_id\":\"{}_{}\",\"state_topic\":\"{}\",\
         \"value_template\":\"{}\",\"device\":{{\"identifiers\":[\"{}\"],\
         \"name\":\"{}\",\"manufacturer\":\"Vestel\",\"model\":\"{}\",\
         \"sw_version\":\"{}\"}}",
        e.name,
        meta.node_id,
        e.object_id,
        meta.state_topic,
        e.value_template,
        meta.device_id,
        meta.device_name,
        meta.device_model,
        meta.sw_version,
    );
    if let Some(u) = e.unit {
        payload.push_str(&format!(",\"unit_of_measurement\":\"{u}\""));
    }
    if let Some(dc) = e.device_class {
        payload.push_str(&format!(",\"device_class\":\"{dc}\""));
    }
    if let Some(sc) = e.state_class {
        payload.push_str(&format!(",\"state_class\":\"{sc}\""));
    }
    if e.diagnostic {
        payload.push_str(",\"entity_category\":\"diagnostic\"");
    }
    if let Some(at) = e.availability_template {
        payload.push_str(&format!(
            ",\"availability_topic\":\"{}\",\"availability_template\":\"{}\"",
            meta.state_topic, at
        ));
    }
    payload.push('}');
    (topic, payload)
}

#[allow(clippy::too_many_arguments)]
fn push(
    out: &mut Vec<(String, String)>,
    meta: &DiscoveryMeta,
    component: &str,
    object_id: &str,
    name: &str,
    value_template: &str,
    unit: Option<&str>,
    device_class: Option<&str>,
    state_class: Option<&str>,
    diagnostic: bool,
) {
    out.push(entity_message(
        meta,
        &Entity {
            component,
            object_id,
            name,
            value_template,
            unit,
            device_class,
            state_class,
            diagnostic,
            availability_template: None,
        },
    ));
}

/// All CN28 telemetry fields as HA entities (#98): per-phase V/A/W/Wh, temperature,
/// the offered/EV/load-balancing currents, the meter-detected verdict and last error.
pub fn cn28_discovery_messages(meta: &DiscoveryMeta) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let m = Some("measurement");
    for n in 1..=3u8 {
        // `value_json.p{n}` is null when that phase has no fresh reading, so each
        // template guards on it to render unavailable rather than error in HA.
        push(
            &mut out,
            meta,
            "sensor",
            &format!("p{n}_voltage"),
            &format!("P{n} voltage"),
            &format!(
                "{{{{ (value_json.p{n}.v_mv / 1000) | round(1) if value_json.p{n} else None }}}}"
            ),
            Some("V"),
            Some("voltage"),
            m,
            false,
        );
        push(
            &mut out,
            meta,
            "sensor",
            &format!("p{n}_current"),
            &format!("P{n} current"),
            &format!(
                "{{{{ (value_json.p{n}.a_ma / 1000) | round(2) if value_json.p{n} else None }}}}"
            ),
            Some("A"),
            Some("current"),
            m,
            false,
        );
        push(
            &mut out,
            meta,
            "sensor",
            &format!("p{n}_power"),
            &format!("P{n} power"),
            &format!("{{{{ value_json.p{n}.w if value_json.p{n} else None }}}}"),
            Some("W"),
            Some("power"),
            m,
            false,
        );
        push(
            &mut out,
            meta,
            "sensor",
            &format!("p{n}_energy"),
            &format!("P{n} energy"),
            &format!("{{{{ value_json.p{n}.wh if value_json.p{n} else None }}}}"),
            Some("Wh"),
            Some("energy"),
            Some("total_increasing"),
            false,
        );
    }
    push(
        &mut out,
        meta,
        "sensor",
        "temperature",
        "Temperature",
        "{{ value_json.temp_c }}",
        Some("°C"),
        Some("temperature"),
        m,
        false,
    );
    push(
        &mut out,
        meta,
        "sensor",
        "ev_current",
        "EV requested current",
        "{{ value_json.ev_current_a }}",
        Some("A"),
        Some("current"),
        m,
        false,
    );
    push(
        &mut out,
        meta,
        "sensor",
        "max_offered_current",
        "Max offered current",
        "{{ value_json.max_offered_a }}",
        Some("A"),
        Some("current"),
        m,
        false,
    );
    push(
        &mut out,
        meta,
        "sensor",
        "lb_current",
        "Load-balancing current",
        "{{ value_json.lb_current_a }}",
        Some("A"),
        Some("current"),
        m,
        false,
    );
    push(
        &mut out,
        meta,
        "binary_sensor",
        "meter_detected",
        "Meter detected",
        "{{ 'ON' if value_json.meter_detected else 'OFF' }}",
        None,
        Some("connectivity"),
        None,
        true,
    );
    push(
        &mut out,
        meta,
        "sensor",
        "last_error",
        "Last error",
        "{{ value_json.last_error }}",
        None,
        None,
        None,
        true,
    );
    // cp_state is null after boot until the first `S:` transition, so key HA
    // availability on its presence: HA shows *unavailable* (not a frozen B/C)
    // while unknown, which evcc maps to "not connected" (#117).
    out.push(entity_message(
        meta,
        &Entity {
            component: "sensor",
            object_id: "cp_state",
            name: "CP state",
            value_template: "{{ value_json.cp_state }}",
            unit: None,
            device_class: None,
            state_class: None,
            diagnostic: false,
            availability_template: Some(
                "{{ 'online' if value_json.cp_state is not none else 'offline' }}",
            ),
        },
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> DiscoveryMeta<'static> {
        DiscoveryMeta {
            prefix: "homeassistant",
            node_id: "evc04_cn28",
            device_id: "evc04",
            device_name: "EVC04 CN28",
            device_model: "EVC04-AC11-T2P",
            sw_version: "v0.1.0-test",
            state_topic: "evc04/cn28/telemetry",
        }
    }

    #[test]
    fn entity_message_builds_topic_and_payload() {
        let e = Entity {
            component: "sensor",
            object_id: "temperature",
            name: "Temperature",
            value_template: "{{ value_json.temp_c }}",
            unit: Some("°C"),
            device_class: Some("temperature"),
            state_class: Some("measurement"),
            diagnostic: false,
            availability_template: None,
        };
        let (topic, payload) = entity_message(&meta(), &e);
        assert_eq!(topic, "homeassistant/sensor/evc04_cn28/temperature/config");
        for needle in [
            r#""unique_id":"evc04_cn28_temperature""#,
            r#""state_topic":"evc04/cn28/telemetry""#,
            r#""value_template":"{{ value_json.temp_c }}""#,
            r#""identifiers":["evc04"]"#,
            r#""unit_of_measurement":"°C""#,
            r#""device_class":"temperature""#,
            r#""state_class":"measurement""#,
        ] {
            assert!(payload.contains(needle), "missing {needle} in {payload}");
        }
        assert!(!payload.contains("entity_category"), "{payload}");
    }

    #[test]
    fn diagnostic_entity_is_categorised() {
        let e = Entity {
            component: "sensor",
            object_id: "last_error",
            name: "Last error",
            value_template: "{{ value_json.last_error }}",
            unit: None,
            device_class: None,
            state_class: None,
            diagnostic: true,
            availability_template: None,
        };
        let (_t, payload) = entity_message(&meta(), &e);
        assert!(
            payload.contains(r#""entity_category":"diagnostic""#),
            "{payload}"
        );
        assert!(!payload.contains("unit_of_measurement"), "{payload}");
    }

    #[test]
    fn cn28_set_covers_phases_temp_verdict_and_error() {
        let msgs = cn28_discovery_messages(&meta());
        // 3 phases x (V/A/W/Wh) + temp + ev + max + lb + meter_detected + last_error + cp_state
        assert_eq!(msgs.len(), 19);
        let topics: Vec<&str> = msgs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(topics.contains(&"homeassistant/sensor/evc04_cn28/p1_voltage/config"));
        assert!(topics.contains(&"homeassistant/sensor/evc04_cn28/p3_energy/config"));
        assert!(
            topics.contains(&"homeassistant/binary_sensor/evc04_cn28/meter_detected/config"),
            "{topics:?}"
        );
    }

    #[test]
    fn phase_voltage_template_guards_an_absent_phase() {
        let msgs = cn28_discovery_messages(&meta());
        let (_t, payload) = msgs
            .iter()
            .find(|(t, _)| t.ends_with("/p1_voltage/config"))
            .expect("p1_voltage present");
        assert!(
            payload.contains("value_json.p1.v_mv / 1000")
                && payload.contains("if value_json.p1 else None"),
            "{payload}"
        );
    }

    #[test]
    fn cp_state_is_unavailable_when_null() {
        let msgs = cn28_discovery_messages(&meta());
        let (_t, payload) = msgs
            .iter()
            .find(|(t, _)| t.ends_with("/cp_state/config"))
            .expect("cp_state present");
        assert!(
            payload.contains(r#""availability_topic":"evc04/cn28/telemetry""#),
            "{payload}"
        );
        assert!(
            payload.contains(
                r#""availability_template":"{{ 'online' if value_json.cp_state is not none else 'offline' }}""#
            ),
            "{payload}"
        );
    }

    #[test]
    fn entities_without_availability_omit_the_keys() {
        let msgs = cn28_discovery_messages(&meta());
        let (_t, payload) = msgs
            .iter()
            .find(|(t, _)| t.ends_with("/temperature/config"))
            .expect("temperature present");
        assert!(!payload.contains("availability_topic"), "{payload}");
        assert!(!payload.contains("availability_template"), "{payload}");
    }
}
