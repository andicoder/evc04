//! The MQTT control surface (docs/mqtt.md): parse the inbound target-current
//! command, publish the retained status object, and keep the broker link alive.
//!
//! The payload parser and the status schema are pure and unit-tested; the live
//! [`run_mqtt`] task is the I/O boundary (mirrors [`crate::slave::run_link`]) and
//! is exercised against a real broker, not in unit tests.

use crate::config::MqttConfig;
use crate::control::Controller;
use crate::slave::LinkHealth;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::Instant;

/// QoS 1 for both directions, fixed by the contract (docs/mqtt.md).
const QOS: QoS = QoS::AtLeastOnce;

/// MQTT keep-alive; the broker marks us dead (and fires the LWT) after ~1.5×.
const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// Republish status at least this often so `last_poll_age_s` keeps advancing for
/// Home Assistant even when no target/link transition forces a publish.
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// Stable client id; only one instance per broker is expected at a time.
const CLIENT_ID: &str = "evc04-charge";

/// Retained Last Will payload the broker publishes to the status topic if the
/// service disconnects ungracefully, flipping Home Assistant to offline without
/// any polling (docs/mqtt.md "Last Will and Testament").
pub const OFFLINE_PAYLOAD: &[u8] = br#"{"online":false}"#;

/// Why an inbound target payload was rejected. The message feeds `status.last_error`
/// so a controller bug is visible rather than silently changing the charge current.
#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    /// Body was not valid JSON, or `amps` was missing or not a number.
    #[error("malformed target payload (expected {{\"amps\": number}})")]
    Malformed,
    /// `amps` parsed but is not finite (e.g. an overflowing exponent).
    #[error("target amps is not finite")]
    NonFinite,
}

/// Inbound command shape. Additive fields are ignored so newer publishers stay
/// compatible with older service versions (docs/mqtt.md).
#[derive(Deserialize)]
struct TargetPayload {
    amps: f64,
}

/// Parse the inbound target charge current (amps) from a `{"amps": N}` payload.
///
/// The value is returned as-is: range clamping is the control math's job
/// ([`crate::reported_current`]), so over/under-range numbers are accepted here.
/// Only structurally invalid payloads — malformed JSON, missing/non-numeric
/// `amps`, or a non-finite value — are rejected; on rejection the last valid
/// target stays in effect (docs/mqtt.md).
pub fn parse_target(payload: &[u8]) -> Result<f32, TargetError> {
    let parsed: TargetPayload =
        serde_json::from_slice(payload).map_err(|_| TargetError::Malformed)?;
    if !parsed.amps.is_finite() {
        return Err(TargetError::NonFinite);
    }
    Ok(parsed.amps as f32)
}

/// Outbound retained status object (docs/mqtt.md "Outbound — status"). Field
/// names are the wire schema; do not rename without updating the contract and the
/// Home Assistant sensor.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub online: bool,
    pub target_ampere: f32,
    /// Last live measured current consumed (the closed-loop input, #22).
    pub measured_ampere: f32,
    /// Current soft-ramped offset, `= MAX_BOX_AMPERE − target` once settled (#24).
    pub offset_ampere: f32,
    pub reported_ampere: f32,
    pub last_poll_age_s: f32,
    pub gateway: String,
    pub mqtt: String,
    /// `true` while the offset is still soft-ramping toward its setpoint (#24).
    pub ramping: bool,
    pub failsafe: bool,
    /// The measurement-loss failsafe (#25): the live measured input went stale, so the
    /// closed loop is bypassed and we serve full charge. Independent of `failsafe`
    /// (the target-staleness failsafe); either can be active on its own.
    pub measurement_failsafe: bool,
    /// Age of the live measured input in seconds; a growing value signals the
    /// publisher/broker went quiet before the failsafe latches.
    pub measurement_age_s: f32,
    pub last_error: Option<String>,
}

impl Status {
    /// Serialise to the compact JSON published on the status topic.
    pub fn to_json(&self) -> String {
        // Infallible: every field is a plain scalar/string/null.
        serde_json::to_string(self).expect("status serialises")
    }
}

