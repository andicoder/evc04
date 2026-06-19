//! Startup configuration, loaded entirely from environment variables (SPECS.md
//! §7). No config files, no secrets baked into the image — the same image runs
//! at any installation. Values are validated here, at the process boundary; the
//! rest of the app trusts the resulting [`Config`].

use crate::slave::PollMatch;
use crate::Ampere;
use serde::Deserialize;
use std::time::Duration;

/// Sanity check on `MAX_BOX_AMPERE`: reject implausible ceilings. Domestic AC charging
/// tops out well below this; 80 A (the top of the EVC04 DIP table) leaves headroom while
/// still catching typos and unit mistakes (e.g. watts entered as amps).
const AMPERE_SANITY_LIMIT: f32 = 80.0;

/// Modbus addresses 1..=247 are assignable to a slave (0 is broadcast, 248..=255
/// reserved).
const SLAVE_ADDRESS_RANGE: std::ops::RangeInclusive<u8> = 1..=247;

/// Validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub gateway_host: String,
    pub gateway_port: u16,
    /// The box's own current ceiling, set by its DIP switches 4-5-6 (SPECS.md §6).
    /// Our reference for 100 % charge; not a fuse we protect.
    pub max_box_ampere: Ampere,
    pub mqtt: MqttConfig,
    pub poll: PollMatch,
    /// How long the last MQTT target stays valid before the full-charge failsafe
    /// engages (SPECS.md §9). Must exceed the controller's republish interval.
    pub target_timeout: Duration,
    /// Minimum charge current the closed loop attempts; below it the 3-phase floor
    /// (~6 A ≈ 4.1 kW) collapses to pause rather than holding a stable current
    /// (SPECS.md §6, issue #23). A target below this serves a hard pause.
    pub min_charge: Ampere,
    /// How long the last measured current stays valid before the measurement-loss
    /// failsafe engages (SPECS.md §9, issue #25). Serving `offset + stale_measured`
    /// would hold the box at the wrong current, so once stale we revert to full charge
    /// (the meterless-box default). Tighter than `target_timeout`: the measurement
    /// republishes faster (~3–6 s) than the target.
    pub measured_timeout: Duration,
    /// Soft-ramp slope for the offset, in amps per second (SPECS.md §6, issue #24). A
    /// step change of the setpoint shocks the box into over-throttling below the car's
    /// floor; rate-limiting the offset keeps the closed loop stable.
    pub ramp_rate: f32,
    /// Home Assistant MQTT discovery (issue #46): publish retained config topics so HA
    /// auto-creates the read-only status sensors.
    pub discovery: DiscoveryConfig,
    /// What to serve when the **target** input goes stale (issue #51). Default
    /// `full_charge` (the meterless-box baseline); an evcc-managed box wants `pause` so a
    /// control-path blip can't flip an intended pause into charging.
    pub target_failsafe: FailsafeMode,
    /// What to serve when the **measured** input goes stale (issue #51). Same modes;
    /// `pause` for evcc, `full_charge` by default.
    pub measured_failsafe: FailsafeMode,
}

/// Direction a staleness failsafe takes when an input ages out (issue #51).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailsafeMode {
    /// Serve `reported = 0` — the meterless-box default, "never worse than no tool"
    /// (SPECS §9). Correct for a Home-Assistant-automation-only setup.
    FullCharge,
    /// Keep serving the last commanded value through the loop (a stale pause stays a
    /// pause, a stale charge stays a charge).
    HoldLast,
    /// Serve the ceiling (zero headroom → the box pauses). The genuinely safe direction
    /// for an evcc-managed box: any control-path fault stops charging.
    Pause,
}

impl FailsafeMode {
    /// The forced per-phase report when this failsafe engages, or `None` for `hold_last`
    /// (serve the held value through the normal loop instead).
    pub fn forced_report(self, max_box: Ampere) -> Option<Ampere> {
        match self {
            FailsafeMode::FullCharge => Some(Ampere(0.0)),
            FailsafeMode::Pause => Some(max_box),
            FailsafeMode::HoldLast => None,
        }
    }

    fn parse(s: &str) -> Option<FailsafeMode> {
        match s {
            "full_charge" => Some(FailsafeMode::FullCharge),
            "hold_last" => Some(FailsafeMode::HoldLast),
            "pause" => Some(FailsafeMode::Pause),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            FailsafeMode::FullCharge => "full_charge",
            FailsafeMode::HoldLast => "hold_last",
            FailsafeMode::Pause => "pause",
        }
    }
}

