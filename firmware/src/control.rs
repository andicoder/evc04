//! On-box control-loop state for the meter emulation (evc04#86).
//!
//! Holds the MQTT control inputs (target / measured / enable and their arrival
//! times), owns the clock, soft-ramps the offset, and each tick computes the
//! per-poll reported current with the **host-tested** `core` control math — so the
//! ESP serves the exact value the k3s daemon would (no second implementation that
//! could drift). Shared `Arc<Mutex<ControlState>>` between the prober thread (which
//! drives the ~1 s tick and publishes status) and the RS485 slave (which reads
//! `reported` to answer the box and stamps each poll).
//!
//! Config mirrors the daemon's env defaults (`charge/src/config.rs`); `MAX_BOX` is
//! the box's DIP-set ceiling for this install. Failsafe direction is `pause` on
//! both channels — a stale input STOPS an evcc/HA-managed box, never starts it
//! (SPECS §9, #52).

use std::time::{Duration, Instant};

use evc04_cn28_core::charge::control::{
    ramp_step, reported_current, Ampere, ControlInputs, FailsafeMode,
};
use evc04_cn28_core::charge::intake::IntakeError;
use evc04_cn28_core::charge::status::{charge_state, status_json, Status};

const MAX_BOX_AMPERE: f32 = 16.0;
const MIN_CHARGE_AMPERE: f32 = 6.0;
const PAUSE_MARGIN_AMPERE: f32 = 4.0;
const RAMP_RATE_AMPERE_PER_S: f32 = 0.5;
const TARGET_TIMEOUT: Duration = Duration::from_secs(60);
const MEASURED_TIMEOUT: Duration = Duration::from_secs(15);
const TARGET_FAILSAFE: FailsafeMode = FailsafeMode::Pause;
const MEASURED_FAILSAFE: FailsafeMode = FailsafeMode::Pause;

/// Live control state. Constructed paused (reported above the ceiling) so the RS485
/// slave serves a safe value before the first tick or before any command lands.
pub struct ControlState {
    target: Option<f32>,
    target_at: Option<Instant>,
    measured: f32,
    measured_at: Instant,
    enabled: bool,
    offset: f32,
    reported: f32,
    ramping: bool,
    last_poll_at: Instant,
    last_tick: Instant,
    last_error: Option<String>,
}

impl ControlState {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            target: None,
            target_at: None,
            // Start with a fresh "measured 0" (like the daemon) so the loop isn't
            // instantly in the measurement failsafe; it ages out if nothing arrives.
            measured: 0.0,
            measured_at: now,
            enabled: true,
            offset: MAX_BOX_AMPERE,
            // Safe default until the first tick: report a pause (above the ceiling).
            reported: MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE,
            ramping: false,
            last_poll_at: now,
            last_tick: now,
            last_error: None,
        }
    }

    /// Per-phase current the RS485 slave should report this poll.
    pub fn reported(&self) -> f32 {
        self.reported
    }

    /// Stamp the moment the box polled us (drives `last_poll_age_s`; a growing value
    /// signals a dead RS485 link).
    pub fn note_poll(&mut self, now: Instant) {
        self.last_poll_at = now;
    }

    /// Apply a parsed target. A good value clears `last_error`; a rejected payload is
    /// held (last good stays in effect) and surfaced.
    pub fn apply_target(&mut self, parsed: Result<f32, IntakeError>, now: Instant) {
        match parsed {
            Ok(v) => {
                self.target = Some(v);
                self.target_at = Some(now);
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad target: {e:?}")),
        }
    }

    pub fn apply_measured(&mut self, parsed: Result<f32, IntakeError>, now: Instant) {
        match parsed {
            Ok(v) => {
                self.measured = v;
                self.measured_at = now;
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad measured: {e:?}")),
        }
    }

    pub fn apply_enable(&mut self, parsed: Result<bool, IntakeError>) {
        match parsed {
            Ok(b) => {
                self.enabled = b;
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad enable: {e:?}")),
        }
    }

    /// Advance the loop one tick: soft-ramp the offset toward `MAX_BOX − target`,
    /// compute the reported current via `core`, store it for the slave, and return
    /// the retained status JSON to publish.
    pub fn tick(&mut self, now: Instant) -> String {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;

        // Ramp only once a target has landed; before that the cold-start pause in
        // `reported_current` governs regardless of the offset.
        if let Some(target) = self.target {
            let setpoint = Ampere(MAX_BOX_AMPERE - target);
            let max_step = Ampere(RAMP_RATE_AMPERE_PER_S * dt);
            let next = ramp_step(Ampere(self.offset), setpoint, max_step);
            self.ramping = next.0 != setpoint.0;
            self.offset = next.0;
        }

        let target_stale = self
            .target_at
            .is_some_and(|t| now.saturating_duration_since(t) > TARGET_TIMEOUT);
        let measured_stale = now.saturating_duration_since(self.measured_at) > MEASURED_TIMEOUT;

        let inputs = ControlInputs {
            max: Ampere(MAX_BOX_AMPERE),
            min_charge: Ampere(MIN_CHARGE_AMPERE),
            pause_margin: Ampere(PAUSE_MARGIN_AMPERE),
            target: self.target.map(Ampere),
            offset: Ampere(self.offset),
            measured: Ampere(self.measured),
            enabled: self.enabled,
            target_stale,
            measured_stale,
            target_failsafe: TARGET_FAILSAFE,
            measured_failsafe: MEASURED_FAILSAFE,
        };
        self.reported = reported_current(&inputs).0;

        let status = Status {
            online: true,
            target_ampere: self.target.unwrap_or(0.0),
            measured_ampere: self.measured,
            offset_ampere: self.offset,
            reported_ampere: self.reported,
            last_poll_age_s: now
                .saturating_duration_since(self.last_poll_at)
                .as_secs_f32(),
            measurement_age_s: now
                .saturating_duration_since(self.measured_at)
                .as_secs_f32(),
            ramping: self.ramping,
            failsafe: target_stale,
            measurement_failsafe: measured_stale,
            charge_state: charge_state(Ampere(self.reported), Ampere(MAX_BOX_AMPERE)),
            enabled: self.enabled,
            last_error: self.last_error.as_deref(),
        };
        status_json(&status)
    }
}

impl Default for ControlState {
    fn default() -> Self {
        Self::new()
    }
}
