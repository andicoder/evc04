//! Central MQTT plane for the firmware (evc04#66/#86).
//!
//! Owns the broker client, every topic name, the inbound connection pump, and all
//! publishing. No other module touches the `EspMqttClient` or a topic string
//! directly — they call the typed methods here (`publish_telemetry`,
//! `publish_charge_status`, …) and receive decoded inbound work as [`InMsg`] over
//! the channel [`Mqtt::connect`] returns. This keeps QoS/retain decisions and the
//! topic namespace in exactly one place.
//!
//! ⚠️ esp-idf-svc's MQTT API (the event/connection split) is version-sensitive —
//! built against the pinned esp-idf-svc 0.52.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttConnection, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};
use evc04_cn28_core::charge::intake::{parse_ampere, parse_enable, parse_watt, IntakeError};
use evc04_cn28_core::probe::{baud, command};
use tracing::warn;

const TOPIC_CMD: &str = "evc04/cn28/cmd";
const TOPIC_BAUD: &str = "evc04/cn28/baud";
// OTA is a device-management concern that outlives the cn28 prober (it stays in
// use whatever firmware role this ESP takes later, #76), so it sits under its own
// durable `evc04/device/*` namespace rather than the prober's `cn28/*` topics.
const TOPIC_OTA: &str = "evc04/device/ota";
const TOPIC_OTA_STATUS: &str = "evc04/device/ota/status";
#[cfg(feature = "raw-debug")]
const TOPIC_RAW: &str = "evc04/cn28/raw";
#[cfg(feature = "raw-debug")]
const TOPIC_RAW_HEX: &str = "evc04/cn28/raw/hex";
#[cfg(feature = "raw-debug")]
const TOPIC_RAW_ASCII: &str = "evc04/cn28/raw/ascii";
/// Per-read accounting for the window on `raw` (#159): what each `read()` claimed
/// against how much of that it actually wrote. Separates a re-delivering read from
/// genuine box output, which the raw bytes alone cannot.
#[cfg(feature = "raw-debug")]
const TOPIC_RAW_READS: &str = "evc04/cn28/raw/reads";
/// Decoded telemetry snapshot (#66): the structured view over the raw frames,
/// retained so a late subscriber (Home Assistant) gets the latest values at once.
/// Public because the HA discovery config (built elsewhere) points its sensors here.
pub const TOPIC_TELEMETRY: &str = "evc04/cn28/telemetry";
const TOPIC_STATUS: &str = "evc04/cn28/status";
/// Build identity (#101): the running `git describe` and OTA slot, retained so an
/// operator can read which image is live without inferring it from the schema.
const TOPIC_VERSION: &str = "evc04/cn28/version";

// Meter-emulation control plane (#86). Device-scoped `evc04/charge/*` topics —
// they superseded the retired k3s daemon's `evc04/*` topics (milestone #65, §7) and
// are what evcc/HA now target. Mirrors docs/mqtt.md.
const TOPIC_CTRL_TARGET: &str = "evc04/charge/target";
/// Raw signed grid power (#136): `{"watt": N}`, negative = export, forwarded by HA
/// untouched. V4 uses only its cadence (liveness failsafe) — never the value.
const TOPIC_CTRL_GRID_POWER: &str = "evc04/charge/grid_power";
const TOPIC_CTRL_ENABLE: &str = "evc04/charge/enable";
const TOPIC_CTRL_STATUS: &str = "evc04/charge/status";
/// Measurement probe (#135 step 6): `{"ampere": N}` lifts the served meter answer
/// to `MAX + N` (bounded, auto-expiring) so the box's response just above its limit
/// can be measured. Diagnostic-only; never part of the evcc/HA control contract.
const TOPIC_CTRL_PROBE: &str = "evc04/charge/probe_over";

const MQTT_URL: &str = env!("MQTT_URL");

/// Work pushed from the MQTT connection pump to the worker loop. The pump decodes
/// the payload (so the loop never parses bytes); a control-plane variant carries
/// the parse result so the loop can apply a good value or surface a rejection.
pub enum InMsg {
    Connected,
    Probe(Vec<u8>),
    SetBaud(u32),
    Ota(String),
    Target(Result<f32, IntakeError>),
    GridPower(Result<f32, IntakeError>),
    Enable(Result<bool, IntakeError>),
    ProbeOver(Result<f32, IntakeError>),
}

