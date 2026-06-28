//! WiFi station bring-up. Shared device infra (the prober needs it for MQTT/OTA),
//! kept in its own module so `main` only orchestrates.
//!
//! Credentials are baked at build time (`env!`), never committed — export
//! WIFI_SSID / WIFI_PASSWORD before `cargo make build` (placeholders otherwise).

use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use evc04_cn28_core::backoff::capped_exponential;
use log::{error, info, warn};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Join attempts before giving up and rebooting to retry bring-up from clean (#103).
const JOIN_ATTEMPTS: u32 = 5;
/// Backoff between join attempts: 500 ms doubling, capped at 8 s.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// Connect to the configured AP and block until the interface is up. The returned
/// guard must stay alive for the link to persist, so `main` holds it for the life
/// of the process.
///
/// The join is retried with capped backoff: a single attempt that times out used
/// to return `Err` and let the whole app die, so a transient AP/RF hiccup bricked
/// the device until a manual power-cycle (#103). If every attempt fails we reboot
/// to retry bring-up from clean rather than sitting dark — this never returns `Err`
/// for a join failure.
pub fn connect(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<BlockingWifi<EspWifi<'static>>> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(nvs))?, sysloop)?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID
            .try_into()
            .map_err(|_| anyhow::anyhow!("ssid too long"))?,
        password: WIFI_PASSWORD
            .try_into()
            .map_err(|_| anyhow::anyhow!("password too long"))?,
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;
    wifi.start()?;

    for attempt in 0..JOIN_ATTEMPTS {
        match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
            Ok(()) => {
                info!("wifi up: {WIFI_SSID} (attempt {}/{JOIN_ATTEMPTS})", attempt + 1);
                return Ok(wifi);
            }
            Err(e) => {
                warn!("wifi join {}/{JOIN_ATTEMPTS} failed: {e}", attempt + 1);
                // Drop any half-open association so the next connect() starts clean.
                let _ = wifi.disconnect();
                if attempt + 1 < JOIN_ATTEMPTS {
                    sleep(capped_exponential(attempt, BACKOFF_BASE, BACKOFF_CAP));
                }
            }
        }
    }

    error!("wifi: all {JOIN_ATTEMPTS} join attempts failed; rebooting to retry clean");
    restart()
}
