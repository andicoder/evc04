//! Firmware entry point for the in-box ESP32 (evc04#66/#85).
//!
//! `main` only wires the hardware and launches the two independent worker threads,
//! each its own routine in its own module:
//!   - [`prober`] — CN28 LOG remote prober over UART1 + MQTT/OTA (#66/#70/#76).
//!   - [`rs485`]  — PRO380 meter-emulation slave over UART2 + MAX3485 (#85).
//!
//! They share no state and run on separate threads, so the box's ~1 Hz meter poll
//! is answered regardless of what the prober is doing (#87 hardens this further).
//!
//! Build/flash (locally only — never CI; needs Espressif's Xtensa toolchain):
//!   ./bootstrap.sh                            # once: sysdeps + espup + cargo tools
//!   export WIFI_SSID=... WIFI_PASSWORD=... MQTT_URL=mqtt://user:pass@host:1883
//!   cd firmware && cargo make build           # native esp build → host ELF
//!   cargo make flash                          # flash + monitor on host (USB)

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::uart::{
    config::Config as UartConfig, config::DataBits, config::StopBits, UartDriver,
};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::error;

mod control;
mod prober;
mod rs485;
mod wifi;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // main owns the WiFi guard: it must outlive both workers, so it is held here
    // for the life of the process.
    let _wifi = wifi::connect(peripherals.modem, sysloop, nvs)?;

    // UART1 → CN28 LOG (9600 8N1). UART0 stays free for the USB log monitor.
    let cn28 = UartDriver::new(
        peripherals.uart1,
        peripherals.pins.gpio16, // TX → CN28 RX (RX/TX swapped in SW for the zero-byte LOG bring-up)
        peripherals.pins.gpio17, // RX ← CN28 TX (was GPIO16)
        Option::<gpio::AnyIOPin>::None,
        Option::<gpio::AnyIOPin>::None,
        &UartConfig::new().baudrate(Hertz(prober::CN28_BAUD)),
    )
    .context("cn28 uart init")?;

    // UART2 → MAX3485 on the RS485 meter bus (9600 8E1, EVEN parity — different from
    // CN28's UART1, independent controllers). DE direction is driven manually on
    // GPIO27 (see rs485.rs); RTS is left unused.
    let meter_uart = UartDriver::new(
        peripherals.uart2,
        peripherals.pins.gpio25,        // TX → MAX3485 DI
        peripherals.pins.gpio26,        // RX ← MAX3485 RO
        Option::<gpio::AnyIOPin>::None, // CTS unused
        Option::<gpio::AnyIOPin>::None, // RTS unused (manual DE on GPIO27)
        &UartConfig::new()
            .baudrate(Hertz(rs485::BAUD))
            .data_bits(DataBits::DataBits8)
            .parity_even()
            .stop_bits(StopBits::STOP1),
    )
    .context("rs485 uart init")?;
    let de = gpio::PinDriver::output(peripherals.pins.gpio27).context("rs485 DE pin")?;

    // Two independent routines, each on its own thread (same spawn pattern). The
    // RS485 slave must keep answering even if the prober exits, so neither blocks
    // the other and main outlives both.
    // Shared control state (#86): the prober thread runs the MQTT intake + ~1 Hz
    // control tick and writes the reported current; the RS485 slave reads it to
    // answer the box. Mutex, not a channel, so the slave always has a value to serve.
    let control = Arc::new(Mutex::new(control::ControlState::new()));

    std::thread::Builder::new()
        .stack_size(8192) // OTA (HTTP download + flash) runs on this thread (#76)
        .spawn({
            let control = Arc::clone(&control);
            move || {
                if let Err(e) = prober::run(cn28, control) {
                    error!("prober exited: {e:#}");
                }
            }
        })
        .expect("spawn cn28 prober");

    std::thread::Builder::new()
        .stack_size(6144)
        .spawn(move || rs485::run(meter_uart, de, control))
        .expect("spawn rs485 meter slave");

    // Keep the process — and the WiFi guard — alive; the workers run on their own
    // threads from here.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
