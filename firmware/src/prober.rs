//! CN28 LOG remote prober (evc04#66/#70/#76).
//!
//! Read/explore only — no RS485, no control, no safety criticality. CN28's LOG
//! console is request/response: it emits nothing unprompted — a byte on its RX
//! triggers a burst of per-phase metering, temperature and detection lines. A
//! probe captures that response in a bounded window, so a window can begin or end
//! mid-line — even a token can straddle the boundary (reassembly + tolerant
//! decoding handle that, #98). This turns an MQTT command topic into those bytes
//! and republishes whatever comes back, so the shell surface can be probed live
//! without reflashing. It also owns MQTT-triggered OTA (#76).
//!
//! [`run`] is the thread routine `main` spawns; everything else is its internals.
//!
//! ⚠️ The esp-idf-svc API (MQTT event/connection split, OTA, HTTP) is
//! version-sensitive — built against the pinned esp-idf-svc 0.52.

use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::hal::uart::UartDriver;
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttConnection, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};
use esp_idf_svc::ota::{EspOta, SlotState};
use esp_idf_svc::sys::{esp_err_t, ESP_ERR_TIMEOUT};
use evc04_cn28_core::cn28::{Cn28Snapshot, LineReassembler};
use evc04_cn28_core::intake::{parse_ampere, parse_enable, IntakeError};
use evc04_cn28_core::discovery::{cn28_discovery_messages, DiscoveryMeta};
use evc04_cn28_core::version::{version_json, Version};
use evc04_cn28_core::{baud, command, dump, ota};
use log::{info, warn};

use crate::control::ControlState;

/// CN28 LOG UART rate: 9600 8N1, no flow control (bench bring-up #72 — the box's
/// LOG console runs at 9600, not the 115200 first assumed). `main` configures UART1
/// with it; `evc04/cn28/baud` can still re-tune it live for a future sweep.
pub const CN28_BAUD: u32 = 9_600;

const TOPIC_CMD: &str = "evc04/cn28/cmd";
const TOPIC_BAUD: &str = "evc04/cn28/baud";
// OTA is a device-management concern that outlives the cn28 prober (it stays in
// use whatever firmware role this ESP takes later, #76), so it sits under its own
// durable `evc04/device/*` namespace rather than the prober's `cn28/*` topics.
const TOPIC_OTA: &str = "evc04/device/ota";
const TOPIC_OTA_STATUS: &str = "evc04/device/ota/status";
const TOPIC_RAW: &str = "evc04/cn28/raw";
const TOPIC_RAW_HEX: &str = "evc04/cn28/raw/hex";
const TOPIC_RAW_ASCII: &str = "evc04/cn28/raw/ascii";
/// Decoded telemetry snapshot (#66): the structured view over the raw frames,
/// retained so a late subscriber (Home Assistant) gets the latest values at once.
const TOPIC_TELEMETRY: &str = "evc04/cn28/telemetry";
const TOPIC_STATUS: &str = "evc04/cn28/status";
/// Build identity (#101): the running `git describe` and OTA slot, retained so an
/// operator can read which image is live — and whether a freshly-OTA'd image is
/// still pending rollback verification — without inferring it from the schema.
const TOPIC_VERSION: &str = "evc04/cn28/version";
/// Baked at build time by `build.rs` (`git describe --tags --always --dirty`).
const FW_VERSION: &str = env!("FW_VERSION");

// Meter-emulation control plane (#86). Device-scoped `evc04/charge/*` so it does
// not collide with the k3s daemon's `evc04/*` topics while both run in parallel
// (the daemon stays production until this port is proven, milestone #65/§12);
// evcc/HA repoint here when the daemon is retired. Mirrors charge/docs/mqtt.md.
const TOPIC_CTRL_TARGET: &str = "evc04/charge/target";
const TOPIC_CTRL_MEASURED: &str = "evc04/charge/measured";
const TOPIC_CTRL_ENABLE: &str = "evc04/charge/enable";
const TOPIC_CTRL_STATUS: &str = "evc04/charge/status";

