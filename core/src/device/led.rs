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
}
