//! CN28 LOG remote prober (evc04#66/#70).
//!
//! Read/explore only — no RS485, no control, no safety criticality. CN28 is
//! strictly request/response: the box sends nothing on its own, but any byte on
//! its RX triggers exactly one ASCII response frame. This firmware turns an MQTT
//! command topic into those bytes and republishes whatever comes back, so the
//! shell surface can be probed live without reflashing.
//!
//! Build via the pinned esp toolchain Docker image (reproducible on any machine):
//!   cargo install cargo-make                  # once (or run ./bootstrap.sh)
//!   export WIFI_SSID=... WIFI_PASSWORD=... MQTT_URL=mqtt://user:pass@host:1883
//!   cd firmware && cargo make build-image     # compiles in Docker → host ELF
//!   cargo make flash                          # flash + monitor on host (USB)
//!
//! Native (no Docker) alternative — needs Espressif's Xtensa toolchain on the host:
//!   ./bootstrap.sh && . $HOME/export-esp.sh   # once / per build shell
//!   cargo run                                 # builds, flashes, monitors
//!
//! Secrets come from these build-time env vars (baked by `env!`), never from a
//! committed file.
//!
//! ⚠️ The esp-idf-svc API (MQTT event/connection split, `UartDriver::new`,
//! `BlockingWifi`) is version-sensitive — verify every call below against the
//! versions Cargo actually resolves on the first real build; expect drift.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::uart::{config::Config as UartConfig, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttConnection, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use evc04_cn28_core::{command, dump};
use log::{info, warn};

// ── Config (compile-time constants; secrets stay in env) ────────────────────
const TOPIC_CMD: &str = "evc04/cn28/cmd";
const TOPIC_RAW: &str = "evc04/cn28/raw";
const TOPIC_RAW_HEX: &str = "evc04/cn28/raw/hex";
const TOPIC_RAW_ASCII: &str = "evc04/cn28/raw/ascii";
const TOPIC_STATUS: &str = "evc04/cn28/status";

const UART_BAUD: u32 = 115_200; // CN28 LOG: 115200 8N1, no flow control.
/// Send `\r\n` every N seconds so frames are captured with no command. 0 = off.
const AUTO_WAKE_SECS: u64 = 0;
/// Per-byte read gap before a response is considered complete.
const READ_GAP: Duration = Duration::from_millis(200);
const READ_BUF: usize = 512;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const MQTT_URL: &str = env!("MQTT_URL");

/// Work pushed from the MQTT connection thread to the prober loop.
enum Job {
    Connected,
    Probe(Vec<u8>),
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let _wifi = connect_wifi(peripherals.modem, sysloop, nvs)?;

    // UART1 on a spare pin pair — UART0 stays free for the USB log monitor.
    let uart = UartDriver::new(
        peripherals.uart1,
        peripherals.pins.gpio17, // TX → CN28 RX
        peripherals.pins.gpio16, // RX ← CN28 TX
        Option::<gpio::AnyIOPin>::None,
        Option::<gpio::AnyIOPin>::None,
        &UartConfig::new().baudrate(Hertz(UART_BAUD)),
    )
    .context("uart init")?;

    let lwt = LwtConfiguration {
        topic: TOPIC_STATUS,
        payload: b"offline",
        qos: QoS::AtLeastOnce,
        retain: true,
    };
    let mqtt_config = MqttClientConfiguration {
        lwt: Some(lwt),
        ..Default::default()
    };
    let (mut client, connection) = EspMqttClient::new(MQTT_URL, &mqtt_config).context("mqtt connect")?;

    // The connection must be pumped continuously or the client stalls. Decode
    // command payloads here, hand raw probe jobs to the prober loop.
    let (tx, rx) = mpsc::channel::<Job>();
    spawn_connection_pump(connection, tx);

    prober_loop(&mut client, &uart, rx)
}

fn prober_loop(client: &mut EspMqttClient<'_>, uart: &UartDriver<'_>, rx: mpsc::Receiver<Job>) -> Result<()> {
    let wake = if AUTO_WAKE_SECS > 0 {
        Some(Duration::from_secs(AUTO_WAKE_SECS))
    } else {
        None
    };

    loop {
        let job = match wake {
            Some(d) => match rx.recv_timeout(d) {
                Ok(job) => Some(job),
                Err(mpsc::RecvTimeoutError::Timeout) => None, // → auto-wake
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(job) => Some(job),
                Err(_) => break,
            },
        };

        match job {
            Some(Job::Connected) => {
                client.subscribe(TOPIC_CMD, QoS::AtLeastOnce)?;
                client.publish(TOPIC_STATUS, QoS::AtLeastOnce, true, b"online")?;
                info!("connected; subscribed to {TOPIC_CMD}");
            }
            Some(Job::Probe(bytes)) => probe(client, uart, &bytes)?,
            None => probe(client, uart, b"\r\n")?, // auto-wake tick
        }
    }
    Ok(())
}

/// Write probe bytes to CN28, drain the response, republish the three views.
fn probe(client: &mut EspMqttClient<'_>, uart: &UartDriver<'_>, bytes: &[u8]) -> Result<()> {
    uart.write(bytes).context("uart write")?;

    let mut resp = Vec::new();
    let mut chunk = [0u8; READ_BUF];
    loop {
        let n = uart.read(&mut chunk, TickType::new_millis(READ_GAP.as_millis() as u64).ticks())?;
        if n == 0 {
            break; // inter-byte gap elapsed → frame complete
        }
        resp.extend_from_slice(&chunk[..n]);
    }

    client.publish(TOPIC_RAW, QoS::AtLeastOnce, false, &resp)?;
    client.publish(TOPIC_RAW_HEX, QoS::AtLeastOnce, false, dump::to_hex(&resp).as_bytes())?;
    client.publish(TOPIC_RAW_ASCII, QoS::AtLeastOnce, false, dump::to_printable(&resp).as_bytes())?;
    info!("probe {} B → {} B response", bytes.len(), resp.len());
    Ok(())
}

fn spawn_connection_pump(mut connection: EspMqttConnection, tx: mpsc::Sender<Job>) {
    std::thread::Builder::new()
        .stack_size(6144)
        .spawn(move || {
            while let Ok(event) = connection.next() {
                match event.payload() {
                    EventPayload::Connected(_) => {
                        let _ = tx.send(Job::Connected);
                    }
                    EventPayload::Received { data, .. } => {
                        let payload = core::str::from_utf8(data).unwrap_or_default();
                        match command::decode_command(payload) {
                            Ok(bytes) => {
                                let _ = tx.send(Job::Probe(bytes));
                            }
                            Err(e) => warn!("bad command {payload:?}: {e:?}"),
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn mqtt pump");
}

fn connect_wifi(
    modem: esp_idf_svc::hal::modem::Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<BlockingWifi<EspWifi<'static>>> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().map_err(|_| anyhow::anyhow!("ssid too long"))?,
        password: WIFI_PASSWORD.try_into().map_err(|_| anyhow::anyhow!("password too long"))?,
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    info!("wifi up: {WIFI_SSID}");
    Ok(wifi)
}
