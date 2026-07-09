//! WiFi station bring-up. Shared device infra (the prober needs it for MQTT/OTA),
//! kept in its own module so `main` only orchestrates.
//!
//! Credentials are baked at build time (`env!`), never committed — export
//! WIFI_SSID / WIFI_PASSWORD before `cargo make build` (placeholders otherwise).

use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use esp_idf_svc::eventloop::{EspSubscription, EspSystemEventLoop, System};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::netif::IpEvent;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{esp_wifi_connect, ESP_OK};
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi, WifiEvent,
};
use evc04_cn28_core::device::backoff::capped_exponential;
use log::{error, info, warn};

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

/// Join attempts before giving up and rebooting to retry bring-up from clean (#103).
const JOIN_ATTEMPTS: u32 = 5;
/// Backoff between join attempts: 500 ms doubling, capped at 8 s.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(8);
/// Consecutive lost-link events (no stable re-join in between) tolerated before we
/// stop re-associating and reboot (#103, mid-run). Reset once we hold an IP again.
const MAX_RECONNECT_FAILS: u32 = 10;
static RECONNECT_FAILS: AtomicU32 = AtomicU32::new(0);

/// Bring up the WiFi station and keep it up for the life of the process.
///
/// The *boot* join is retried with capped backoff and, if every attempt fails,
/// reboots rather than letting the app die (#103). Once up, an event watcher
/// re-associates on a *mid-run* link loss and reboots only if reconnection keeps
/// failing — esp-idf-svc does not auto-reconnect, so without this a later AP drop
/// left the box a silent zombie until a manual power-cycle. The returned
/// [`WifiGuard`] owns the driver and the watchers; `main` holds it.
pub fn connect(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<WifiGuard> {
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(modem, sysloop.clone(), Some(nvs))?,
        sysloop.clone(),
    )?;
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
    join_or_reboot(&mut wifi);

    // Up. Watch for a mid-run link loss and recover without a human power-cycle.
    RECONNECT_FAILS.store(0, Ordering::Relaxed);
    let link_lost = sysloop.subscribe::<WifiEvent, _>(|event| {
        if matches!(event, WifiEvent::StaDisconnected(_)) {
            let fails = RECONNECT_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
            if fails > MAX_RECONNECT_FAILS {
                error!("wifi: {fails} lost-link events without a stable re-join; rebooting");
                restart();
            }
            warn!("wifi link lost; re-associating ({fails}/{MAX_RECONNECT_FAILS})");
            // Re-associating from the event task is the documented STA_DISCONNECTED
            // recovery path; the matching `got ip` clears the tally.
            let rc = unsafe { esp_wifi_connect() };
            if rc != ESP_OK {
                warn!("esp_wifi_connect failed: {rc}");
            }
        }
    })?;
    let got_ip = sysloop.subscribe::<IpEvent, _>(|event| {
        if matches!(event, IpEvent::DhcpIpAssigned(_)) {
            RECONNECT_FAILS.store(0, Ordering::Relaxed);
            info!("wifi link restored (got ip)");
        }
    })?;

    Ok(WifiGuard {
        _wifi: wifi,
        _link_lost: link_lost,
        _got_ip: got_ip,
    })
}

/// Join the AP, retrying with capped backoff; reboot if every attempt fails so a
/// transient hiccup never bricks the device (#103). Returns only once connected.
fn join_or_reboot(wifi: &mut BlockingWifi<EspWifi<'static>>) {
    for attempt in 0..JOIN_ATTEMPTS {
        match wifi.connect().and_then(|()| wifi.wait_netif_up()) {
            Ok(()) => {
                info!("wifi up: {WIFI_SSID} (attempt {}/{JOIN_ATTEMPTS})", attempt + 1);
                return;
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
    restart();
}

/// Owns the WiFi driver and the link-loss watchers; all must outlive the link, so
/// `main` keeps this for the whole process.
pub struct WifiGuard {
    _wifi: BlockingWifi<EspWifi<'static>>,
    _link_lost: EspSubscription<'static, System>,
    _got_ip: EspSubscription<'static, System>,
}