/// Home Assistant MQTT discovery settings (issue #46). Opt-in so an upgrade never
/// sprays retained configs under the discovery prefix unasked.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    /// HA's discovery prefix (HA default `homeassistant`).
    pub prefix: String,
    /// Node id segment in the config topic + the device identifier — make it unique
    /// per install when several share a broker.
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub topic_target: String,
    pub topic_status: String,
    /// Inbound live measured per-phase current that closes the control loop
    /// (SPECS.md §6, issue #22). Source-agnostic: grid/total today, charger CT later.
    pub topic_measured: String,
}

impl Config {
    /// `host:port` form for dialling the gateway (accepted by `ToSocketAddrs`).
    pub fn gateway_addr(&self) -> String {
        format!("{}:{}", self.gateway_host, self.gateway_port)
    }

    /// One-line startup summary for the logs (issue #43). Lists the operationally
    /// relevant config so a deployment is self-documenting — **never the broker
    /// password**: `MQTT_PASS` is reported only as present/absent, never its value.
    pub fn log_summary(&self) -> String {
        format!(
            "gateway={} max_box={}A mqtt={}:{} auth={} target={:?} measured={:?} status={:?} \
             min_charge={}A ramp={}A/s target_timeout={}s measured_timeout={}s ha_discovery={} \
             target_failsafe={} measured_failsafe={}",
            self.gateway_addr(),
            self.max_box_ampere.0,
            self.mqtt.host,
            self.mqtt.port,
            if self.mqtt.pass.is_some() {
                "set"
            } else {
                "none"
            },
            self.mqtt.topic_target,
            self.mqtt.topic_measured,
            self.mqtt.topic_status,
            self.min_charge.0,
            self.ramp_rate,
            self.target_timeout.as_secs(),
            self.measured_timeout.as_secs(),
            if self.discovery.enabled {
                format!("{}/{}", self.discovery.prefix, self.discovery.node_id)
            } else {
                "off".to_string()
            },
            self.target_failsafe.as_str(),
            self.measured_failsafe.as_str(),
        )
    }

    /// Load and validate from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::from_vars(std::env::vars())
    }

    /// Load and validate from an explicit set of key/value pairs (testable seam).
    pub fn from_vars<I>(vars: I) -> Result<Config, ConfigError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let raw: RawConfig = envy::from_iter(vars)?;
        raw.validate()
    }
}

impl RawConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        let mut problems = Vec::new();

        if !(self.max_box_ampere.is_finite()
            && self.max_box_ampere > 0.0
            && self.max_box_ampere <= AMPERE_SANITY_LIMIT)
        {
            problems.push(format!(
                "MAX_BOX_AMPERE must be in (0, {AMPERE_SANITY_LIMIT}] A, got {}",
                self.max_box_ampere
            ));
        }
        if self.gateway_port == 0 {
            problems.push("GATEWAY_PORT must be 1..=65535".to_string());
        }
        if self.mqtt_port == 0 {
            problems.push("MQTT_PORT must be 1..=65535".to_string());
        }
        if !SLAVE_ADDRESS_RANGE.contains(&self.slave_address) {
            problems.push(format!(
                "SLAVE_ADDRESS must be {}..={}, got {}",
                SLAVE_ADDRESS_RANGE.start(),
                SLAVE_ADDRESS_RANGE.end(),
                self.slave_address
            ));
        }
        if self.poll_quantity == 0 {
            problems.push("POLL_QUANTITY must be greater than 0".to_string());
        }
        if self.target_timeout_seconds == 0 {
            problems.push("TARGET_TIMEOUT_SECONDS must be greater than 0".to_string());
        }
        if self.measured_timeout_seconds == 0 {
            problems.push("MEASURED_TIMEOUT_SECONDS must be greater than 0".to_string());
        }
        if !(self.ramp_rate_ampere_per_second.is_finite() && self.ramp_rate_ampere_per_second > 0.0)
        {
            problems.push(format!(
                "RAMP_RATE_AMPERE_PER_SECOND must be a finite value greater than 0, got {}",
                self.ramp_rate_ampere_per_second
            ));
        }
        if !(self.min_charge_ampere.is_finite()
            && self.min_charge_ampere > 0.0
            && self.min_charge_ampere <= self.max_box_ampere)
        {
            problems.push(format!(
                "MIN_CHARGE_AMPERE must be in (0, MAX_BOX_AMPERE={}] A, got {}",
                self.max_box_ampere, self.min_charge_ampere
            ));
        }

        let parse_failsafe = |var: &str, raw: &str, problems: &mut Vec<String>| {
            FailsafeMode::parse(raw).unwrap_or_else(|| {
                problems.push(format!(
                    "{var} must be one of full_charge|hold_last|pause, got {raw}"
                ));
                FailsafeMode::FullCharge
            })
        };
        let target_failsafe =
            parse_failsafe("TARGET_FAILSAFE", &self.target_failsafe, &mut problems);
        let measured_failsafe =
            parse_failsafe("MEASURED_FAILSAFE", &self.measured_failsafe, &mut problems);

        if !problems.is_empty() {
            return Err(ConfigError::Invalid(problems));
        }

        Ok(Config {
            gateway_host: self.gateway_host,
            gateway_port: self.gateway_port,
            max_box_ampere: Ampere(self.max_box_ampere),
            mqtt: MqttConfig {
                host: self.mqtt_host,
                port: self.mqtt_port,
                user: self.mqtt_user,
                pass: self.mqtt_pass,
                topic_target: self.mqtt_topic_target,
                topic_status: self.mqtt_topic_status,
                topic_measured: self.mqtt_topic_measured,
            },
            poll: PollMatch {
                addr: self.slave_address,
                register: self.poll_register,
                qty: self.poll_quantity,
            },
            target_timeout: Duration::from_secs(self.target_timeout_seconds),
            min_charge: Ampere(self.min_charge_ampere),
            measured_timeout: Duration::from_secs(self.measured_timeout_seconds),
            ramp_rate: self.ramp_rate_ampere_per_second,
            discovery: DiscoveryConfig {
                enabled: self.ha_discovery_enabled,
                prefix: self.ha_discovery_prefix,
                node_id: self.ha_discovery_node_id,
            },
            target_failsafe,
            measured_failsafe,
        })
    }
}

