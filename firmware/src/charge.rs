//! On-box meter-emulation control (evc04#86): the worker-local [`Controller`] and
//! the lock-free [`Handoff`] that carries two scalars across the thread boundary.
//!
//! The [`Controller`] holds the MQTT control inputs (target / grid heartbeat /
//! enable and their arrival times), owns the clock, and each tick computes the
//! per-poll reported current with the **host-tested** `core` V4 grant tracking
//! (#135): the box's own `lb_current` from the CN28 LOG is the feedback, the meter
//! answer tells the box "over / at / under the limit" and the box's measured
//! internal loop does the ramping. It lives only on the prober/worker thread.
//!
//! Only two values genuinely cross to the RS485 slave thread, so they live in
//! [`Handoff`] (two atomics, no mutex): `reported` (worker → slave, the current to
//! answer the box with) and `last_poll` (slave → worker, the box's last-poll
//! timestamp for the status liveness).
//!
//! `MAX_BOX` is the box's DIP-set ceiling for this install. The `target` is a
//! **latched** setpoint (never aged out — aging it would deadlock evcc, whose MQTT
//! charger publishes the current on-change and then holds it). The failsafes all
//! pause: a stale grid heartbeat (#136 — HA/evcc gone while the latched target
//! would charge forever), stale CN28 feedback (the regulation is blind) and
//! `enable=false` each STOP an evcc/HA-managed box, never start it (SPECS §7, #52).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};
use evc04_cn28_core::charge::control::{
    grant_tracking_current, probe_report, Ampere, GrantControlInputs,
};
use evc04_cn28_core::charge::intake::IntakeError;
use evc04_cn28_core::charge::status::{charge_state, status_json, Status};
use evc04_cn28_core::probe::cn28::CpState;
use log::warn;

const MAX_BOX_AMPERE: f32 = 16.0;
const MIN_CHARGE_AMPERE: f32 = 6.0;
const PAUSE_MARGIN_AMPERE: f32 = 4.0;
/// The grid heartbeat (#136) arrives every ~5 s; three missed publishes → pause.
const GRID_TIMEOUT: Duration = Duration::from_secs(15);

/// NVS namespace + keys for the persisted setpoint. The `target` topic is
/// non-retained and evcc publishes the current on-change (then holds it), so without
/// this the box would cold-start paused after every OTA/reboot until evcc's next
/// change — the car would not resume. Persisting the last commanded `target`/`enable`
/// lets a reboot pick up where it left off. Written only on change, so flash wear
/// stays negligible (evcc changes the target minutes apart, within NVS wear-levelling).
const NVS_NAMESPACE: &str = "charge";
const NVS_KEY_TARGET: &str = "target";
const NVS_KEY_ENABLE: &str = "enable";

/// V4 (#135): cap on the over-report — the strongest *measured* shed rate
/// (−2 A/eval) while staying clearly below the box's cut threshold (>2, ≤4).
const LB_TRACKING_MAX_OVER_AMPERE: f32 = 2.0;
/// CN28 feedback (the box's ~5 s `lb_current` metering) older than this is stale —
/// the V4 regulation is blind, so the controller pauses the box.
const CN28_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-phase value that pauses the box: above the ceiling so an active charge is
/// actually cut (#57). The [`Handoff`] starts here so the slave serves a safe value
/// before the first tick or before any command lands.
const PAUSE_REPORT_AMPERE: f32 = MAX_BOX_AMPERE + PAUSE_MARGIN_AMPERE;

/// Measurement probe (#135 step 6): the largest accepted lift over the ceiling —
/// below `PAUSE_MARGIN_AMPERE`, so a probe can approach the box's cut threshold
/// (#57: ~2–4 A over) without commanding the hard pause outright.
const PROBE_MAX_OVER_AMPERE: f32 = 3.5;
/// A probe expires on its own: a forgotten publish must not keep perturbing the
/// meter answer. Re-publish to extend a running measurement.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// Wait this long after boot before probing for the pilot state (#161). The control
/// pilot is transition-only, so a reboot with a vehicle plugged in and idle leaves it
/// unknown forever — nothing changes, so the box never reports. Long enough that a
/// real transition (someone plugging in, evcc resuming) resolves it first and the
/// probe never runs.
const PILOT_PROBE_DELAY: Duration = Duration::from_secs(90);
/// How long the probe holds the offer open.
///
/// A first attempt used 3 s, hoping to be too brief for a plugged car to engage. It
/// was measured live on 2026-08-16 and did nothing at all: the offer opened and closed
/// exactly as designed and the box never moved `Cmax`, because the box's own up clock
/// is ~30 s (the same clock the cold-start kick works around). So a short window is not
/// the cautious choice, it is simply the useless one — there is no setting that both
/// beats the box's clock and undercuts the car's.
///
/// 45 s therefore accepts that a plugged car may start. That is bounded and cheap: the
/// moment the pilot reports, `charge_state` becomes valid, evcc stops rejecting it and
/// takes over within its next cycle — which is exactly the outcome the probe exists to
/// produce.
const PILOT_PROBE_WINDOW: Duration = Duration::from_secs(45);