/// Auto-poll the LOG every N seconds (sends `\r\n`) so the telemetry refreshes for
/// HA/evcc without an external trigger; 0 = off. The box's own meter updates ~1 Hz,
/// so below ~1–2 s only adds empty windows. This reads the box's KLEFR meter — not
/// the grid meter, so it does not drive real-time load management (#98).
const AUTO_WAKE_SECS: u64 = 2;
/// Re-publish the retained `online` liveness this often. After a reboot the *new*
/// session can publish `online` before the broker fires the *old* session's
/// retained LWT `offline` (its will latency is ~keepalive×1.5), leaving the status
/// stuck `offline` while the device is up — seen right after an OTA. The heartbeat
/// re-asserts `online`, so any such stale `offline` self-corrects within one tick.
const STATUS_HEARTBEAT: Duration = Duration::from_secs(30);
/// Control-loop tick: ramp the offset and republish the retained charge status. ~1 Hz
/// matches the box's poll cadence and the daemon's control interval (#86).
const CONTROL_TICK: Duration = Duration::from_secs(1);
/// Per-byte read gap before a response is considered complete.
const READ_GAP: Duration = Duration::from_millis(200);
/// How long to wait for the *first* response byte before treating the line as
/// silent. Much longer than READ_GAP: a slow shell — or a slower baud mid-sweep
/// (#79) — can take far longer than the inter-byte gap to begin replying, and a
/// 200 ms first-byte window would drop those frames as "no response".
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(2);
const READ_BUF: usize = 512;
/// Chunk size for streaming the OTA image from HTTP into the inactive slot.
const OTA_BUF: usize = 1024;
/// Per-operation network timeout for the OTA HTTP transfer. Bounds connect and
/// each read so an unreachable or stalled image server fails fast instead of
/// blocking the prober thread forever — a hang there lapses the MQTT keepalive
/// and wedges the device until a power-cycle (observed #76/#101). A healthy LAN
/// server answers in milliseconds, so this only ever trips on a real fault.
const OTA_HTTP_TIMEOUT: Duration = Duration::from_secs(20);

const MQTT_URL: &str = env!("MQTT_URL");

/// Work pushed from the MQTT connection thread to the prober loop.
enum Job {
    Connected,
    Probe(Vec<u8>),
    SetBaud(u32),
    Ota(String),
    /// A control-plane input (#86): the parse result so the loop can apply a good
    /// value or surface a rejection in status — the decode lives in the pump.
    Target(Result<f32, IntakeError>),
    Measured(Result<f32, IntakeError>),
    Enable(Result<bool, IntakeError>),
}

/// Thread routine: connect to the broker, pump the connection, and serve probes /
/// baud changes / OTA forever. `uart` is the CN28 UART (`main` owns construction).
pub fn run(uart: UartDriver<'static>, control: Arc<Mutex<ControlState>>) -> Result<()> {
    // The single allowed LWT goes to the safety-relevant charge status: an
    // ungraceful drop must tell an evcc/HA-managed controller the box went offline
    // (#86). cn28/status keeps its retained `online` via the heartbeat instead (it
    // is a debug-prober liveness topic, not control-critical).
    let lwt = LwtConfiguration {
        topic: TOPIC_CTRL_STATUS,
        payload: br#"{"online":false}"#,
        qos: QoS::AtLeastOnce,
        retain: true,
    };
    let mqtt_config = MqttClientConfiguration {
        lwt: Some(lwt),
        // Detect a dropped link within the keepalive window and let esp-mqtt
        // auto-reconnect; each reconnect re-fires CONNECTED, which re-subscribes
        // and republishes `online` (see prober_loop), so the device self-heals
        // after a network blip. A brownout-induced *reset* is a hardware issue
        // this cannot fix — see #79.
        keep_alive_interval: Some(Duration::from_secs(30)),
        reconnect_timeout: Some(Duration::from_secs(5)),
        ..Default::default()
    };
    let (mut client, connection) =
        EspMqttClient::new(MQTT_URL, &mqtt_config).context("mqtt connect")?;

    // The connection must be pumped continuously or the client stalls. Decode
    // command payloads there, hand raw probe jobs to the prober loop.
    let (tx, rx) = mpsc::channel::<Job>();
    spawn_connection_pump(connection, tx);

    prober_loop(&mut client, &uart, rx, control)
}

