//! CN28 LOG prober — the worker thread (evc04#66/#70).
//!
//! Read/explore only — no RS485, no safety criticality. CN28's LOG console is
//! request/response: it emits nothing unprompted — a byte on its RX triggers a
//! burst of per-phase metering, temperature and detection lines. A probe captures
//! that response in a bounded window, so a window can begin or end mid-line — even
//! a token can straddle the boundary (reassembly + tolerant decoding handle that,
//! #98). This turns an MQTT command topic into those bytes and republishes whatever
//! comes back, so the shell surface can be probed live without reflashing.
//!
//! This module is the worker loop: it drives the timers (control tick, status
//! heartbeat, auto-wake), serves probes and baud changes, and runs the ~1 Hz
//! control tick. The MQTT transport lives in [`crate::mqtt`]; device-management
//! work (OTA, version, discovery) lives in [`crate::device`]; this loop calls both.
//!
//! [`run`] is the thread routine `main` spawns; everything else is its internals.

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::task::watchdog::{TWDTDriver, WatchdogSubscription};
use esp_idf_svc::hal::uart::UartDriver;
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::{esp_err_t, esp_timer_get_time, ESP_ERR_TIMEOUT};
use evc04_cn28_core::probe::cn28::{Cn28Snapshot, LineReassembler};
#[cfg(feature = "raw-debug")]
use evc04_cn28_core::debug::dump;
use log::{info, warn};

use crate::charge::{Controller, Handoff};
use crate::device;
use crate::mqtt::{InMsg, Mqtt};

/// CN28 LOG UART rate: 9600 8N1, no flow control (bench bring-up #72 — the box's
/// LOG console runs at 9600, not the 115200 first assumed). `main` configures UART1
/// with it; `evc04/cn28/baud` can still re-tune it live for a future sweep.
pub const CN28_BAUD: u32 = 9_600;

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

/// Thread routine: connect to the broker, pump the connection, and serve probes /
/// baud changes / OTA forever. `uart` is the CN28 UART (`main` owns construction).
pub fn run(
    uart: UartDriver<'static>,
    handoff: Arc<Handoff>,
    mut twdt: TWDTDriver<'static>,
) -> Result<()> {
    let (mut mqtt, rx) = Mqtt::connect()?;

    // Watch this (the worker) task with the hardware watchdog (#113); the loop feeds
    // it every iteration, so a hang reboots the chip.
    let mut wdt = twdt.watch_current_task().context("twdt subscribe")?;
    worker_loop(&mut mqtt, &uart, rx, &handoff, &mut wdt)
}

