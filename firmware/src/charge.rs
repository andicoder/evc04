//! On-box meter-emulation control (evc04#86): the worker-local [`Controller`] and
//! the lock-free [`Handoff`] that carries two scalars across the thread boundary.
//!
//! The [`Controller`] holds the MQTT control inputs (target / measured / enable and
//! their arrival times), owns the clock, soft-ramps the offset, and each tick
//! computes the per-poll reported current with the **host-tested** `core` control
//! math — so the ESP serves the exact value the k3s daemon would (no second
//! implementation that could drift). It lives only on the prober/worker thread.
//!
//! Only two values genuinely cross to the RS485 slave thread, so they live in
//! [`Handoff`] (two atomics, no mutex): `reported` (worker → slave, the current to
//! answer the box with) and `last_poll` (slave → worker, the box's last-poll
//! timestamp for the status liveness).
//!
//! Config mirrors the daemon's env defaults (`charge/src/config.rs`); `MAX_BOX` is
//! the box's DIP-set ceiling for this install. Failsafe direction is `pause` on
//! both channels — a stale input STOPS an evcc/HA-managed box, never starts it
//! (SPECS §9, #52).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use evc04_cn28_core::charge::control::{
    ramp_step, reported_current, trim_decay, trim_step, Ampere, ControlInputs, FailsafeMode,
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

/// #119 layered integral trim: pushes the box below its natural ~9–15 A floor by
/// integrating the CN28-reported actual charge current against the target.
/// 🤔 `TRIM_KI` and `TRIM_MAX_AMPERE` are first guesses that need live tuning on the
/// box (the box may not drop below ~9 A at all — the trim is built to *reveal* the
/// real minimum via saturation, not to assume it).
const TRIM_KI: f32 = 0.5;
const TRIM_MAX_AMPERE: f32 = 8.0;
/// Per stale-sample decay back toward 0 (#119): when CN28 feedback ages out we relax
/// the correction to the hardware-proven `offset + measured` loop rather than hold a
/// value the loop can no longer see.
const TRIM_DECAY_AMPERE: f32 = 1.0;
/// Advance the trim at the box's metering cadence (~5 s), **not** per 1 s tick —
/// integrating stale data each tick would over-correct ~5× and oscillate (#119).
const CN28_FEEDBACK_PERIOD: Duration = Duration::from_secs(5);
/// CN28 feedback older than this is stale → the trim decays instead of integrating.
const CN28_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-phase value that pauses the box: above the ceiling so an active charge is
/// actually cut (#57). The [`Handoff`] starts here so the slave serves a safe value
/// before the first tick or before any command lands.
const PAUSE_REPORT_AMPERE: f32 = MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE;

/// One control tick's outputs: the per-phase current to hand to the slave, and the
/// retained status JSON to publish.
pub struct Tick {
    pub reported: f32,
    pub status_json: String,
}

/// Worker-local control state. Lives only on the prober/worker thread; the value it
/// computes for the slave crosses via [`Handoff`], not this struct.
pub struct Controller {
    target: Option<f32>,
    target_at: Option<Instant>,
    measured: f32,
    measured_at: Instant,
    enabled: bool,
    offset: f32,
    ramping: bool,
    last_tick: Instant,
    last_error: Option<String>,
    /// #119: latest CN28-reported actual per-phase charge current and when it landed,
    /// the integral `trim`, and when the trim was last advanced (its ~5 s cadence).
    cn28_actual: f32,
    cn28_at: Instant,
    trim: f32,
    last_trim_at: Instant,
}