fn prober_loop(
    client: &mut EspMqttClient<'_>,
    uart: &UartDriver<'_>,
    rx: mpsc::Receiver<Job>,
    control: Arc<Mutex<ControlState>>,
) -> Result<()> {
    let auto_wake = (AUTO_WAKE_SECS > 0).then(|| Duration::from_secs(AUTO_WAKE_SECS));
    let mut next_heartbeat = Instant::now() + STATUS_HEARTBEAT;
    let mut next_control = Instant::now() + CONTROL_TICK;
    let mut next_wake = auto_wake.map(|d| Instant::now() + d);
    // Accumulates the latest decoded LOG fields across probe windows so a
    // truncated window's gaps stay filled from earlier ones (#66).
    let mut telemetry = Cn28Snapshot::new();
    // Reassembles whole LOG lines from the byte stream: a probe's response is
    // captured in a bounded window, so a line — even a token — can split across
    // the boundary. Lives across windows so the tail of one joins the head of next.
    let mut reassembler = LineReassembler::new();

    loop {
        // Block for a job, but never longer than the soonest pending timer so the
        // control tick, the heartbeat (and optional auto-wake) still fire on an idle
        // connection.
        let mut deadline = next_heartbeat.min(next_control);
        if let Some(w) = next_wake {
            deadline = deadline.min(w);
        }
        let timeout = deadline.saturating_duration_since(Instant::now());

        match rx.recv_timeout(timeout) {
            Ok(Job::Connected) => {
                client.subscribe(TOPIC_CMD, QoS::AtLeastOnce)?;
                client.subscribe(TOPIC_BAUD, QoS::AtLeastOnce)?;
                client.subscribe(TOPIC_OTA, QoS::AtLeastOnce)?;
                client.subscribe(TOPIC_CTRL_TARGET, QoS::AtLeastOnce)?;
                client.subscribe(TOPIC_CTRL_MEASURED, QoS::AtLeastOnce)?;
                client.subscribe(TOPIC_CTRL_ENABLE, QoS::AtLeastOnce)?;
                client.publish(TOPIC_STATUS, QoS::AtLeastOnce, true, b"online")?;
                next_heartbeat = Instant::now() + STATUS_HEARTBEAT;
                // Republish charge status at once so it overwrites a stale LWT
                // `offline` from a previous session as soon as we are back up.
                publish_charge_status(client, &control)?;
                next_control = Instant::now() + CONTROL_TICK;
                info!("connected; subscribed to cn28 + charge control topics");
                // Announce the running build + slot (#101) *before* confirming, so
                // `pending_verify` still reflects the just-booted (unverified) state
                // — that is the signal an OTA actually took. Diagnostic only, so a
                // failure is logged, not propagated.
                if let Err(e) = publish_version(client) {
                    warn!("version: publish skipped: {e:#}");
                }
                // Register the telemetry sensors with Home Assistant (retained
                // discovery configs, #98). Idempotent on reconnect; non-fatal.
                if let Err(e) = publish_discovery(client) {
                    warn!("discovery: publish skipped: {e:#}");
                }
                // Reaching the broker is the proof a freshly-OTA'd image needs to
                // cancel its pending rollback (#76). A confirm failure must not
                // kill the loop, so it is logged, not propagated.
                if let Err(e) = confirm_running_slot() {
                    warn!("ota: confirm skipped: {e:#}");
                }
            }
            Ok(Job::Probe(bytes)) => probe(client, uart, &bytes, &mut telemetry, &mut reassembler)?,
            Ok(Job::SetBaud(rate)) => set_baud(client, uart, rate)?,
            Ok(Job::Ota(payload)) => run_ota(client, &payload)?,
            Ok(Job::Target(parsed)) => control.lock().unwrap().apply_target(parsed, Instant::now()),
            Ok(Job::Measured(parsed)) => control
                .lock()
                .unwrap()
                .apply_measured(parsed, Instant::now()),
            Ok(Job::Enable(parsed)) => control.lock().unwrap().apply_enable(parsed),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if now >= next_control {
                    publish_charge_status(client, &control)?;
                    next_control = now + CONTROL_TICK;
                }
                if now >= next_heartbeat {
                    client.publish(TOPIC_STATUS, QoS::AtLeastOnce, true, b"online")?;
                    next_heartbeat = now + STATUS_HEARTBEAT;
                }
                if let (Some(w), Some(d)) = (next_wake, auto_wake) {
                    if now >= w {
                        probe(client, uart, b"\r\n", &mut telemetry, &mut reassembler)?; // auto-wake tick
                        next_wake = Some(now + d);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Advance the control loop one tick and publish the retained charge status (#86).
fn publish_charge_status(
    client: &mut EspMqttClient<'_>,
    control: &Arc<Mutex<ControlState>>,
) -> Result<()> {
    let json = control.lock().unwrap().tick(Instant::now());
    client.publish(TOPIC_CTRL_STATUS, QoS::AtLeastOnce, true, json.as_bytes())?;
    Ok(())
}

/// Write probe bytes to CN28, drain the response, republish the three raw views,
/// and fold the decoded lines into the retained telemetry snapshot.
fn probe(
    client: &mut EspMqttClient<'_>,
    uart: &UartDriver<'_>,
    bytes: &[u8],
    telemetry: &mut Cn28Snapshot,
    reassembler: &mut LineReassembler,
) -> Result<()> {
    uart.write(bytes).context("uart write")?;

    // esp-idf-hal reports an elapsed read timeout as Err(ESP_ERR_TIMEOUT), not
    // Ok(0). A quiet line — the gap after a frame, or no response at all — is
    // exactly that timeout, so it means "drained", not "failed". Propagating it
    // would kill the prober loop on the first silent probe.
    let first = TickType::new_millis(FIRST_BYTE_TIMEOUT.as_millis() as u64).ticks();
    let gap = TickType::new_millis(READ_GAP.as_millis() as u64).ticks();
    let mut resp = Vec::new();
    let mut chunk = [0u8; READ_BUF];
    loop {
        // Wait FIRST_BYTE_TIMEOUT for the opening byte, then only READ_GAP
        // between bytes — so a slow/late reply still lands, but a finished frame
        // still returns promptly once the line goes quiet.
        let timeout = if resp.is_empty() { first } else { gap };
        match uart.read(&mut chunk, timeout) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&chunk[..n]),
            Err(e) if e.code() == ESP_ERR_TIMEOUT as esp_err_t => break,
            Err(e) => return Err(e).context("uart read"),
        }
    }

    client.publish(TOPIC_RAW, QoS::AtLeastOnce, false, &resp)?;
    client.publish(
        TOPIC_RAW_HEX,
        QoS::AtLeastOnce,
        false,
        dump::to_hex(&resp).as_bytes(),
    )?;
    client.publish(
        TOPIC_RAW_ASCII,
        QoS::AtLeastOnce,
        false,
        dump::to_printable(&resp).as_bytes(),
    )?;

    // Reassemble whole lines from this window's bytes (a line, or even a token,
    // can straddle window boundaries — the reassembler holds the partial tail) and
    // fold each into the running snapshot. Republish (retained) only when something
    // decoded. Unrecognised/garbled lines are skipped, so a partial window never
    // corrupts the object — it just contributes whatever whole lines it carried.
    let mut updated = false;
    for line in reassembler.push(&resp) {
        updated |= telemetry.apply_line(&line);
    }
    if updated {
        client.publish(
            TOPIC_TELEMETRY,
            QoS::AtLeastOnce,
            true,
            telemetry.to_json().as_bytes(),
        )?;
    }

    info!("probe {} B → {} B response", bytes.len(), resp.len());
    Ok(())
}

/// Re-tune the UART rate live for the baud sweep (#79). The result is echoed on
/// the status topic *non-retained*, so it never clobbers the retained
/// online/offline liveness (or the LWT).
fn set_baud(client: &mut EspMqttClient<'_>, uart: &UartDriver<'_>, rate: u32) -> Result<()> {
    match uart.change_baudrate(Hertz(rate)) {
        Ok(_) => {
            info!("uart baud set to {rate}");
            client.publish(
                TOPIC_STATUS,
                QoS::AtLeastOnce,
                false,
                format!("baud {rate}").as_bytes(),
            )?;
        }
        Err(e) => {
            warn!("uart baud {rate} rejected: {e}");
            client.publish(
                TOPIC_STATUS,
                QoS::AtLeastOnce,
                false,
                format!("baud {rate} failed").as_bytes(),
            )?;
        }
    }
    Ok(())
}

/// Pull a firmware image over plain HTTP and flash it to the inactive slot, then
/// reboot into it (#76). Runs on the prober thread, so the download blocks the
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
fn run_ota(client: &mut EspMqttClient<'_>, payload: &str) -> Result<()> {
    // Security (#76): a *retained* trigger would re-fire an OTA from this URL on
    // every reconnect — e.g. against a since-dead image server — a flash loop. So
    // the moment we act on any trigger, delete it (a zero-length retained publish
    // removes the retained message), guaranteeing no OTA URL can persist on the
    // broker regardless of who set it. The pump ignores the empty echo.
    client.publish(TOPIC_OTA, QoS::AtLeastOnce, true, b"")?;

    let url = match ota::validate_ota_url(payload) {
        Ok(url) => url,
        Err(e) => {
            warn!("bad ota url {payload:?}: {e:?}");
            client.publish(
                TOPIC_OTA_STATUS,
                QoS::AtLeastOnce,
                false,
                format!("failed {e:?}").as_bytes(),
            )?;
            return Ok(());
        }
    };

    client.publish(TOPIC_OTA_STATUS, QoS::AtLeastOnce, false, b"downloading")?;
    match download_and_flash(url) {
        Ok(total) => {
            info!("ota wrote {total} B; rebooting into the new slot");
            client.publish(TOPIC_OTA_STATUS, QoS::AtLeastOnce, false, b"ok")?;
            // Let the broker flush the status before the link drops on reboot.
            std::thread::sleep(Duration::from_millis(500));
            restart();
        }
        Err(e) => {
            warn!("ota failed: {e:#}");
            client.publish(
                TOPIC_OTA_STATUS,
                QoS::AtLeastOnce,
                false,
                format!("failed {e}").as_bytes(),
            )?;
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
fn confirm_running_slot() -> Result<()> {
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
fn publish_version(client: &mut EspMqttClient<'_>) -> Result<()> {
    let ota = EspOta::new().context("ota init")?;
    let slot = ota.get_running_slot().context("running slot")?;
    let label = format!("{}", slot.label);
    let json = version_json(&Version {
        fw: FW_VERSION,
        slot: &label,
        pending_verify: slot.state == SlotState::Unverified,
    });
    client.publish(TOPIC_VERSION, QoS::AtLeastOnce, true, json.as_bytes())?;
    Ok(())
}

/// Publish the Home Assistant MQTT discovery configs (retained) so the telemetry
/// sensors auto-register (#98). One HA device (`evc04`) shared with the charge
/// controller once it moves onto the ESP (#87). Idempotent across reconnects.
fn publish_discovery(client: &mut EspMqttClient<'_>) -> Result<()> {
    let meta = DiscoveryMeta {
        prefix: "homeassistant",
        node_id: "evc04_cn28",
        device_id: "evc04",
        device_name: "EVC04 CN28",
        device_model: "EVC04-AC11-T2P",
        sw_version: FW_VERSION,
        state_topic: TOPIC_TELEMETRY,
    };
    for (topic, payload) in cn28_discovery_messages(&meta) {
        client.publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())?;
    }
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
                    EventPayload::Received { topic, data, .. } => {
                        let payload = core::str::from_utf8(data).unwrap_or_default();
                        // Route by topic: baud re-tunes the UART, ota triggers a
                        // firmware pull, any other (the command channel) is decoded
                        // to probe bytes.
                        match topic {
                            Some(t) if t == TOPIC_BAUD => match baud::parse_baud(payload) {
                                Ok(rate) => {
                                    let _ = tx.send(Job::SetBaud(rate));
                                }
                                Err(e) => warn!("bad baud {payload:?}: {e:?}"),
                            },
                            Some(t) if t == TOPIC_OTA => {
                                // Ignore our own retained-clear (empty payload);
                                // forward every real trigger raw so run_ota both
                                // validates it and deletes the retained message,
                                // so no OTA URL can ever persist (#76).
                                if !data.is_empty() {
                                    let _ = tx.send(Job::Ota(payload.to_string()));
                                }
                            }
                            // Control plane (#86): forward the parse result; the loop
                            // applies a good value or surfaces a rejection in status.
                            Some(t) if t == TOPIC_CTRL_TARGET => {
                                let _ = tx.send(Job::Target(parse_ampere(payload)));
                            }
                            Some(t) if t == TOPIC_CTRL_MEASURED => {
                                let _ = tx.send(Job::Measured(parse_ampere(payload)));
                            }
                            Some(t) if t == TOPIC_CTRL_ENABLE => {
                                let _ = tx.send(Job::Enable(parse_enable(payload)));
                            }
                            _ => match command::decode_command(payload) {
                                Ok(bytes) => {
                                    let _ = tx.send(Job::Probe(bytes));
                                }
                                Err(e) => warn!("bad command {payload:?}: {e:?}"),
                            },
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn mqtt pump");
}