fn worker_loop(
    mqtt: &mut Mqtt,
    uart: &UartDriver<'_>,
    rx: mpsc::Receiver<InMsg>,
    handoff: &Handoff,
    wdt: &mut WatchdogSubscription<'_>,
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
    // The control state lives only on this thread; only its computed `reported`
    // crosses to the slave, via `handoff`.
    let mut controller = Controller::new();

    loop {
        // Feed the task watchdog (#113): every loop turn proves we are alive; a
        // hang past the watchdog timeout reboots the chip.
        let _ = wdt.feed();
        // Block for a job, but never longer than the soonest pending timer so the
        // control tick, the heartbeat (and optional auto-wake) still fire on an idle
        // connection.
        let mut deadline = next_heartbeat.min(next_control);
        if let Some(w) = next_wake {
            deadline = deadline.min(w);
        }
        let timeout = deadline.saturating_duration_since(Instant::now());

        match rx.recv_timeout(timeout) {
            Ok(InMsg::Connected) => {
                mqtt.subscribe_all()?;
                mqtt.publish_status_online()?;
                next_heartbeat = Instant::now() + STATUS_HEARTBEAT;
                // Republish charge status at once so it overwrites a stale LWT
                // `offline` from a previous session as soon as we are back up.
                control_tick(mqtt, &mut controller, handoff)?;
                next_control = Instant::now() + CONTROL_TICK;
                info!("connected; subscribed to cn28 + charge control topics");
                // Announce the running build + slot (#101) *before* confirming, so
                // `pending_verify` still reflects the just-booted (unverified) state
                // — that is the signal an OTA actually took. Diagnostic only, so a
                // failure is logged, not propagated.
                if let Err(e) = device::publish_version(mqtt) {
                    warn!("version: publish skipped: {e:#}");
                }
                // Register the telemetry sensors with Home Assistant (retained
                // discovery configs, #98). Idempotent on reconnect; non-fatal.
                if let Err(e) = device::publish_discovery(mqtt) {
                    warn!("discovery: publish skipped: {e:#}");
                }
                // Reaching the broker is the proof a freshly-OTA'd image needs to
                // cancel its pending rollback (#76). A confirm failure must not
                // kill the loop, so it is logged, not propagated.
                if let Err(e) = device::confirm_running_slot() {
                    warn!("ota: confirm skipped: {e:#}");
                }
            }
            Ok(InMsg::Probe(bytes)) => {
                probe(mqtt, uart, &bytes, &mut telemetry, &mut reassembler)?
            }
            Ok(InMsg::SetBaud(rate)) => set_baud(mqtt, uart, rate)?,
            Ok(InMsg::Ota(payload)) => device::run_ota(mqtt, &payload)?,
            Ok(InMsg::Target(parsed)) => controller.apply_target(parsed, Instant::now()),
            Ok(InMsg::Measured(parsed)) => controller.apply_measured(parsed, Instant::now()),
            Ok(InMsg::Enable(parsed)) => controller.apply_enable(parsed),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if now >= next_control {
                    control_tick(mqtt, &mut controller, handoff)?;
                    next_control = now + CONTROL_TICK;
                }
                if now >= next_heartbeat {
                    mqtt.publish_status_online()?;
                    next_heartbeat = now + STATUS_HEARTBEAT;
                }
                if let (Some(w), Some(d)) = (next_wake, auto_wake) {
                    if now >= w {
                        probe(mqtt, uart, b"\r\n", &mut telemetry, &mut reassembler)?; // auto-wake tick
                        next_wake = Some(now + d);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Advance the control loop one tick: read the slave-stamped poll liveness from the
/// handoff, tick the controller, hand the new per-phase current back to the slave via
/// the handoff, and publish the retained charge status (#86).
fn control_tick(mqtt: &mut Mqtt, controller: &mut Controller, handoff: &Handoff) -> Result<()> {
    let now_ms = (unsafe { esp_timer_get_time() } / 1000) as u32;
    let last_poll_age_s = now_ms.wrapping_sub(handoff.last_poll_ms()) as f32 / 1000.0;
    let tick = controller.tick(Instant::now(), last_poll_age_s);
    handoff.set_reported(tick.reported);
    mqtt.publish_charge_status(&tick.status_json)
}

/// Write probe bytes to CN28, drain the response, republish the raw views (debug
/// only), and fold the decoded lines into the retained telemetry snapshot.
fn probe(
    mqtt: &mut Mqtt,
    uart: &UartDriver<'_>,
    bytes: &[u8],
    telemetry: &mut Cn28Snapshot,
    reassembler: &mut LineReassembler,
) -> Result<()> {
    uart.write(bytes).context("uart write")?;

    // esp-idf-hal reports an elapsed read timeout as Err(ESP_ERR_TIMEOUT), not
    // Ok(0). A quiet line — the gap after a frame, or no response at all — is
    // exactly that timeout, so it means "drained", not "failed". Propagating it
    // would kill the worker loop on the first silent probe.
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

    // Raw views are capture/discovery debug only — compiled out of production
    // builds so the box does not spray three extra publishes per auto-poll (#110).
    #[cfg(feature = "raw-debug")]
    mqtt.publish_raw(&resp, &dump::to_hex(&resp), &dump::to_printable(&resp))?;

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
        mqtt.publish_telemetry(&telemetry.to_json())?;
    }

    info!("probe {} B → {} B response", bytes.len(), resp.len());
    Ok(())
}

/// Re-tune the UART rate live for the baud sweep (#79). The result is echoed on
/// the status topic *non-retained*, so it never clobbers the retained
/// online/offline liveness (or the LWT).
fn set_baud(mqtt: &mut Mqtt, uart: &UartDriver<'_>, rate: u32) -> Result<()> {
    match uart.change_baudrate(Hertz(rate)) {
        Ok(_) => {
            info!("uart baud set to {rate}");
            mqtt.publish_baud_result(rate, true)?;
        }
        Err(e) => {
            warn!("uart baud {rate} rejected: {e}");
            mqtt.publish_baud_result(rate, false)?;
        }
    }
    Ok(())
}
