//! Device-management plumbing (evc04#76/#98/#101): MQTT-triggered OTA, the retained
//! build/identity publish, the Home Assistant discovery configs, and the reset-reason
//! string. These outlive whatever firmware role this ESP takes, so they sit apart
//! from the CN28 [`crate::probe`] loop. All publishing goes through [`crate::mqtt`].
//!
//! ⚠️ The esp-idf-svc API (OTA, HTTP) is version-sensitive — built against the
//! pinned esp-idf-svc 0.52.

use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::hal::reset::{restart, ResetReason};
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::ota::{EspOta, SlotState};
use esp_idf_svc::sys::esp_timer_get_time;
use evc04_cn28_core::device::discovery::{cn28_discovery_messages, DiscoveryMeta};
use evc04_cn28_core::device::ota;
use evc04_cn28_core::device::version::{version_json, Version};
use log::{info, warn};

use crate::mqtt::Mqtt;

/// Baked at build time by `build.rs` (`git describe --tags --always --dirty`).
const FW_VERSION: &str = env!("FW_VERSION");

/// Chunk size for streaming the OTA image from HTTP into the inactive slot.
const OTA_BUF: usize = 1024;
/// Per-operation network timeout for the OTA HTTP transfer. Bounds connect and
/// each read so an unreachable or stalled image server fails fast instead of
/// blocking the worker thread forever — a hang there lapses the MQTT keepalive
/// and wedges the device until a power-cycle (observed #76/#101). A healthy LAN
/// server answers in milliseconds, so this only ever trips on a real fault.
const OTA_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Pull a firmware image over plain HTTP and flash it to the inactive slot, then
/// reboot into it (#76). Runs on the worker thread, so the download blocks the
/// loop; a stalled transfer lapses the MQTT keepalive and the broker drops us
/// (observed: an unreachable server wedged the device until a power-cycle). The
/// HTTP client therefore carries a per-operation timeout ([`OTA_HTTP_TIMEOUT`])
/// so a fault fails fast and the loop recovers. Probe responsiveness is
/// irrelevant during a flash. Progress is reported on the status topic so a
/// rollout is observable.
///
/// A failure must never propagate: the running (good) image is untouched, so we
/// publish `failed …` on the OTA status topic and carry on rather than killing
/// the loop.
pub fn run_ota(mqtt: &mut Mqtt, payload: &str) -> Result<()> {
    // Security (#76): a *retained* trigger would re-fire an OTA from this URL on
    // every reconnect — e.g. against a since-dead image server — a flash loop. So
    // the moment we act on any trigger, delete it, guaranteeing no OTA URL can
    // persist on the broker regardless of who set it. The pump ignores the empty echo.
    mqtt.clear_ota_trigger()?;

    let url = match ota::validate_ota_url(payload) {
        Ok(url) => url,
        Err(e) => {
            warn!("bad ota url {payload:?}: {e:?}");
            mqtt.publish_ota_status(&format!("failed {e:?}"))?;
            return Ok(());
        }
    };

    mqtt.publish_ota_status("downloading")?;
    // Light the OTA pattern for the duration of the flash (#123). Success reboots, so
    // it only needs clearing on the failure path.
    crate::led::set_ota(true);
    match download_and_flash(url) {
        Ok(total) => {
            info!("ota wrote {total} B; rebooting into the new slot");
            mqtt.publish_ota_status("ok")?;
            // Let the broker flush the status before the link drops on reboot.
            std::thread::sleep(Duration::from_millis(500));
            restart();
        }
        Err(e) => {
            crate::led::set_ota(false);
            warn!("ota failed: {e:#}");
            mqtt.publish_ota_status(&format!("failed {e}"))?;
            Ok(())
        }
    }
}