/// Flat env-mapped shape; envy lowercases each env key to match these fields.
#[derive(Debug, Deserialize)]
struct RawConfig {
    gateway_host: String,
    gateway_port: u16,
    max_box_ampere: f32,
    mqtt_host: String,
    mqtt_port: u16,
    mqtt_user: Option<String>,
    mqtt_pass: Option<String>,
    mqtt_topic_target: String,
    mqtt_topic_status: String,
    #[serde(default = "default_topic_measured")]
    mqtt_topic_measured: String,
    #[serde(default = "default_slave_address")]
    slave_address: u8,
    #[serde(default = "default_poll_register")]
    poll_register: u16,
    #[serde(default = "default_poll_quantity")]
    poll_quantity: u16,
    #[serde(default = "default_target_timeout_seconds")]
    target_timeout_seconds: u64,
    #[serde(default = "default_min_charge_ampere")]
    min_charge_ampere: f32,
    #[serde(default = "default_measured_timeout_seconds")]
    measured_timeout_seconds: u64,
    #[serde(default = "default_ramp_rate_ampere_per_second")]
    ramp_rate_ampere_per_second: f32,
    #[serde(default)]
    ha_discovery_enabled: bool,
    #[serde(default = "default_discovery_prefix")]
    ha_discovery_prefix: String,
    #[serde(default = "default_discovery_node_id")]
    ha_discovery_node_id: String,
    #[serde(default = "default_failsafe")]
    target_failsafe: String,
    #[serde(default = "default_failsafe")]
    measured_failsafe: String,
}

fn default_failsafe() -> String {
    "full_charge".to_string()
}

fn default_discovery_prefix() -> String {
    "homeassistant".to_string()
}

fn default_discovery_node_id() -> String {
    "evc04".to_string()
}

fn default_slave_address() -> u8 {
    1
}

fn default_poll_register() -> u16 {
    0x500C
}

fn default_poll_quantity() -> u16 {
    6
}

fn default_target_timeout_seconds() -> u64 {
    60
}

fn default_topic_measured() -> String {
    "evc04/measured".to_string()
}

/// 3-phase charging floor (~6 A ≈ 4.1 kW); below it the box can't hold a stable
/// current (SPECS.md §6), so it's the default minimum the closed loop attempts.
fn default_min_charge_ampere() -> f32 {
    6.0
}

/// Measurement republishes every ~3–6 s (SPECS.md §6), so ~2–3 missed updates before we
/// stop trusting the closed loop and revert to full charge (#25).
fn default_measured_timeout_seconds() -> u64 {
    15
}

/// Gentle default offset slope (A/s): on bench testing ~0.5 A/s extended the stable
/// range down to ~9 A without the box over-throttling on a step change (SPECS.md §6).
fn default_ramp_rate_ampere_per_second() -> f32 {
    0.5
}

/// Why a configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required var was missing or a value failed to parse to its type.
    #[error("{0}")]
    Env(#[from] envy::Error),
    /// Values parsed but fell outside their allowed range.
    #[error("invalid configuration: {}", .0.join("; "))]
    Invalid(Vec<String>),
}
