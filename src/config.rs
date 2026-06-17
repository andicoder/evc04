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
const SLAVE_ADDR_RANGE: std::ops::RangeInclusive<u8> = 1..=247;

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
    pub failsafe_after: Duration,
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
        if !SLAVE_ADDR_RANGE.contains(&self.slave_addr) {
            problems.push(format!(
                "SLAVE_ADDR must be {}..={}, got {}",
                SLAVE_ADDR_RANGE.start(),
                SLAVE_ADDR_RANGE.end(),
                self.slave_addr
            ));
        }
        if self.poll_qty == 0 {
            problems.push("POLL_QTY must be greater than 0".to_string());
        }
        if self.failsafe_after_s == 0 {
            problems.push("FAILSAFE_AFTER_S must be greater than 0".to_string());
        }

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
                addr: self.slave_addr,
                register: self.poll_register,
                qty: self.poll_qty,
            },
            failsafe_after: Duration::from_secs(self.failsafe_after_s),
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
    #[serde(default = "default_slave_addr")]
    slave_addr: u8,
    #[serde(default = "default_poll_register")]
    poll_register: u16,
    #[serde(default = "default_poll_qty")]
    poll_qty: u16,
    #[serde(default = "default_failsafe_after_s")]
    failsafe_after_s: u64,
}

fn default_slave_addr() -> u8 {
    1
}

fn default_poll_register() -> u16 {
    0x500C
}

fn default_poll_qty() -> u16 {
    6
}

fn default_failsafe_after_s() -> u64 {
    60
}

fn default_topic_measured() -> String {
    "evc04/measured".to_string()
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
