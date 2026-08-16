//! Serialize the retained MQTT status object (evc04#86), mirroring
//! `docs/mqtt.md` so an external controller (evcc / Home Assistant) reads the
//! on-box firmware in the documented contract shape. Pure: the firmware gathers the
//! live values and hands them in; this module only derives `charge_state` and
//! formats the JSON. `no_std` + `alloc`, no serde — the object is flat and fixed.

use alloc::format;
use alloc::string::String;

use super::control::Ampere;
use crate::probe::cn28::CpState;

/// Measured current above which the car is definitely drawing (#158). Well above
/// the ~40 mA per-phase noise an idle box reads, and well below the 6 A minimum of
/// any real charge, so neither noise nor a genuine charge sits near the boundary.
const CURRENT_FLOWING_AMPERE: f32 = 1.0;

/// evcc charge status (#148): mirror the box's real control-pilot state (CN28 LOG
/// `S:` line) instead of approximating from our command. `""` when the pilot is
/// unknown — post-reboot blind window (`cp_state` is transition-only, #117), a
/// stale CN28 feed, or a fault — because evcc's `ChargeStatusString("")` errors
/// and the loadpoint then *retains* its last status: never a phantom unplug or
/// connect. Our pause (reporting **above** the ceiling so the box cuts, #57)
/// still forces `B` while the pilot reads `C`, so evcc's power estimate drops to
/// 0 during the ramp-down. Reporting exactly the ceiling holds an active charge,
/// so `== max` is still `C`.
///
/// The pilot is only trusted where it cannot have gone stale: a non-`C` letter with
/// the metering showing real current is overridden to `C` (#158). See the match arm.
pub fn charge_state(
    reported: Ampere,
    max: Ampere,
    pause_margin: Ampere,
    cp_state: Option<CpState>,
    cn28_stale: bool,
    measured: Ampere,
) -> &'static str {
    if cn28_stale {
        return "";
    }
    match cp_state {
        // A pilot that is not `C` while the metering shows the car drawing is a
        // physical contradiction — current only flows in state C — and the pilot is
        // the untrustworthy half: `S:` is transition-only, so one lost line pins the
        // letter indefinitely (#158, live 25 h of `B` through a 7 kW charge). The
        // metering is stateless and cannot go stale that way, which is the resolution
        // #117 already pointed at. Only ever upgrades: `C` is left alone below,
        // because a real state C may legitimately draw nothing while the car pauses,
        // and downgrading on that would flap.
        None | Some(CpState::NoVehicle) | Some(CpState::Connected)
            if measured.0 >= CURRENT_FLOWING_AMPERE =>
        {
            "C"
        }
        // A fault is never overridden — `F` means the box itself is unhappy, and
        // guessing past that is not the metering's job.
        None | Some(CpState::Fault) => "",
        Some(CpState::NoVehicle) => "A",
        Some(CpState::Connected) => "B",
        Some(CpState::Charging) => {
            // Only a pause-level report (the ceiling plus the full margin, #57)
            // forces `B`: a V4 shed report (max+1..max+2) still charges, and
            // flashing `B` mid-shed zeroes evcc's charge-power estimate and
            // rattles its PV loop (live 2026-07-05).
            if reported.0 >= max.0 + pause_margin.0 {
                "B"
            } else {
                "C"
            }
        }
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
    pub charge_state: &'a str,
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

    fn fresh(cp: CpState) -> Option<CpState> {
        Some(cp)
    }

    #[test]
    fn no_vehicle_reports_a() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::NoVehicle),
                false,
                Ampere(0.0),
            ),
            "A"
        );
    }

    #[test]
    fn no_vehicle_wins_even_when_pausing() {
        assert_eq!(
            charge_state(
                Ampere(16.5),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::NoVehicle),
                false,
                Ampere(0.0),
            ),
            "A"
        );
    }

    #[test]
    fn connected_idle_reports_b() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Connected),
                false,
                Ampere(0.0),
            ),
            "B"
        );
    }

    #[test]
    fn charging_reports_c() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(0.0),
            ),
            "C"
        );
    }

    #[test]
    fn charging_at_exactly_the_ceiling_reports_c() {
        assert_eq!(
            charge_state(
                Ampere(16.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(0.0),
            ),
            "C"
        );
    }

    #[test]
    fn our_pause_reports_b_even_if_pilot_charging() {
        // A pause report is the ceiling plus the full margin (#57).
        assert_eq!(
            charge_state(
                Ampere(20.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(0.0),
            ),
            "B"
        );
    }

    #[test]
    fn shed_report_keeps_c_while_the_pilot_charges() {
        // Live 2026-07-05: a 1 A shed report (max+1, V4 down-regulation — the car
        // keeps charging) flashed `B` at evcc, dropping its charge-power estimate
        // to 0 mid-charge and rattling the PV loop. Only a pause-level report
        // (≥ max + margin) may force `B`; sheds stay `C`.
        assert_eq!(
            charge_state(
                Ampere(17.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(0.0),
            ),
            "C"
        );
        assert_eq!(
            charge_state(
                Ampere(18.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(0.0),
            ),
            "C"
        );
    }

    #[test]
    fn unknown_pilot_reports_empty() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                None,
                false,
                Ampere(0.0)
            ),
            ""
        );
    }

    #[test]
    fn stale_feed_reports_empty() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                true,
                Ampere(0.0),
            ),
            ""
        );
    }

    // --- Measured current outranks a latched pilot (#158) --------------------
    // `S:` is emitted only on transitions, so one lost line pins the pilot letter
    // indefinitely — live on 2026-08-14 it read `B` for 25 h through a 7 kW charge,
    // and again on 2026-08-16 while the car pulled 15 A. Current can only flow in
    // state C, so the box's own metering settles the contradiction. #117 reached
    // the same conclusion from the other end: the reliable-after-boot answer is
    // "a live, stateless source (external charge-power measurement)", not a
    // remembered categorical state.

    #[test]
    fn measured_current_overrides_a_latched_connected_pilot() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Connected),
                false,
                Ampere(15.0),
            ),
            "C"
        );
    }

    #[test]
    fn measured_current_resolves_a_pilot_that_is_still_unknown() {
        // After a reboot mid-charge the box has seen no transition at all; the
        // metering still proves the car is drawing.
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                None,
                false,
                Ampere(15.0)
            ),
            "C"
        );
    }

    #[test]
    fn metering_noise_does_not_fake_a_charge() {
        // An idle box reads ~40 mA of noise per phase; the lowest real charge is 6 A.
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Connected),
                false,
                Ampere(0.045),
            ),
            "B"
        );
    }

    #[test]
    fn measured_current_never_overrides_a_fault() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Fault),
                false,
                Ampere(15.0),
            ),
            ""
        );
    }

    #[test]
    fn measured_current_does_not_override_a_stale_feed() {
        // A stale CN28 feed means the metering is as untrustworthy as the pilot.
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Connected),
                true,
                Ampere(15.0),
            ),
            ""
        );
    }

    #[test]
    fn our_deliberate_pause_still_reports_b_while_current_is_still_flowing() {
        // The pause report (#57) forces `B` on purpose during the ramp-down, and
        // current is still flowing at that moment. The cross-check must not undo it
        // — it corrects a *latched* pilot, never our own commanded state.
        assert_eq!(
            charge_state(
                Ampere(20.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Charging),
                false,
                Ampere(15.0),
            ),
            "B"
        );
    }

    #[test]
    fn an_unplugged_pilot_with_no_current_still_reports_a() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::NoVehicle),
                false,
                Ampere(0.0),
            ),
            "A"
        );
    }

    #[test]
    fn fault_reports_empty() {
        assert_eq!(
            charge_state(
                Ampere(6.0),
                Ampere(16.0),
                Ampere(4.0),
                fresh(CpState::Fault),
                false,
                Ampere(0.0),
            ),
            ""
        );
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
            charge_state: "C",
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
            charge_state: "B",
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

    #[test]
    fn json_emits_empty_charge_state() {
        let s = Status {
            online: true,
            target_ampere: 6.0,
            grid_power_w: 0.0,
            reported_ampere: 16.5,
            last_poll_age_s: 0.2,
            grid_age_s: 1.0,
            grid_failsafe: false,
            charge_state: "",
            enabled: true,
            last_error: None,
            lb_current_ampere: 0.0,
            cn28_feedback_stale: false,
            probe_over_ampere: 0.0,
        };
        let json = status_json(&s);
        assert!(json.contains(r#""charge_state":"""#), "{json}");
    }
}