/// Aggregate the live daemon state into the retained status object (docs/mqtt.md).
///
/// The `mqtt` field is always `connected`: this only runs inside the live event
/// loop, and an ungraceful disconnect is reported by the broker via the LWT, not
/// by us. `gateway` maps the slave's [`LinkHealth`]; the target/reported/failsafe
/// fields come from the closed-loop [`Controller`]; `last_poll_age_s` is the time
/// since the slave last answered a poll (a growing value signals a dead bus).
pub fn assemble_status(
    controller: &Controller,
    gateway: LinkHealth,
    last_poll: Instant,
    last_error: Option<String>,
) -> Status {
    let gateway = match gateway {
        LinkHealth::Up => "connected",
        LinkHealth::Stalled => "reconnecting",
        LinkHealth::Down => "down",
    };
    Status {
        online: true,
        target_ampere: controller.effective_target().0,
        measured_ampere: controller.measured().0,
        offset_ampere: controller.offset().0,
        reported_ampere: controller.reported_frame()[0],
        last_poll_age_s: last_poll.elapsed().as_secs_f32(),
        gateway: gateway.to_string(),
        mqtt: "connected".to_string(),
        ramping: controller.ramping(),
        failsafe: controller.failsafe_active(),
        measurement_failsafe: controller.measurement_failsafe_active(),
        measurement_age_s: controller.measurement_age().as_secs_f32(),
        last_error,
    }
}

/// Keep the broker link alive (docs/mqtt.md §7/§8): subscribe to the target topic,
/// apply each valid command, and publish the retained status. rumqttc's event loop
/// reconnects on its own as long as we keep polling; we re-subscribe and re-publish
/// on every `ConnAck` so a reconnect restores both. Runs until the task is cancelled.
///
/// `apply_target` is the seam the control loop (#6) fills: it receives every parsed
/// command — `Ok(amps)` to adopt, `Err` to surface in `status.last_error` while
/// holding the last good value. `apply_measured` is the same seam for the live
/// measured current that closes the loop (#22); both inbound topics carry the
/// identical `{"amps": N}` shape, so they share [`parse_target`]. `status`
/// snapshots the live state to publish.
pub async fn run_mqtt(
    cfg: MqttConfig,
    apply_target: impl Fn(Result<f32, TargetError>),
    apply_measured: impl Fn(Result<f32, TargetError>),
    status: impl Fn() -> Status,
) {
    let mut opts = MqttOptions::new(CLIENT_ID, &cfg.host, cfg.port);
    opts.set_keep_alive(KEEP_ALIVE);
    if let (Some(user), Some(pass)) = (&cfg.user, &cfg.pass) {
        opts.set_credentials(user, pass);
    }
    opts.set_last_will(LastWill::new(&cfg.topic_status, OFFLINE_PAYLOAD, QOS, true));

    let (client, mut eventloop) = AsyncClient::new(opts, 16);
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);

    loop {
        tokio::select! {
            event = eventloop.poll() => match event {
                // (Re)connected: rumqttc does not replay subscriptions, so re-arm
                // the target subscription and republish status after every ConnAck.
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    let _ = client.subscribe(&cfg.topic_target, QOS).await;
                    let _ = client.subscribe(&cfg.topic_measured, QOS).await;
                    publish_status(&client, &cfg.topic_status, &status).await;
                }
                Ok(Event::Incoming(Packet::Publish(p))) if p.topic == cfg.topic_target => {
                    apply_target(parse_target(&p.payload));
                    publish_status(&client, &cfg.topic_status, &status).await;
                }
                Ok(Event::Incoming(Packet::Publish(p))) if p.topic == cfg.topic_measured => {
                    apply_measured(parse_target(&p.payload));
                    publish_status(&client, &cfg.topic_status, &status).await;
                }
                Ok(_) => {}
                // Broker dropped: the next poll reconnects; pause so we don't spin.
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            },
            _ = ticker.tick() => {
                publish_status(&client, &cfg.topic_status, &status).await;
            }
        }
    }
}

async fn publish_status(client: &AsyncClient, topic: &str, status: &impl Fn() -> Status) {
    let _ = client.publish(topic, QOS, true, status().to_json()).await;
}
