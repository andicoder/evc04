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
use std::process::ExitCode;
use tokio::sync::watch;
use tokio::time::Instant;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("evc04-charge: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Serve full charge (report 0 A) until the first MQTT target arrives, so startup
    // and the grace window match the meterless-box default (SPECS.md §9).
    let (sink, view) = control::channel(cfg.max_box_ampere, cfg.failsafe_after);

    // The second inbound channel that closes the loop (#22): the live measured
    // current the box's draw rises into. Held at 0 A until the first publish; once it
    // goes stale the controller reverts to full charge (#25).
    let (measured_sink, measured_view) =
        control::measurement_channel(Ampere(0.0), cfg.meas_stale_timeout);

    // Join target + measurement into the closed-loop answer the slave serves (#23).
    let controller = control::Controller::new(view, measured_view, cfg.min_charge);

    // Cross-subsystem signals the status publisher reads.
    let (gateway_tx, gateway_rx) = watch::channel(LinkHealth::Down);
    let (poll_tx, poll_rx) = watch::channel(Instant::now());
    let (error_tx, error_rx) = watch::channel::<Option<String>>(None);

    // `serve_connection` calls this exactly once per answered poll, so stamping the
    // poll time here records bus liveness without touching the slave's framing code.
    let slave_controller = controller.clone();
    let reported_frame = move || {
        let _ = poll_tx.send(Instant::now());
        slave_controller.reported_frame()
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
        let _ = target_error_tx.send(match &target {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        sink.apply(target);
    };
    let apply_measured = move |measured| {
        let _ = error_tx.send(match &measured {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        measured_sink.apply(measured);
    };
    let status = move || {
        assemble_status(
            &controller,
            *gateway_rx.borrow(),
            *poll_rx.borrow(),
            error_rx.borrow().clone(),
        )
    };

    // Runs until cancelled; the gateway link task runs alongside it.
    run_mqtt(cfg.mqtt, apply, apply_measured, status).await;
    ExitCode::SUCCESS
}