impl Controller {
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
            ramping: false,
            last_tick: now,
            last_error: None,
            // Start with the trim idle and a fresh (zero) feedback reading, so the
            // loop behaves exactly like pre-#119 until real CN28 current arrives.
            cn28_actual: 0.0,
            cn28_at: now,
            trim: 0.0,
            last_trim_at: now,
        }
    }

    /// Feed the latest CN28-reported actual per-phase charge current (#119). Called by
    /// the prober whenever a telemetry window decodes a phase reading; it only stamps
    /// the value and its arrival time — the trim itself advances in [`tick`] on the
    /// ~5 s cadence, so repeated identical readings between refreshes don't over-integrate.
    pub fn apply_cn28_feedback(&mut self, actual_ampere: f32, now: Instant) {
        self.cn28_actual = actual_ampere;
        self.cn28_at = now;
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
    /// compute the reported current via `core`, and return it alongside the retained
    /// status JSON. `last_poll_age_s` (the slave-stamped RS485 liveness) is supplied
    /// by the caller from the [`Handoff`], since this struct no longer sees the poll.
    pub fn tick(&mut self, now: Instant, last_poll_age_s: f32) -> Tick {
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

        // #119: advance the integral trim at the box's ~5 s metering cadence, not per
        // 1 s tick. Fresh feedback with a target → integrate toward the floor; stale
        // feedback (or no target to seek) → decay back toward the proven base loop.
        let cn28_stale = now.saturating_duration_since(self.cn28_at) > CN28_TIMEOUT;
        if now.saturating_duration_since(self.last_trim_at) >= CN28_FEEDBACK_PERIOD {
            self.last_trim_at = now;
            self.trim = match self.target {
                Some(target) if !cn28_stale => trim_step(
                    Ampere(self.trim),
                    Ampere(self.cn28_actual),
                    Ampere(target),
                    TRIM_KI,
                    Ampere(TRIM_MAX_AMPERE),
                ),
                _ => trim_decay(Ampere(self.trim), Ampere(TRIM_DECAY_AMPERE)),
            }
            .0;
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
            trim: Ampere(self.trim),
            measured: Ampere(self.measured),
            enabled: self.enabled,
            target_stale,
            measured_stale,
            target_failsafe: TARGET_FAILSAFE,
            measured_failsafe: MEASURED_FAILSAFE,
        };
        let reported = reported_current(&inputs).0;

        let status = Status {
            online: true,
            target_ampere: self.target.unwrap_or(0.0),
            measured_ampere: self.measured,
            offset_ampere: self.offset,
            reported_ampere: reported,
            last_poll_age_s,
            measurement_age_s: now
                .saturating_duration_since(self.measured_at)
                .as_secs_f32(),
            ramping: self.ramping,
            failsafe: target_stale,
            measurement_failsafe: measured_stale,
            charge_state: charge_state(Ampere(reported), Ampere(MAX_BOX_AMPERE)),
            enabled: self.enabled,
            last_error: self.last_error.as_deref(),
            trim_ampere: self.trim,
            cn28_actual_ampere: self.cn28_actual,
            cn28_feedback_stale: cn28_stale,
        };
        Tick {
            reported,
            status_json: status_json(&status),
        }
    }
}

impl Default for Controller {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock-free hand-off between the worker (control) thread and the RS485 slave.
/// Only two scalars genuinely cross the boundary, so each is a single atomic — no
/// mutex, and the slave never blocks on the worker to answer a poll.
///
/// Both are 32-bit: the ESP32 (Xtensa) has no native 64-bit atomics, so the poll
/// timestamp is milliseconds (not microseconds) since boot. It wraps after ~49.7
/// days; the worker reads it with `wrapping_sub`, which yields the correct age for
/// any real gap (< the wrap period).
pub struct Handoff {
    /// `reported` as f32 bits: worker stores it each tick, slave loads it to answer.
    reported_bits: AtomicU32,
    /// `esp_timer` milliseconds of the box's last poll: slave stamps it, worker reads
    /// it for `last_poll_age_s`. 0 = never polled (≈ boot, so age ≈ uptime).
    last_poll_ms: AtomicU32,
}

impl Handoff {
    /// Construct paused: report above the ceiling so the slave serves a safe value
    /// before the first control tick (the cold-start pause, #52/#59).
    pub fn new() -> Self {
        Self {
            reported_bits: AtomicU32::new(PAUSE_REPORT_AMPERE.to_bits()),
            last_poll_ms: AtomicU32::new(0),
        }
    }

    /// Worker: store the per-phase current the slave should report next.
    pub fn set_reported(&self, amps: f32) {
        self.reported_bits.store(amps.to_bits(), Ordering::Relaxed);
    }

    /// Slave: the latest per-phase current to answer the box with.
    pub fn reported(&self) -> f32 {
        f32::from_bits(self.reported_bits.load(Ordering::Relaxed))
    }

    /// Slave: stamp the moment the box polled us (drives `last_poll_age_s`; a growing
    /// gap signals a dead RS485 link). `now_ms` is `esp_timer` milliseconds.
    pub fn note_poll(&self, now_ms: u32) {
        self.last_poll_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Worker: `esp_timer` milliseconds of the last poll (0 if never polled).
    pub fn last_poll_ms(&self) -> u32 {
        self.last_poll_ms.load(Ordering::Relaxed)
    }
}

impl Default for Handoff {
    fn default() -> Self {
        Self::new()
    }
}