/// Owns the broker client; all publishes go through its methods so the topic
/// names, QoS and retain flags live in one place.
pub struct Mqtt {
    client: EspMqttClient<'static>,
}

impl Mqtt {
    /// Connect to the broker, spawn the connection pump, and return the publisher
    /// handle plus the inbound channel.
    ///
    /// The single allowed LWT goes to the safety-relevant charge status: an
    /// ungraceful drop must tell an evcc/HA-managed controller the box went
    /// offline (#86). `cn28/status` keeps its retained `online` via the heartbeat
    /// the worker loop fires instead (it is a debug-prober liveness topic, not
    /// control-critical). The keepalive lets esp-mqtt auto-reconnect; each
    /// reconnect re-fires CONNECTED, which re-subscribes and republishes `online`.
    pub fn connect() -> Result<(Self, mpsc::Receiver<InMsg>)> {
        let lwt = LwtConfiguration {
            topic: TOPIC_CTRL_STATUS,
            payload: br#"{"online":false}"#,
            qos: QoS::AtLeastOnce,
            retain: true,
        };
        let config = MqttClientConfiguration {
            lwt: Some(lwt),
            keep_alive_interval: Some(Duration::from_secs(30)),
            reconnect_timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let (client, connection) =
            EspMqttClient::new(MQTT_URL, &config).context("mqtt connect")?;
        let (tx, rx) = mpsc::channel();
        spawn_connection_pump(connection, tx);
        Ok((Self { client }, rx))
    }

    /// Subscribe to every inbound topic (idempotent on reconnect).
    pub fn subscribe_all(&mut self) -> Result<()> {
        for topic in [
            TOPIC_CMD,
            TOPIC_BAUD,
            TOPIC_OTA,
            TOPIC_CTRL_TARGET,
            TOPIC_CTRL_GRID_POWER,
            TOPIC_CTRL_ENABLE,
            TOPIC_CTRL_PROBE,
        ] {
            self.client.subscribe(topic, QoS::AtLeastOnce)?;
        }
        Ok(())
    }

    /// Retained `online` liveness on the debug-prober status topic (heartbeat).
    pub fn publish_status_online(&mut self) -> Result<()> {
        self.client
            .publish(TOPIC_STATUS, QoS::AtLeastOnce, true, b"online")?;
        Ok(())
    }

    /// Retained charge-control status — the safety-relevant topic that also carries
    /// the LWT, so republishing it on connect overwrites a stale `offline`.
    pub fn publish_charge_status(&mut self, json: &str) -> Result<()> {
        self.client
            .publish(TOPIC_CTRL_STATUS, QoS::AtLeastOnce, true, json.as_bytes())?;
        Ok(())
    }

    /// Retained decoded telemetry snapshot.
    pub fn publish_telemetry(&mut self, json: &str) -> Result<()> {
        self.client
            .publish(TOPIC_TELEMETRY, QoS::AtLeastOnce, true, json.as_bytes())?;
        Ok(())
    }

    /// Retained build identity.
    pub fn publish_version(&mut self, json: &str) -> Result<()> {
        self.client
            .publish(TOPIC_VERSION, QoS::AtLeastOnce, true, json.as_bytes())?;
        Ok(())
    }

    /// Retained Home Assistant discovery configs (dynamic `homeassistant/…` topics).
    pub fn publish_discovery(&mut self, messages: Vec<(String, String)>) -> Result<()> {
        for (topic, payload) in messages {
            self.client
                .publish(&topic, QoS::AtLeastOnce, true, payload.as_bytes())?;
        }
        Ok(())
    }

    /// Non-retained baud-sweep result echoed on the status topic, so it never
    /// clobbers the retained online/offline liveness (or the LWT).
    pub fn publish_baud_result(&mut self, rate: u32, ok: bool) -> Result<()> {
        let msg = if ok {
            format!("baud {rate}")
        } else {
            format!("baud {rate} failed")
        };
        self.client
            .publish(TOPIC_STATUS, QoS::AtLeastOnce, false, msg.as_bytes())?;
        Ok(())
    }

    /// Non-retained OTA progress (`downloading` / `ok` / `failed …`).
    pub fn publish_ota_status(&mut self, status: &str) -> Result<()> {
        self.client
            .publish(TOPIC_OTA_STATUS, QoS::AtLeastOnce, false, status.as_bytes())?;
        Ok(())
    }

    /// Delete the retained OTA trigger (a zero-length retained publish removes it)
    /// so a stale URL can never re-fire an OTA on the next reconnect (#76).
    pub fn clear_ota_trigger(&mut self) -> Result<()> {
        self.client.publish(TOPIC_OTA, QoS::AtLeastOnce, true, b"")?;
        Ok(())
    }

    /// Non-retained raw capture views (capture/discovery debug only, #110).
    #[cfg(feature = "raw-debug")]
    pub fn publish_raw(&mut self, raw: &[u8], hex: &str, ascii: &str) -> Result<()> {
        self.client.publish(TOPIC_RAW, QoS::AtLeastOnce, false, raw)?;
        self.client
            .publish(TOPIC_RAW_HEX, QoS::AtLeastOnce, false, hex.as_bytes())?;
        self.client
            .publish(TOPIC_RAW_ASCII, QoS::AtLeastOnce, false, ascii.as_bytes())?;
        Ok(())
    }

    /// Non-retained per-read trace for the window just published (#159).
    #[cfg(feature = "raw-debug")]
    pub fn publish_read_trace(&mut self, json: &str) -> Result<()> {
        self.client
            .publish(TOPIC_RAW_READS, QoS::AtLeastOnce, false, json.as_bytes())?;
        Ok(())
    }
}

/// Pump the connection (it must be drained continuously or the client stalls),
/// decode each payload, and hand decoded work to the worker loop over `tx`.
fn spawn_connection_pump(mut connection: EspMqttConnection, tx: mpsc::Sender<InMsg>) {
    std::thread::Builder::new()
        .stack_size(6144)
        .spawn(move || {
            while let Ok(event) = connection.next() {
                match event.payload() {
                    EventPayload::Connected(_) => {
                        let _ = tx.send(InMsg::Connected);
                    }
                    EventPayload::Received { topic, data, .. } => {
                        let payload = core::str::from_utf8(data).unwrap_or_default();
                        // Route by topic: baud re-tunes the UART, ota triggers a
                        // firmware pull, control payloads decode to the plane, any
                        // other (the command channel) decodes to probe bytes.
                        match topic {
                            Some(t) if t == TOPIC_BAUD => match baud::parse_baud(payload) {
                                Ok(rate) => {
                                    let _ = tx.send(InMsg::SetBaud(rate));
                                }
                                Err(e) => warn!(payload, error = ?e, "rejected baud payload"),
                            },
                            Some(t) if t == TOPIC_OTA => {
                                // Ignore our own retained-clear (empty payload);
                                // forward every real trigger raw so the loop both
                                // validates it and deletes the retained message.
                                if !data.is_empty() {
                                    let _ = tx.send(InMsg::Ota(payload.to_string()));
                                }
                            }
                            Some(t) if t == TOPIC_CTRL_TARGET => {
                                let _ = tx.send(InMsg::Target(parse_ampere(payload)));
                            }
                            Some(t) if t == TOPIC_CTRL_GRID_POWER => {
                                let _ = tx.send(InMsg::GridPower(parse_watt(payload)));
                            }
                            Some(t) if t == TOPIC_CTRL_ENABLE => {
                                let _ = tx.send(InMsg::Enable(parse_enable(payload)));
                            }
                            Some(t) if t == TOPIC_CTRL_PROBE => {
                                let _ = tx.send(InMsg::ProbeOver(parse_ampere(payload)));
                            }
                            _ => match command::decode_command(payload) {
                                Ok(bytes) => {
                                    let _ = tx.send(InMsg::Probe(bytes));
                                }
                                Err(e) => warn!(payload, error = ?e, "rejected command payload"),
                            },
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("spawn mqtt pump");
}
