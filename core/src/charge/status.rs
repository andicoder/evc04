//! Serialize the retained MQTT status object (evc04#86), mirroring
//! `charge/docs/mqtt.md` so an external controller (evcc / Home Assistant) reads
//! the on-box firmware exactly like the k3s daemon. Pure: the firmware gathers the
//! live values and hands them in; this module only derives `charge_state` and
//! formats the JSON. `no_std` + `alloc`, no serde — the object is flat and fixed.

use alloc::format;
use alloc::string::String;

use super::control::Ampere;

/// Approximate evcc charge status (#28): `B` (connected, not charging) when the
/// emulation pauses the box — reporting **above** the ceiling so the box actually
/// cuts (#57) — otherwise `C` (charging allowed / current may flow). Reporting
/// exactly the ceiling holds an active charge, so `== max` is still `C`. `A` (no
/// vehicle) is never asserted: a meter emulation has no control-pilot line.
pub fn charge_state(reported: Ampere, max: Ampere) -> char {
    if reported.0 > max.0 {
        'B'
    } else {
        'C'
    }
}

/// The live status snapshot the firmware hands in each publish. Amperes are plain
/// `f32` here — this is the wire boundary.
pub struct Status<'a> {
    pub online: bool,
    pub target_ampere: f32,
    pub measured_ampere: f32,
    pub offset_ampere: f32,
    pub reported_ampere: f32,
    pub last_poll_age_s: f32,
    pub measurement_age_s: f32,
    pub ramping: bool,
    pub failsafe: bool,
    pub measurement_failsafe: bool,
    pub charge_state: char,
    pub enabled: bool,
    /// Reason for the most recent rejected input, or `None` when healthy. Internal
    /// fixed strings (never raw payload), so they need no JSON escaping.
    pub last_error: Option<&'a str>,
}

/// Render [`Status`] as the retained status JSON (one flat object, field order
/// fixed so Home Assistant value templates stay stable).
pub fn status_json(s: &Status) -> String {
    let last_error = match s.last_error {
        Some(e) => format!("\"{e}\""),
        None => String::from("null"),
    };
    format!(
        "{{\"online\":{},\"target_ampere\":{},\"measured_ampere\":{},\
         \"offset_ampere\":{},\"reported_ampere\":{},\"last_poll_age_s\":{},\
         \"measurement_age_s\":{},\"ramping\":{},\"failsafe\":{},\
         \"measurement_failsafe\":{},\"charge_state\":\"{}\",\"enabled\":{},\
         \"last_error\":{}}}",
        s.online,
        s.target_ampere,
        s.measured_ampere,
        s.offset_ampere,
        s.reported_ampere,
        s.last_poll_age_s,
        s.measurement_age_s,
        s.ramping,
        s.failsafe,
        s.measurement_failsafe,
        s.charge_state,
        s.enabled,
        last_error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_when_reported_below_ceiling() {
        assert_eq!(charge_state(Ampere(6.0), Ampere(16.0)), 'C');
    }

    #[test]
    fn charges_when_reported_zero() {
        assert_eq!(charge_state(Ampere(0.0), Ampere(16.0)), 'C');
    }

    #[test]
    fn charges_at_exactly_the_ceiling() {
        assert_eq!(charge_state(Ampere(16.0), Ampere(16.0)), 'C');
    }

    #[test]
    fn not_charging_when_paused_above_ceiling() {
        assert_eq!(charge_state(Ampere(16.5), Ampere(16.0)), 'B');
    }

    #[test]
    fn serializes_a_healthy_status() {
        let s = Status {
            online: true,
            target_ampere: 6.5,
            measured_ampere: 5.0,
            offset_ampere: 1.5,
            reported_ampere: 6.5,
            last_poll_age_s: 0.4,
            measurement_age_s: 1.1,
            ramping: false,
            failsafe: false,
            measurement_failsafe: false,
            charge_state: 'C',
            enabled: true,
            last_error: None,
        };
        assert_eq!(
            status_json(&s),
            r#"{"online":true,"target_ampere":6.5,"measured_ampere":5,"offset_ampere":1.5,"reported_ampere":6.5,"last_poll_age_s":0.4,"measurement_age_s":1.1,"ramping":false,"failsafe":false,"measurement_failsafe":false,"charge_state":"C","enabled":true,"last_error":null}"#
        );
    }

    #[test]
    fn serializes_last_error_as_a_quoted_string() {
        let s = Status {
            online: true,
            target_ampere: 6.0,
            measured_ampere: 0.0,
            offset_ampere: 10.0,
            reported_ampere: 10.0,
            last_poll_age_s: 0.2,
            measurement_age_s: 0.3,
            ramping: true,
            failsafe: false,
            measurement_failsafe: true,
            charge_state: 'C',
            enabled: false,
            last_error: Some("bad target"),
        };
        let json = status_json(&s);
        assert!(json.contains(r#""last_error":"bad target""#), "{json}");
        assert!(json.contains(r#""measurement_failsafe":true"#), "{json}");
        assert!(json.contains(r#""enabled":false"#), "{json}");
    }
}
