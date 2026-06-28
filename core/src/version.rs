//! Serialize the retained `evc04/cn28/version` object (evc04#101): the running
//! firmware build id and its OTA slot, so an operator can read which image is live
//! and whether a freshly-OTA'd image is still pending rollback verification —
//! without inferring the build from the telemetry schema. Pure: the firmware
//! gathers the live values (a build-time `git describe`, the esp-idf slot/state)
//! and hands them in; this module only formats the JSON. `no_std` + `alloc`.

use alloc::format;
use alloc::string::String;

/// The build/identity snapshot the firmware publishes once per session. All fields
/// are internal fixed strings (a build-time `git describe`, an esp-idf slot label),
/// never raw payload, so they need no JSON escaping.
pub struct Version<'a> {
    pub fw: &'a str,
    pub slot: &'a str,
    pub pending_verify: bool,
}

/// Render [`Version`] as the retained version JSON (one flat object, field order
/// fixed so subscribers' value templates stay stable).
pub fn version_json(v: &Version) -> String {
    format!(
        "{{\"fw\":\"{}\",\"slot\":\"{}\",\"pending_verify\":{}}}",
        v.fw, v.slot, v.pending_verify,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_confirmed_slot() {
        let v = Version {
            fw: "v0.3.1-2-gc96eb6c",
            slot: "ota_0",
            pending_verify: false,
        };
        assert_eq!(
            version_json(&v),
            r#"{"fw":"v0.3.1-2-gc96eb6c","slot":"ota_0","pending_verify":false}"#
        );
    }

    #[test]
    fn serializes_a_pending_verify_slot() {
        let v = Version {
            fw: "deadbeef-dirty",
            slot: "ota_1",
            pending_verify: true,
        };
        assert_eq!(
            version_json(&v),
            r#"{"fw":"deadbeef-dirty","slot":"ota_1","pending_verify":true}"#
        );
    }
}
