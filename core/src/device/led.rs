//! Status-LED state machine (evc04#123): pick the single state the on-box LED
//! shows from the live device conditions, and render its blink pattern. Pure so it
//! is host-tested; the firmware owns the GPIO and the timing loop, and only feeds
//! the conditions in and toggles the pin from [`led_on`].

/// The live device conditions the LED reflects. The firmware fills these from the
/// signals it already has: `error` from the control failsafe / `last_error` / a dead
/// RS485 poll, `ota` while a firmware image is downloading/flashing, `wifi_up` /
/// `mqtt_up` from the connection state, `charging` from `charge_state == 'C'`.
pub struct LedInputs {
    pub error: bool,
    pub ota: bool,
    pub wifi_up: bool,
    pub mqtt_up: bool,
    pub charging: bool,
}

/// The one state the LED shows. A single LED can only show one thing, so the
/// variants are a strict priority order (highest first): a fault must be seen over
/// "charging", and a connectivity loss over normal operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    /// Control failsafe / rejected input / dead RS485 link — the box is being held.
    Error,
    /// Firmware download/flash in progress.
    Ota,
    /// Not associated to the AP / no IP.
    WifiDown,
    /// WiFi up but the broker is unreachable.
    MqttDown,
    /// Online and current is flowing to the car.
    Charging,
    /// Online and healthy, no charge in progress.
    Idle,
}

/// Resolve the highest-priority state the conditions call for. 🤔 The order
/// (connectivity above charging) is a judgement call (#123); flip here if a live
/// glance should favour the charge state over a broker blip.
pub fn led_state(i: &LedInputs) -> LedState {
    if i.error {
        LedState::Error
    } else if i.ota {
        LedState::Ota
    } else if !i.wifi_up {
        LedState::WifiDown
    } else if !i.mqtt_up {
        LedState::MqttDown
    } else if i.charging {
        LedState::Charging
    } else {
        LedState::Idle
    }
}

/// Whether the LED is lit `elapsed_ms` into `state`'s repeating blink pattern. The
/// firmware calls this from its LED-timing thread with a millisecond clock and drives
/// the pin accordingly. Each state has a distinct rate/duty so a single monochrome LED
/// still tells the states apart:
///   - Error — fast even blink (~5 Hz), "look at me".
///   - Ota — very fast flicker (~10 Hz).
///   - WifiDown — slow even blink (~1 Hz).
///   - MqttDown — a double-blip then a long pause.
///   - Charging — a long pulse, briefly dipping (a coarse "breathing" without PWM).
///   - Idle — a short heartbeat blip every few seconds.
pub fn led_on(state: LedState, elapsed_ms: u32) -> bool {
    match state {
        LedState::Error => phase(elapsed_ms, 200) < 100, // ~5 Hz, 50% duty
        LedState::Ota => phase(elapsed_ms, 100) < 50,    // ~10 Hz, 50% duty
        LedState::WifiDown => phase(elapsed_ms, 1000) < 500, // ~1 Hz, 50% duty
        LedState::MqttDown => {
            // two BLIP-wide pulses at the start of a 2 s cycle, then a long pause.
            let t = phase(elapsed_ms, 2000);
            t < BLIP_MS || (2 * BLIP_MS..3 * BLIP_MS).contains(&t)
        }
        LedState::Charging => phase(elapsed_ms, 2000) < 1600, // long pulse, brief dip
        LedState::Idle => phase(elapsed_ms, 3000) < BLIP_MS,  // one heartbeat blip
    }
}

/// A single on/off pulse width, reused for the mqtt double-blip and the idle
/// heartbeat so both read as the same short flash.
const BLIP_MS: u32 = 150;

/// Position within a `period_ms` cycle, in milliseconds.
fn phase(elapsed_ms: u32, period_ms: u32) -> u32 {
    elapsed_ms % period_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> LedInputs {
        LedInputs {
            error: false,
            ota: false,
            wifi_up: true,
            mqtt_up: true,
            charging: false,
        }
    }

    #[test]
    fn error_dominates_every_other_condition() {
        let i = LedInputs {
            error: true,
            ota: true,
            wifi_up: false,
            mqtt_up: false,
            charging: true,
        };
        assert_eq!(led_state(&i), LedState::Error);
    }

    #[test]
    fn ota_shows_when_there_is_no_error() {
        let i = LedInputs {
            ota: true,
            wifi_up: false,
            charging: true,
            ..healthy()
        };
        assert_eq!(led_state(&i), LedState::Ota);
    }

    #[test]
    fn wifi_down_outranks_mqtt_and_charging() {
        let i = LedInputs {
            wifi_up: false,
            mqtt_up: false,
            charging: true,
            ..healthy()
        };
        assert_eq!(led_state(&i), LedState::WifiDown);
    }

    #[test]
    fn mqtt_down_shows_when_wifi_is_up() {
        let i = LedInputs {
            mqtt_up: false,
            charging: true,
            ..healthy()
        };
        assert_eq!(led_state(&i), LedState::MqttDown);
    }

    #[test]
    fn charging_shows_when_connected_and_current_flows() {
        let i = LedInputs {
            charging: true,
            ..healthy()
        };
        assert_eq!(led_state(&i), LedState::Charging);
    }

    #[test]
    fn idle_when_connected_and_not_charging() {
        assert_eq!(led_state(&healthy()), LedState::Idle);
    }

    #[test]
    fn error_is_a_fast_even_blink() {
        assert!(led_on(LedState::Error, 50));
        assert!(!led_on(LedState::Error, 150));
        // repeats every 200 ms
        assert!(led_on(LedState::Error, 250));
    }

    #[test]
    fn ota_flickers_faster_than_error() {
        assert!(led_on(LedState::Ota, 20));
        assert!(!led_on(LedState::Ota, 70));
    }

    #[test]
    fn wifi_down_is_a_slow_even_blink() {
        assert!(led_on(LedState::WifiDown, 100));
        assert!(!led_on(LedState::WifiDown, 700));
    }

    #[test]
    fn mqtt_down_is_two_blips_then_a_long_pause() {
        assert!(led_on(LedState::MqttDown, 50)); // first blip
        assert!(!led_on(LedState::MqttDown, 200)); // gap between blips
        assert!(led_on(LedState::MqttDown, 350)); // second blip
        assert!(!led_on(LedState::MqttDown, 1000)); // the long pause
    }

    #[test]
    fn charging_is_mostly_on_with_a_brief_dip() {
        assert!(led_on(LedState::Charging, 100));
        assert!(!led_on(LedState::Charging, 1800));
    }

    #[test]
    fn idle_is_a_brief_heartbeat_blip() {
        assert!(led_on(LedState::Idle, 10)); // the blip
        assert!(!led_on(LedState::Idle, 500)); // off the rest of the cycle
        assert!(led_on(LedState::Idle, 3000)); // next heartbeat
    }
}
