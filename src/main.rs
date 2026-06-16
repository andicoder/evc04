//! The evc04-charge daemon (SPECS.md §7): emulate the Inepro PRO380 meter the
//! EVC04 polls, modulated by an MQTT target current.
//!
//! Three concerns, wired here and connected through the lock-free `watch` channels
//! the modules expose:
//! - the gateway link + Modbus slave ([`slave::run_link`]),
//! - the MQTT control surface ([`mqtt::run_mqtt`]),
//! - the control seam + failsafe ([`control::channel`]) bridging the two.

use evc04_charge::config::Config;
use evc04_charge::control;
use evc04_charge::mqtt::{assemble_status, run_mqtt};
use evc04_charge::slave::{run_link, LinkConfig, LinkHealth};
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

    // Serve the failsafe value until the first MQTT target arrives, so startup and
    // the grace window are already safe (SPECS.md §9).
    let (sink, view) = control::channel(
        cfg.fuse_limit_a,
        cfg.failsafe_target_a,
        cfg.failsafe_after,
        cfg.failsafe_target_a,
    );

    // Cross-subsystem signals the status publisher reads.
    let (gateway_tx, gateway_rx) = watch::channel(LinkHealth::Down);
    let (poll_tx, poll_rx) = watch::channel(Instant::now());
    let (error_tx, error_rx) = watch::channel::<Option<String>>(None);

    // `serve_connection` calls this exactly once per answered poll, so stamping the
    // poll time here records bus liveness without touching the slave's framing code.
    let slave_view = view.clone();
    let currents = move || {
        let _ = poll_tx.send(Instant::now());
        slave_view.currents()
    };
    tokio::spawn(run_link(
        cfg.gateway_addr(),
        LinkConfig {
            poll: cfg.poll,
            ..Default::default()
        },
        currents,
        gateway_tx,
    ));

    let apply = move |target| {
        let _ = error_tx.send(match &target {
            Ok(_) => None,
            Err(e) => Some(format!("{e}")),
        });
        sink.apply(target);
    };
    let status = move || {
        assemble_status(
            &view,
            *gateway_rx.borrow(),
            *poll_rx.borrow(),
            error_rx.borrow().clone(),
        )
    };

    // Runs until cancelled; the gateway link task runs alongside it.
    run_mqtt(cfg.mqtt, apply, status).await;
    ExitCode::SUCCESS
}