/// One control tick's outputs: the per-phase current to hand to the slave and the
/// retained status JSON to publish.
pub struct Tick {
    pub reported: f32,
    pub status_json: String,
}

/// Worker-local control state. Lives only on the prober/worker thread; the value it
/// computes for the slave crosses via [`Handoff`], not this struct.
pub struct Controller {
    target: Option<f32>,
    /// The raw signed grid power (#136) and when it last arrived. The value is a
    /// pass-through diagnostic; its *age* is the HA/evcc liveness failsafe.
    grid_w: f32,
    grid_at: Instant,
    enabled: bool,
    last_error: Option<String>,
    /// V4 feedback: the box's latest grant (`lb_current` from the CN28 LOG, A) and
    /// when it landed, plus the car's live draw (max phase current from the MID
    /// metering) that gates the start posture.
    cn28_lb: f32,
    cn28_car: f32,
    /// The box's real control-pilot state from the CN28 LOG `S:` line (#148) —
    /// transition-only, so `None` from boot until the first plug/charge event.
    cn28_cp_state: Option<CpState>,
    cn28_at: Instant,
    /// #135 step 6: active measurement-probe lift (A over the ceiling) and when it
    /// was last commanded; expires after [`PROBE_TIMEOUT`]. Deliberately *not*
    /// persisted to NVS — a probe is a manual measurement, never a reboot survivor.
    probe_over: f32,
    probe_at: Option<Instant>,
    /// Post-boot pilot probe (#161): when it started, or `None` if it has not run
    /// yet this boot. One shot only — a second one would be a repeat offer nobody
    /// asked for, and the first either resolved the pilot or the box is not talking.
    pilot_probe_at: Option<Instant>,
    pilot_probed: bool,
    /// Boot instant, so the probe delay is measured from a fixed point rather than
    /// from whichever input last arrived.
    boot_at: Instant,
    /// Persisted-setpoint store: the last `target`/`enable` are written here on change
    /// and restored on boot so an OTA/reboot resumes instead of cold-start pausing.
    /// `None` if NVS could not be opened — persistence off, the box still runs.
    nvs: Option<EspDefaultNvs>,
}

