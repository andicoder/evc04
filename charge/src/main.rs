//! The evc04-charge daemon (SPECS.md §7): emulate the Inepro PRO380 meter the
//! EVC04 polls, modulated by an MQTT target current.
//!
//! Three concerns, wired here and connected through the lock-free `watch` channels
//! the modules expose:
//! - the gateway link + Modbus slave ([`slave::run_link`]),
//! - the MQTT control surface ([`mqtt::run_mqtt`]),
//! - the control seam + failsafe ([`control::channel`]) bridging the two.

use evc04_charge::config::Config;
use evc04_charge::mqtt::{assemble_status, run_mqtt};
use evc04_charge::slave::{run_link, LinkConfig, LinkHealth};
use evc04_charge::{control, Ampere};
use std::cell::Cell;
use std::process::ExitCode;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing_subscriber::EnvFilter;

/// Soft-ramp tick (#24): advance the offset ~1 Hz, matching the box's poll cadence.
const RAMP_TICK: Duration = Duration::from_secs(1);

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Logging (#43): level via RUST_LOG (e.g. `info`, `evc04_charge=debug,rumqttc=warn`),
    // defaulting to `info`. The 1 Hz poll path logs at `trace`, so `info` stays quiet.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("configuration error: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("starting evc04-charge: {}", cfg.log_summary());

    // Pause the box until the first MQTT target arrives (#59): a cold start is un-commanded,
    // not a request to charge full, so we hold it paused through the grace window rather than
    // opening at full charge at the worst time. Past the window the configured failsafe governs.
    let (sink, view) = control::channel(cfg.max_box_ampere, cfg.target_timeout);

    // The second inbound channel that closes the loop (#22): the live measured
    // current the box's draw rises into. Held at 0 A until the first publish; once it
    // goes stale the controller reverts to full charge (#25).
    let (measured_sink, measured_view) =
        control::measurement_channel(Ampere(0.0), cfg.measured_timeout);

    // Soft-ramp the offset toward `max − target` so a step change of the setpoint reaches
    // the box gradually instead of shocking its loop below the car's floor (#24). The
    // driver reads the live target and writes the offset the slave serves.
    let (offset_tx, offset_view) = control::offset_channel(Ampere(0.0));
    tokio::spawn(control::run_ramp(
        view.clone(),
        cfg.ramp_rate,
        RAMP_TICK,
        offset_tx,
    ));

    // Optional enable gate (#60): an on/off override layered on top of the target so evcc's
    // `enable` and `maxcurrent` stop racing on one topic. Defaults to `true` (honor the
    // target); a retained `enable:false` is restored by the broker on reconnect.
    let (enable_sink, enable_view) = control::enable_channel(true);

    // Join target + measurement + ramped offset + enable gate into the answer the slave
    // serves (#23/#24/#60). Each staleness failsafe takes its configured direction (#51).
    let controller = control::Controller::new(
        view,
        measured_view,
        offset_view,
        enable_view,
        cfg.min_charge,
        cfg.pause_margin,
        cfg.target_failsafe,
        cfg.measured_failsafe,
    );

    // Cross-subsystem signals the status publisher reads.
    let (gateway_tx, gateway_rx) = watch::channel(LinkHealth::Down);
    let (poll_tx, poll_rx) = watch::channel(Instant::now());
    let (error_tx, error_rx) = watch::channel::<Option<String>>(None);

    // `serve_connection` calls this exactly once per answered poll, so stamping the
    // poll time here records bus liveness without touching the slave's framing code.
    let slave_controller = controller.clone();
    let reported_frame = move || {
        let _ = poll_tx.send(Instant::now());
        let frame = slave_controller.reported_frame();
        tracing::trace!("answered poll: reported {} A/phase", frame[0]);
        frame
    };
    tokio::spawn(run_link(
        cfg.gateway_addr(),
        LinkConfig {
            poll: cfg.poll,
            ..Default::default()
        },
        reported_frame,
        gateway_tx,
    ));

    let target_error_tx = error_tx.clone();
    let apply = move |target| {
        match &target {
            Ok(amps) => tracing::debug!("target accepted: {amps} A"),
            Err(e) => tracing::warn!("target rejected: {e}"),
        }
        let _ = target_error_tx.send(match &target {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        sink.apply(target);
    };
    let measured_error_tx = error_tx.clone();
    let apply_measured = move |measured| {
        match &measured {
            Ok(amps) => tracing::debug!("measured accepted: {amps} A"),
            Err(e) => tracing::warn!("measured rejected: {e}"),
        }
        let _ = measured_error_tx.send(match &measured {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        measured_sink.apply(measured);
    };
    let apply_enable = move |enable| {
        match &enable {
            Ok(on) => tracing::debug!("enable gate set: {on}"),
            Err(e) => tracing::warn!("enable rejected: {e}"),
        }
        let _ = error_tx.send(match &enable {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        enable_sink.apply(enable);
    };

    // Log the headline control edges (#43): both staleness failsafes entering/leaving
    // full charge, and the charge starting/stopping. Edge-detected so the steady state
    // is silent. `Cell` is fine — this closure runs only on the single-threaded runtime.
    let prev_failsafe = Cell::new(false);
    let prev_measurement_failsafe = Cell::new(false);
    let prev_charging = Cell::new(false);
    // Tracks the enable gate (#60); starts open to match the default so the first close logs.
    let prev_enabled = Cell::new(true);
    let status = move || {
        let s = assemble_status(
            &controller,
            *gateway_rx.borrow(),
            *poll_rx.borrow(),
            error_rx.borrow().clone(),
        );
        if s.failsafe != prev_failsafe.replace(s.failsafe) {
            if s.failsafe {
                tracing::warn!(
                    "target stale: failing over to {} (reported {} A/phase)",
                    controller.target_failsafe_mode().as_str(),
                    s.reported_ampere
                );
            } else {
                tracing::info!("target fresh again: resuming control");
            }
        }
        if s.measurement_failsafe != prev_measurement_failsafe.replace(s.measurement_failsafe) {
            if s.measurement_failsafe {
                tracing::warn!(
                    "measurement stale: abandoning closed loop, {} (reported {} A/phase)",
                    controller.measured_failsafe_mode().as_str(),
                    s.reported_ampere
                );
            } else {
                tracing::info!("measurement fresh again: resuming closed loop");
            }
        }
        if s.enabled != prev_enabled.replace(s.enabled) {
            if s.enabled {
                tracing::info!("enable gate opened: honoring the target");
            } else {
                tracing::warn!(
                    "enable gate closed: pausing the box (reported {} A/phase)",
                    s.reported_ampere
                );
            }
        }
        let charging = s.charge_state == "C";
        if charging != prev_charging.replace(charging) {
            tracing::info!(
                "charge state {} (reported {} A/phase)",
                s.charge_state,
                s.reported_ampere
            );
        }
        s
    };

    // HA MQTT discovery configs to publish (retained) on connect (#46); empty when
    // discovery is disabled (opt-in via HA_DISCOVERY_ENABLED).
    let discovery =
        evc04_charge::discovery::discovery_messages(&cfg.discovery, &cfg.mqtt.topic_status);

    // Runs until cancelled; the gateway link task runs alongside it.
    run_mqtt(
        cfg.mqtt,
        discovery,
        apply,
        apply_measured,
        apply_enable,
        status,
    )
    .await;
    ExitCode::SUCCESS
}
