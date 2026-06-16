//! Startup configuration, loaded entirely from environment variables (SPECS.md
//! §7). No config files, no secrets baked into the image — the same image runs
//! at any installation. Values are validated here, at the process boundary; the
//! rest of the app trusts the resulting [`Config`].

use crate::slave::PollMatch;
use serde::Deserialize;

/// Reject implausible fuse limits. Domestic AC charging tops out well below this;
/// 80 A leaves headroom while still catching typos and unit mistakes.
const MAX_FUSE_A: f32 = 80.0;

/// Modbus addresses 1..=247 are assignable to a slave (0 is broadcast, 248..=255
/// reserved).
const SLAVE_ADDR_RANGE: std::ops::RangeInclusive<u8> = 1..=247;

/// Validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub gateway_host: String,
    pub gateway_port: u16,
    pub fuse_limit_a: f32,
    pub mqtt: MqttConfig,
    pub poll: PollMatch,
    /// Target charge current to serve if the MQTT command goes stale (SPECS.md §9).
    pub failsafe_target_a: f32,
}

#[derive(Debug, Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub topic_target: String,
    pub topic_status: String,
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

        if !(self.fuse_limit_a.is_finite()
            && self.fuse_limit_a > 0.0
            && self.fuse_limit_a <= MAX_FUSE_A)
        {
            problems.push(format!(
                "FUSE_LIMIT_A must be in (0, {MAX_FUSE_A}] A, got {}",
                self.fuse_limit_a
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
        // Failsafe is a charge target, so it cannot exceed the fuse headroom.
        if !(self.failsafe_target_a.is_finite()
            && self.failsafe_target_a >= 0.0
            && self.failsafe_target_a <= self.fuse_limit_a)
        {
            problems.push(format!(
                "FAILSAFE_TARGET_A must be in [0, FUSE_LIMIT_A], got {}",
                self.failsafe_target_a
            ));
        }

        if !problems.is_empty() {
            return Err(ConfigError::Invalid(problems));
        }

        Ok(Config {
            gateway_host: self.gateway_host,
            gateway_port: self.gateway_port,
            fuse_limit_a: self.fuse_limit_a,
            mqtt: MqttConfig {
                host: self.mqtt_host,
                port: self.mqtt_port,
                user: self.mqtt_user,
                pass: self.mqtt_pass,
                topic_target: self.mqtt_topic_target,
                topic_status: self.mqtt_topic_status,
            },
            poll: PollMatch {
                addr: self.slave_addr,
                register: self.poll_register,
                qty: self.poll_qty,
            },
            failsafe_target_a: self.failsafe_target_a,
        })
    }
}

/// Flat env-mapped shape; envy lowercases each env key to match these fields.
#[derive(Debug, Deserialize)]
struct RawConfig {
    gateway_host: String,
    gateway_port: u16,
    fuse_limit_a: f32,
    mqtt_host: String,
    mqtt_port: u16,
    mqtt_user: Option<String>,
    mqtt_pass: Option<String>,
    mqtt_topic_target: String,
    mqtt_topic_status: String,
    #[serde(default = "default_slave_addr")]
    slave_addr: u8,
    #[serde(default = "default_poll_register")]
    poll_register: u16,
    #[serde(default = "default_poll_qty")]
    poll_qty: u16,
    failsafe_target_a: f32,
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

/// Why a configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// A required var was missing or a value failed to parse to its type.
    Env(envy::Error),
    /// Values parsed but fell outside their allowed range.
    Invalid(Vec<String>),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Env(e) => write!(f, "{e}"),
            ConfigError::Invalid(problems) => {
                write!(f, "invalid configuration: {}", problems.join("; "))
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<envy::Error> for ConfigError {
    fn from(e: envy::Error) -> Self {
        ConfigError::Env(e)
    }
}
