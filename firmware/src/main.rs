//! Firmware entry point for the in-box ESP32 (evc04#66/#85).
//!
//! `main` only wires the hardware and launches the two worker threads, each its own
//! routine in its own module:
//!   - [`probe`] — CN28 LOG worker over UART1: probes, telemetry, the ~1 Hz control
//!     tick, MQTT intake and MQTT-triggered OTA (#66/#70/#76/#86).
//!   - [`rs485`] — PRO380 meter-emulation slave over UART2 + TTL485 v2 (#85).
//!
//! Supporting modules: [`mqtt`] (broker client + connection pump), [`charge`] (the
//! control state plus the lock-free [`charge::Handoff`] the two threads share),
//! [`device`] (OTA/version/discovery), [`wifi`], [`logging`] (the `tracing` facade
//! and its OTLP export, #3).
//!
//! The two threads share only that lock-free handoff, so the box's ~1 Hz meter poll
//! is answered regardless of what the worker is doing (#87 hardens this further).
//!
//! Build/flash (locally only — never CI; needs Espressif's Xtensa toolchain):
//!   ./bootstrap.sh                            # once: sysdeps + espup + cargo tools
//!   export WIFI_SSID=... WIFI_PASSWORD=... MQTT_URL=mqtt://user:pass@host:1883
//!   export OTLP_LOGS_URL=http://collector.lan:4318/v1/logs
//!   cd firmware && cargo make build           # native esp build → host ELF
//!   cargo make flash                          # flash + monitor on host (USB)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::hal::task::watchdog::{TWDTConfig, TWDTDriver};
use esp_idf_svc::hal::uart::{
    config::Config as UartConfig, config::DataBits, config::StopBits, UartDriver,
};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use tracing::error;

mod charge;
mod device;
mod logging;
mod mqtt;
mod probe;
mod rs485;
mod wifi;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    // Logging comes up before anything else so the WiFi bring-up is itself on
    // the record (#3). The exporter simply drops batches until the link is
    // there; it never blocks the boot path.
    //
    // Deliberately NOT fatal: propagating an error here would return from `main`
    // before the RS485 slave thread exists, and a silent meter hard-faults the
    // wallbox to solid red (SPECS §7). Losing the logs is survivable; losing the
    // meter is not. `tracing` macros degrade to no-ops, and esp-idf's own output
    // still reaches the USB console.
    let _logger = logging::init()
        .inspect_err(|e| eprintln!("logging init failed, continuing without it: {e:#}"))
        .ok();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // main owns the WiFi guard: it must outlive both workers, so it is held here
    // for the life of the process. The NVS partition handle is cloneable (an Arc):
    // WiFi keeps one for its calibration store, the prober gets another for the
    // persisted charge setpoint (its own namespace, so the two never collide).
    let _wifi = wifi::connect(peripherals.modem, sysloop, nvs.clone())?;

    // Real wall-clock timestamps for the log records (#3). Held here like the
    // WiFi guard; syncing runs in the background, so the workers below — and
    // with them the box's meter poll — start on time either way.
    let _sntp = logging::start_clock_sync()?;

    // UART1 → CN28 LOG (9600 8N1). UART0 stays free for the USB log monitor.
    let cn28 = UartDriver::new(
        peripherals.uart1,
        peripherals.pins.gpio16, // TX → CN28 pin 3 (box RX)
        peripherals.pins.gpio17, // RX ← CN28 pin 2 (box TX)
        Option::<gpio::AnyIOPin>::None,
        Option::<gpio::AnyIOPin>::None,
        &UartConfig::new().baudrate(Hertz(probe::CN28_BAUD)),
    )
    .context("cn28 uart init")?;

    // UART2 → TTL485 v2 on the RS485 meter bus (9600 8E1, EVEN parity — different from
    // CN28's UART1, independent controllers). The module switches TX/RX itself, so
    // there is no DE line to drive; CTS/RTS are both unused.
    let meter_uart = UartDriver::new(
        peripherals.uart2,
        peripherals.pins.gpio25,        // TX → TTL485 RXD
        peripherals.pins.gpio26,        // RX ← TTL485 TXD
        Option::<gpio::AnyIOPin>::None, // CTS unused
        Option::<gpio::AnyIOPin>::None, // RTS unused (auto-direction module)
        &UartConfig::new()
            .baudrate(Hertz(rs485::BAUD))
            .data_bits(DataBits::DataBits8)
            .parity_even()
            .stop_bits(StopBits::STOP1),
    )
    .context("rs485 uart init")?;

    // Two independent routines, each on its own thread (same spawn pattern). The
    // RS485 slave must keep answering even if the prober exits, so neither blocks
    // the other and main outlives both.
    // Cross-thread hand-off (#86): the prober thread runs the MQTT intake + ~1 Hz
    // control tick (the Controller lives there, on its own thread) and writes the
    // reported current here; the RS485 slave reads it to answer the box and stamps
    // each poll. Two lock-free atomics, so the slave always has a value to serve and
    // never blocks on the worker.
    let handoff = Arc::new(charge::Handoff::new());

    // Task watchdog (#113): the prober subscribes its own task and feeds it each
    // loop; a hang longer than this reboots the chip. 60 s clears the longest
    // legitimate block (a bounded OTA download); panic_on_trigger turns the timeout
    // into a clean reset (surfaced as `reset_reason: task_watchdog` in telemetry).
    let twdt = TWDTDriver::new(
        peripherals.twdt,
        &TWDTConfig {
            duration: Duration::from_secs(60),
            panic_on_trigger: true,
            ..Default::default()
        },
    )
    .context("twdt init")?;

    std::thread::Builder::new()
        .stack_size(8192) // OTA (HTTP download + flash) runs on this thread (#76)
        .spawn({
            let handoff = Arc::clone(&handoff);
            move || {
                if let Err(e) = probe::run(cn28, handoff, twdt, nvs) {
                    error!(error = ?e, "prober exited");
                }
                // The prober is the device's whole job and now feeds production
                // telemetry; if its loop ever returns — a publish error on an MQTT
                // blip, a dropped channel — don't sit dead. Reboot to re-run the
                // full bring-up (#113).
                error!("prober loop ended — rebooting to recover (#113)");
                restart();
            }
        })
        .expect("spawn cn28 prober");

    std::thread::Builder::new()
        .stack_size(6144)
        .spawn(move || rs485::run(meter_uart, handoff))
        .expect("spawn rs485 meter slave");

    // Keep the process — and the WiFi guard — alive; the workers run on their own
    // threads from here.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
