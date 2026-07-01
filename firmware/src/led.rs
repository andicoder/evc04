//! On-box status LED (evc04#123). A single monochrome LED on GPIO2 (the WROOM-32
//! onboard LED, active-high) shows the device state at a glance.
//!
//! The workers set **condition bits** in one lock-free [`AtomicU8`] via the `set_*`
//! helpers — each owns its own bit, so no worker clobbers another's signal. A
//! dedicated timing thread resolves those bits to a single [`LedState`] with the
//! host-tested `core` logic and renders its blink pattern, so blink timing never
//! competes with the ~1 Hz control tick or the RS485 poll answer.
//!
//! 🤔 GPIO2 / active-high is the WROOM-32 DevKitC onboard LED from memory — confirm
//! the pin and polarity on the physical module (#123).

use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::gpio::{Output, PinDriver};
use esp_idf_svc::sys::esp_timer_get_time;
use evc04_cn28_core::device::led::{led_on, led_state, LedInputs};

const ERROR: u8 = 1 << 0;
const OTA: u8 = 1 << 1;
const WIFI_UP: u8 = 1 << 2;
const MQTT_UP: u8 = 1 << 3;
const CHARGING: u8 = 1 << 4;

/// Condition bits: workers write, the LED thread reads. Starts all-zero — wifi and
/// mqtt "down" — so the LED shows the bring-up (wifi-down) pattern from boot until
/// the first join.
static BITS: AtomicU8 = AtomicU8::new(0);

/// How often the LED thread re-samples the state and re-drives the pin. 20 ms (~50 Hz)
/// keeps even the fastest pattern (the ~10 Hz OTA flicker) crisp.
const TICK: Duration = Duration::from_millis(20);

fn set_flag(mask: u8, on: bool) {
    if on {
        BITS.fetch_or(mask, Ordering::Relaxed);
    } else {
        BITS.fetch_and(!mask, Ordering::Relaxed);
    }
}

pub fn set_wifi_up(up: bool) {
    set_flag(WIFI_UP, up);
}
pub fn set_mqtt_up(up: bool) {
    set_flag(MQTT_UP, up);
}
pub fn set_ota(active: bool) {
    set_flag(OTA, active);
}
pub fn set_charging(active: bool) {
    set_flag(CHARGING, active);
}
pub fn set_error(active: bool) {
    set_flag(ERROR, active);
}

fn inputs() -> LedInputs {
    let b = BITS.load(Ordering::Relaxed);
    LedInputs {
        error: b & ERROR != 0,
        ota: b & OTA != 0,
        wifi_up: b & WIFI_UP != 0,
        mqtt_up: b & MQTT_UP != 0,
        charging: b & CHARGING != 0,
    }
}

/// LED-timing thread routine (`main` spawns it and owns the pin construction). Loops
/// forever: resolve the current state, ask `core` whether the LED is lit this instant,
/// drive the pin. A GPIO write can't meaningfully fail, so any error is ignored.
pub fn run(mut pin: PinDriver<'static, Output>) -> ! {
    loop {
        let state = led_state(&inputs());
        let now_ms = (unsafe { esp_timer_get_time() } / 1000) as u32;
        let _ = if led_on(state, now_ms) {
            pin.set_high()
        } else {
            pin.set_low()
        };
        sleep(TICK);
    }
}