impl Controller {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        let now = Instant::now();
        let mut controller = Self {
            target: None,
            // Both freshness clocks start "just fed" so boot gets one grace window
            // (15 s) to receive the real heartbeat/telemetry before the failsafes
            // pause; the values themselves are neutral zeros until then.
            grid_w: 0.0,
            grid_at: now,
            enabled: true,
            last_error: None,
            cn28_lb: 0.0,
            cn28_car: 0.0,
            cn28_cp_state: None,
            cn28_at: now,
            probe_over: 0.0,
            probe_at: None,
            pilot_probe_at: None,
            pilot_probed: false,
            boot_at: now,
            nvs: None,
        };
        // Open the persistence namespace and restore the last commanded setpoint, so
        // an OTA/reboot resumes rather than cold-starting paused. Best-effort: if NVS
        // won't open, persistence is simply off and the box cold-starts as before.
        match EspNvs::new(partition, NVS_NAMESPACE, true) {
            Ok(nvs) => {
                controller.restore_from(&nvs);
                controller.nvs = Some(nvs);
            }
            Err(e) => warn!("charge: NVS open failed, persistence off: {e:#}"),
        }
        controller
    }

    /// Seed `target`/`enable` from the persisted setpoint. Only a value the
    /// controller actually commanded before is restored — a first-ever boot with no
    /// stored key still starts `target = None` → cold-start pause, never a default
    /// charge (#59).
    fn restore_from(&mut self, nvs: &EspDefaultNvs) {
        if let Ok(Some(bits)) = nvs.get_u32(NVS_KEY_TARGET) {
            self.target = Some(f32::from_bits(bits));
        }
        if let Ok(Some(b)) = nvs.get_u8(NVS_KEY_ENABLE) {
            self.enabled = b != 0;
        }
    }

    /// Persist the target as raw f32 bits (NVS has no float type). Best-effort: a
    /// write failure only means this reboot won't resume — never fatal.
    fn persist_target(&self, v: f32) {
        if let Some(nvs) = &self.nvs {
            if let Err(e) = nvs.set_u32(NVS_KEY_TARGET, v.to_bits()) {
                warn!("charge: persist target failed: {e:#}");
            }
        }
    }

    fn persist_enable(&self, b: bool) {
        if let Some(nvs) = &self.nvs {
            if let Err(e) = nvs.set_u8(NVS_KEY_ENABLE, b as u8) {
                warn!("charge: persist enable failed: {e:#}");
            }
        }
    }

    /// Feed the box's latest grant (`lb_current` from the CN28 LOG), the car's
    /// live draw and the control-pilot state — the V4 feedback variables plus the
    /// evcc-facing pilot mirror (#148). Called by the prober whenever a telemetry
    /// window decodes one; stamping the arrival time keeps the staleness failsafe
    /// honest.
    pub fn apply_cn28_feedback(
        &mut self,
        lb_ampere: f32,
        car_ampere: f32,
        cp_state: Option<CpState>,
        now: Instant,
    ) {
        self.cn28_lb = lb_ampere;
        self.cn28_car = car_ampere;
        self.cn28_cp_state = cp_state;
        self.cn28_at = now;
    }

    /// Apply a parsed target. A good value clears `last_error`; a rejected payload is
    /// held (last good stays in effect) and surfaced. The target is latched — once set
    /// it never ages out (evcc publishes it on-change and holds it), so no arrival time
    /// is tracked; `_now` is kept for call-site symmetry with `apply_measured`.
    pub fn apply_target(&mut self, parsed: Result<f32, IntakeError>, _now: Instant) {
        match parsed {
            Ok(v) => {
                // Persist only on change: evcc shifts the target minutes apart on
                // PV/price moves, so this stays well within NVS wear-levelling.
                if self.target != Some(v) {
                    self.persist_target(v);
                }
                self.target = Some(v);
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad target: {e:?}")),
        }
    }

    /// Apply a grid_power heartbeat (#136): the raw signed watts, stored untouched.
    pub fn apply_grid_power(&mut self, parsed: Result<f32, IntakeError>, now: Instant) {
        match parsed {
            Ok(v) => {
                self.grid_w = v;
                self.grid_at = now;
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad grid_power: {e:?}")),
        }
    }

    /// Command a measurement probe (#135 step 6): lift the served meter answer to
    /// `MAX + over` for the next [`PROBE_TIMEOUT`]. Boundary validation here: only
    /// `0 ..= PROBE_MAX_OVER_AMPERE` is accepted (0 clears), anything else is
    /// rejected and surfaced — a typo'd payload must not push the box to the cut.
    pub fn apply_probe_over(&mut self, parsed: Result<f32, IntakeError>, now: Instant) {
        match parsed {
            Ok(v) if (0.0..=PROBE_MAX_OVER_AMPERE).contains(&v) => {
                self.probe_over = v;
                self.probe_at = (v > 0.0).then_some(now);
                self.last_error = None;
                warn!("probe_over set to {v} A (auto-expires in {PROBE_TIMEOUT:?})");
            }
            Ok(v) => self.last_error = Some(format!("probe_over out of range: {v}")),
            Err(e) => self.last_error = Some(format!("bad probe_over: {e:?}")),
        }
    }

    pub fn apply_enable(&mut self, parsed: Result<bool, IntakeError>) {
        match parsed {
            Ok(b) => {
                if self.enabled != b {
                    self.persist_enable(b);
                }
                self.enabled = b;
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(format!("bad enable: {e:?}")),
        }
    }

    /// Advance the loop one tick: compute the V4 reported current via `core` and
    /// return it alongside the retained status JSON. `last_poll_age_s` (the
    /// slave-stamped RS485 liveness) is supplied by the caller from the [`Handoff`],
    /// since this struct no longer sees the poll.
    /// Whether the bounded post-boot pilot probe should open the offer this tick (#161).
    ///
    /// The control pilot is transition-only, so a reboot with a vehicle plugged in and
    /// *idle* leaves it unknown forever: nothing changes, so the box never reports, so
    /// `charge_state` stays empty, so evcc rejects it and commands nothing — and that
    /// is what keeps nothing from changing. Measured 2026-08-16; it cost an afternoon
    /// of PV surplus. Moving the offered current is the only lever that breaks the
    /// cycle, and it demonstrably does (`None` -> `B` -> `C` after a single enable).
    ///
    /// Fires at most once per boot, only while the pilot is still unknown, and only
    /// after a delay long enough for a real transition — someone plugging in, evcc
    /// resuming — to resolve it first. With nothing plugged in it cannot start
    /// anything; with a vehicle plugged in the window is short, and if the car does
    /// engage, evcc regains a valid status within its next cycle and takes over, which
    /// is the whole point of the probe.
    fn pilot_probe_active(&mut self, now: Instant) -> bool {
        if self.cn28_cp_state.is_some() || self.pilot_probed {
            return false;
        }
        match self.pilot_probe_at {
            Some(started) if now.saturating_duration_since(started) < PILOT_PROBE_WINDOW => true,
            Some(_) => {
                self.pilot_probed = true;
                warn!("pilot probe window closed, pilot still unknown");
                false
            }
            None if now.saturating_duration_since(self.boot_at) >= PILOT_PROBE_DELAY => {
                self.pilot_probe_at = Some(now);
                warn!("pilot unknown since boot, opening the offer briefly to make the box report");
                true
            }
            None => false,
        }
    }

    pub fn tick(&mut self, now: Instant, last_poll_age_s: f32) -> Tick {
        let grid_stale = now.saturating_duration_since(self.grid_at) > GRID_TIMEOUT;
        let cn28_stale = now.saturating_duration_since(self.cn28_at) > CN28_TIMEOUT;

        // The target is a latched setpoint with no staleness of its own: evcc's MQTT
        // charger sets the current on-change and holds it, so a target timeout would
        // deadlock (box forgets → pauses → evcc never re-sends). The grid heartbeat
        // carries the "controller is alive" failsafe instead (#136).
        // Hoisted out of the struct literal below: it takes &mut self.
        let pilot_probe = self.pilot_probe_active(now);
        let reported = grant_tracking_current(&GrantControlInputs {
            max: Ampere(MAX_BOX_AMPERE),
            min_charge: Ampere(MIN_CHARGE_AMPERE),
            pause_margin: Ampere(PAUSE_MARGIN_AMPERE),
            max_over: Ampere(LB_TRACKING_MAX_OVER_AMPERE),
            target: self.target.map(Ampere),
            lb: Ampere(self.cn28_lb),
            car: Ampere(self.cn28_car),
            lb_stale: cn28_stale,
            grid_stale,
            enabled: self.enabled,
            pilot_probe,
        })
        .0;

        // Expire a stale probe before applying it (#135 step 6).
        if self
            .probe_at
            .is_some_and(|at| now.saturating_duration_since(at) > PROBE_TIMEOUT)
        {
            self.probe_over = 0.0;
            self.probe_at = None;
            warn!("probe_over expired");
        }
        // charge_state stays derived from the UNprobed value: the probe perturbs only
        // what the meter tells the box, not our command state —
        // evcc reads charge_state as its charger status, and a probe flipping it to
        // 'B' would make evcc believe the charge stopped.
        let charge_state_letter = charge_state(
            Ampere(reported),
            Ampere(MAX_BOX_AMPERE),
            Ampere(PAUSE_MARGIN_AMPERE),
            self.cn28_cp_state,
            cn28_stale,
            // The car's real draw (max phase current off the CN28 MID metering),
            // which outranks a latched pilot letter (#158).
            Ampere(self.cn28_car),
        );
        let served = probe_report(
            Ampere(reported),
            Ampere(self.probe_over),
            Ampere(MAX_BOX_AMPERE),
            Ampere(PROBE_MAX_OVER_AMPERE),
        )
        .0;
        let status = Status {
            online: true,
            target_ampere: self.target.unwrap_or(0.0),
            grid_power_w: self.grid_w,
            // The status shows what the slave actually serves — probe included.
            reported_ampere: served,
            last_poll_age_s,
            grid_age_s: now.saturating_duration_since(self.grid_at).as_secs_f32(),
            grid_failsafe: grid_stale,
            charge_state: charge_state_letter,
            enabled: self.enabled,
            last_error: self.last_error.as_deref(),
            lb_current_ampere: self.cn28_lb,
            cn28_feedback_stale: cn28_stale,
            probe_over_ampere: if self.probe_at.is_some() {
                self.probe_over
            } else {
                0.0
            },
        };
        Tick {
            reported: served,
            status_json: status_json(&status),
        }
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