/// Stream `url` into the inactive OTA slot, returning the byte count written.
/// On any error the half-written `EspOtaUpdate` is dropped, which aborts it, so
/// the bootable slot is never corrupted.
fn download_and_flash(url: &str) -> Result<usize> {
    let mut http = EspHttpConnection::new(&HttpConfig {
        buffer_size: Some(OTA_BUF),
        timeout: Some(OTA_HTTP_TIMEOUT),
        ..Default::default()
    })
    .context("http client init")?;
    http.initiate_request(Method::Get, url, &[])
        .context("http GET")?;
    http.initiate_response().context("http response")?;
    let status = http.status();
    if status != 200 {
        anyhow::bail!("http status {status}");
    }

    let mut ota = EspOta::new().context("ota init")?;
    let mut update = ota.initiate_update().context("ota begin")?;
    let mut buf = [0u8; OTA_BUF];
    let mut total = 0usize;
    loop {
        let n = http.read(&mut buf).context("http read")?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n]).context("ota write")?;
        total += n;
    }
    if total == 0 {
        anyhow::bail!("empty image");
    }
    update.complete().context("ota complete")?;
    Ok(total)
}

/// Confirm-after-proof: a just-OTA'd image boots *unverified* (pending-verify).
/// Cancel the rollback only once — guarded by the slot state — so a re-fired
/// CONNECTED on a later reconnect is a no-op. An image that never reaches here
/// (no WiFi/MQTT) stays unverified and the bootloader reverts on the next reset.
pub fn confirm_running_slot() -> Result<()> {
    let mut ota = EspOta::new().context("ota init")?;
    let slot = ota.get_running_slot().context("running slot")?;
    if slot.state == SlotState::Unverified {
        ota.mark_running_slot_valid().context("mark slot valid")?;
        info!("ota: confirmed running slot {}", slot.label);
    }
    Ok(())
}

/// Publish the retained build identity (#101): the baked `git describe` and the
/// running OTA slot, with `pending_verify` true while the image is still unverified
/// (a fresh OTA that has not yet confirmed). Called before [`confirm_running_slot`]
/// so that signal survives.
pub fn publish_version(mqtt: &mut Mqtt) -> Result<()> {
    let ota = EspOta::new().context("ota init")?;
    let slot = ota.get_running_slot().context("running slot")?;
    let label = format!("{}", slot.label);
    let uptime_s = (unsafe { esp_timer_get_time() } / 1_000_000) as u64;
    let json = version_json(&Version {
        fw: FW_VERSION,
        slot: &label,
        pending_verify: slot.state == SlotState::Unverified,
        reset_reason: reset_reason_str(),
        uptime_s,
    });
    mqtt.publish_version(&json)
}

/// The esp-idf reset reason as a short stable string for telemetry (#113).
fn reset_reason_str() -> &'static str {
    match ResetReason::get() {
        ResetReason::Software => "software",
        ResetReason::Panic => "panic",
        ResetReason::TaskWatchdog => "task_watchdog",
        ResetReason::InterruptWatchdog => "int_watchdog",
        ResetReason::Watchdog => "watchdog",
        ResetReason::PowerOn => "power_on",
        ResetReason::ExternalPin => "external_pin",
        ResetReason::Brownout => "brownout",
        ResetReason::DeepSleep => "deep_sleep",
        _ => "other",
    }
}

/// Publish the Home Assistant MQTT discovery configs (retained) so the telemetry
/// sensors auto-register (#98). One HA device (`evc04`) shared with the charge
/// controller once it moves onto the ESP (#87). Idempotent across reconnects.
pub fn publish_discovery(mqtt: &mut Mqtt) -> Result<()> {
    let meta = DiscoveryMeta {
        prefix: "homeassistant",
        node_id: "evc04_cn28",
        device_id: "evc04",
        device_name: "EVC04 CN28",
        device_model: "EVC04-AC11-T2P",
        sw_version: FW_VERSION,
        state_topic: crate::mqtt::TOPIC_TELEMETRY,
    };
    mqtt.publish_discovery(cn28_discovery_messages(&meta))
}
