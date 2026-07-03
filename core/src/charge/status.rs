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

/// The live status snapshot the firmware hands in each publish (V4, #135/#136).
/// Amperes/watts are plain `f32` here — this is the wire boundary.
pub struct Status<'a> {
    pub online: bool,
    pub target_ampere: f32,
    /// The raw signed grid power heartbeat (#136), passed through untouched —
    /// negative = export. Diagnostic only; V4 regulates on the grant, not on this.
    pub grid_power_w: f32,
    pub reported_ampere: f32,
    pub last_poll_age_s: f32,
    pub grid_age_s: f32,
    /// The grid heartbeat aged out → the controller is pausing the box.
    pub grid_failsafe: bool,
    pub charge_state: char,
    pub enabled: bool,
    /// Reason for the most recent rejected input, or `None` when healthy. Internal
    /// fixed strings (never raw payload), so they need no JSON escaping.
    pub last_error: Option<&'a str>,
    /// The box's current grant (`lb_current` from the CN28 LOG) — the V4 feedback.
    pub lb_current_ampere: f32,
    /// The CN28 feedback aged out → the regulation is blind, the box is paused.
    pub cn28_feedback_stale: bool,
    /// #135 step 6: active measurement-probe lift over the ceiling (A), 0 when off.
    /// While set, `reported_ampere` sits above the ceiling *on purpose*.
    pub probe_over_ampere: f32,
}

/// Render [`Status`] as the retained status JSON (one flat object, field order
/// fixed so Home Assistant value templates stay stable).
pub fn status_json(s: &Status) -> String {
    let last_error = match s.last_error {
        Some(e) => format!("\"{e}\""),
        None => String::from("null"),
    };
    format!(
        "{{\"online\":{},\"target_ampere\":{},\"grid_power_w\":{},\
         \"reported_ampere\":{},\"last_poll_age_s\":{},\"grid_age_s\":{},\
         \"grid_failsafe\":{},\"charge_state\":\"{}\",\"enabled\":{},\
         \"last_error\":{},\"lb_current_ampere\":{},\
         \"cn28_feedback_stale\":{},\"probe_over_ampere\":{}}}",
        s.online,
        s.target_ampere,
        s.grid_power_w,
        s.reported_ampere,
        s.last_poll_age_s,
        s.grid_age_s,
        s.grid_failsafe,
        s.charge_state,
        s.enabled,
        last_error,
        s.lb_current_ampere,
        s.cn28_feedback_stale,
        s.probe_over_ampere,
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
            grid_power_w: -3200.0,
            reported_ampere: 16.0,
            last_poll_age_s: 0.4,
            grid_age_s: 1.1,
            grid_failsafe: false,
            charge_state: 'C',
            enabled: true,
            last_error: None,
            lb_current_ampere: 7.0,
            cn28_feedback_stale: false,
            probe_over_ampere: 0.0,
        };
        assert_eq!(
            status_json(&s),
            r#"{"online":true,"target_ampere":6.5,"grid_power_w":-3200,"reported_ampere":16,"last_poll_age_s":0.4,"grid_age_s":1.1,"grid_failsafe":false,"charge_state":"C","enabled":true,"last_error":null,"lb_current_ampere":7,"cn28_feedback_stale":false,"probe_over_ampere":0}"#
        );
    }

    #[test]
    fn serializes_last_error_as_a_quoted_string() {
        let s = Status {
            online: true,
            target_ampere: 6.0,
            grid_power_w: 450.0,
            reported_ampere: 20.0,
            last_poll_age_s: 0.2,
            grid_age_s: 17.3,
            grid_failsafe: true,
            charge_state: 'B',
            enabled: false,
            last_error: Some("bad target"),
            lb_current_ampere: 9.0,
            cn28_feedback_stale: true,
            probe_over_ampere: 1.5,
        };
        let json = status_json(&s);
        assert!(json.contains(r#""last_error":"bad target""#), "{json}");
        assert!(json.contains(r#""grid_failsafe":true"#), "{json}");
        assert!(json.contains(r#""enabled":false"#), "{json}");
        assert!(json.contains(r#""lb_current_ampere":9"#), "{json}");
        assert!(json.contains(r#""cn28_feedback_stale":true"#), "{json}");
        assert!(json.contains(r#""probe_over_ampere":1.5"#), "{json}");
    }
}
