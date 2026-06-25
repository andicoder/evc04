//! WiFi station bring-up. Shared device infra (the prober needs it for MQTT/OTA),
//! kept in its own module so `main` only orchestrates.
//!
//! Credentials are baked at build time (`env!`), never committed — export
//! WIFI_SSID / WIFI_PASSWORD before `cargo make build` (placeholders otherwise).

use anyhow::Result;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::info;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Connect to the configured AP and block until the interface is up. The returned
/// guard must stay alive for the link to persist, so `main` holds it for the life
/// of the process.
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
    wifi.connect()?;
    wifi.wait_netif_up()?;
    info!("wifi up: {WIFI_SSID}");
    Ok(wifi)
}
