//! The control seam (SPECS.md §6/§9): the MQTT target current drives the household
//! current the meter slave serves, with a failsafe when commands go stale.
//!
//! A `watch` channel carries the latest `(target, timestamp)` (mirroring the
//! [`crate::slave`] `LinkHealth` watch idiom). The slave recomputes
//! [`crate::reported_current`] from it on every poll, so a new target is reflected
//! in the very next served frame and reads on the poll path stay lock-free.
//!
//! SPECS §9: a silent meter *faults* the box (solid red), it does not merely
//! pause. So when no fresh target arrives within `failsafe_after`, we keep
//! answering — with **full charge** (report 0 A, the meterless-box default) — rather
//! than going quiet. Staleness is derived on read against the last command's timestamp.

use crate::config::FailsafeMode;
use crate::mqtt::TargetError;
use crate::{ramp_step, reported_from_offset, Ampere};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// Combine two forced failsafe reports into the **safest** one (higher report = less
/// charge), so when both staleness failsafes engage the least-charge directive wins (#51).
fn safest(a: Option<Ampere>, b: Option<Ampere>) -> Option<Ampere> {
    match (a, b) {
        (Some(x), Some(y)) => Some(Ampere(x.0.max(y.0))),
        (only, None) | (None, only) => only,
    }
}

/// The last command and when it was accepted, so any reader can judge staleness.
#[derive(Clone, Copy)]
struct Sample {
    target: Ampere,
    at: Instant,
}

/// Write half: hand to the MQTT task. Each accepted target replaces the last and
/// refreshes the staleness clock; a rejected command holds the previous value.
pub struct TargetSink {
    tx: watch::Sender<Sample>,
}

impl TargetSink {
    /// Adopt an accepted target; hold the last good value on a rejected command
    /// (docs/mqtt.md). Every accepted command restarts the failsafe window. The amps
    /// arrive as a raw `f32` off the wire and become an [`Ampere`] here, at the boundary.
    pub fn apply(&self, target: Result<f32, TargetError>) {
        if let Ok(amps) = target {
            let _ = self.tx.send(Sample {
                target: Ampere(amps),
                at: Instant::now(),
            });
        }
    }
}

/// Read half (cloneable, lock-free reads) of the target stream: the [`Controller`]'s
/// failsafe-aware target source and the status publisher's view of target/failsafe state.
#[derive(Clone)]
pub struct ControlView {
    rx: watch::Receiver<Sample>,
    max_box_ampere: Ampere,
    failsafe_after: Duration,
}

impl ControlView {
    /// The last commanded target, post-clamp — what the status topic reports as
    /// `target_ampere` (docs/mqtt.md). Staleness is *not* folded in here (#51): the
    /// [`Controller`] decides what a stale target means via its [`FailsafeMode`], and the
    /// `failsafe` flag (not a value jump) signals the override.
    pub fn effective_target(&self) -> Ampere {
        self.rx
            .borrow()
            .target
            .clamp(Ampere(0.0), self.max_box_ampere)
    }

    /// True while the last accepted target is older than `failsafe_after`, so the
    /// target-staleness failsafe is engaged.
    pub fn failsafe_active(&self) -> bool {
        self.rx.borrow().at.elapsed() > self.failsafe_after
    }
}

/// The live measured current and when it last updated, so the closed loop and the
/// measurement-loss failsafe (#25) can both judge its freshness.
#[derive(Clone, Copy)]
struct Measurement {
    amps: Ampere,
    at: Instant,
}

/// Write half of the measurement channel: hand to the MQTT task. A fresh value
/// replaces the last and restarts the age clock; a rejected payload holds the last
/// good value, because serving an offset against a corrupt measurement is unsafe
/// (#25), not merely wrong.
pub struct MeasurementSink {
    tx: watch::Sender<Measurement>,
}

impl MeasurementSink {
    /// Adopt a fresh measured current; hold the last good value on a rejected
    /// payload (docs/mqtt.md), mirroring [`TargetSink::apply`].
    pub fn apply(&self, measured: Result<f32, TargetError>) {
        if let Ok(amps) = measured {
            let _ = self.tx.send(Measurement {
                amps: Ampere(amps),
                at: Instant::now(),
            });
        }
    }
}

/// Read half (cloneable, lock-free reads) of the live measured current that closes
/// the loop. The closed loop (#23) reads [`Self::measured`]; the measurement-loss
/// failsafe (#25) reads [`Self::failsafe_active`]/[`Self::age`].
#[derive(Clone)]
pub struct MeasurementView {
    rx: watch::Receiver<Measurement>,
    failsafe_after: Duration,
}

impl MeasurementView {
    /// Latest measured per-phase current. A single published value applies to all three
    /// phases (docs/mqtt.md), so this scalar is broadcast downstream.
    pub fn measured(&self) -> Ampere {
        self.rx.borrow().amps
    }

    /// Time since the last accepted measurement — the status topic's `measurement_age_s`.
    pub fn age(&self) -> Duration {
        self.rx.borrow().at.elapsed()
    }

    /// True once the measured input is older than `failsafe_after`: the closed loop can
    /// no longer be trusted (serving `offset + stale_measured` would hold the box at the
    /// wrong current), so the [`Controller`] reverts to full charge (#25).
    pub fn failsafe_active(&self) -> bool {
        self.age() > self.failsafe_after
    }
}

/// Wire the MQTT measured-current stream to a lock-free view the closed loop reads on
/// the poll path (#22). `initial` is held until the first measurement arrives;
/// `failsafe_after` is the staleness window past which the measurement-loss failsafe
/// engages (#25).
pub fn measurement_channel(
    initial: Ampere,
    failsafe_after: Duration,
) -> (MeasurementSink, MeasurementView) {
    let (tx, rx) = watch::channel(Measurement {
        amps: initial,
        at: Instant::now(),
    });
    (
        MeasurementSink { tx },
        MeasurementView { rx, failsafe_after },
    )
}

/// Read half (cloneable, lock-free reads) of the soft-ramped offset the loop serves
/// (#24). The driver ([`run_ramp`]) writes it; the [`Controller`] reads it on the poll
/// path.
#[derive(Clone)]
pub struct OffsetView {
    rx: watch::Receiver<Ampere>,
}

impl OffsetView {
    /// Latest ramped offset (it converges to `max − target` once the setpoint settles).
    pub fn offset(&self) -> Ampere {
        *self.rx.borrow()
    }
}

/// Wire the soft-ramp driver to a lock-free view the closed loop reads on the poll path
/// (#24). `initial` is the offset before the ramp moves it — 0 (full charge) at startup,
/// so the box charges at the meterless default until the first target ramps it down.
pub fn offset_channel(initial: Ampere) -> (watch::Sender<Ampere>, OffsetView) {
    let (tx, rx) = watch::channel(initial);
    (tx, OffsetView { rx })
}

/// Drive the soft-ramp (#24): every `tick`, move the offset toward its setpoint
/// (`max − effective_target`) by at most `rate_ampere_per_s × dt` and publish it for the slave
/// to read. A step change of the target then reaches the box gradually instead of shocking
/// its closed loop below the car's floor. Runs until every reader is dropped.
///
/// This is the I/O boundary (like [`run_link`]/[`run_mqtt`]); the bounded-step arithmetic
/// is unit-tested in [`ramp_step`], not here.
pub async fn run_ramp(
    target: ControlView,
    rate_ampere_per_s: f32,
    tick: Duration,
    tx: watch::Sender<Ampere>,
) {
    let mut interval = tokio::time::interval(tick);
    let mut last = Instant::now();
    loop {
        interval.tick().await;
        let now = Instant::now();
        let dt = now.duration_since(last);
        last = now;

        let setpoint = target.max_box_ampere - target.effective_target();
        let max_step = Ampere(rate_ampere_per_s * dt.as_secs_f32());
        let next = ramp_step(*tx.borrow(), setpoint, max_step);
        if tx.send(next).is_err() {
            return; // all readers gone; nothing left to drive
        }
    }
}

/// Joins the inbound streams into the single per-poll answer the slave serves: the
/// failsafe-aware [`ControlView`] target (for the min-charge cutoff), the soft-ramped
/// [`OffsetView`], and the live [`MeasurementView`] current — `clamp(offset + measured)`
/// (#23/#24). Cloneable, lock-free reads.
#[derive(Clone)]
pub struct Controller {
    target: ControlView,
    measured: MeasurementView,
    offset: OffsetView,
    min_charge: Ampere,
    target_failsafe: FailsafeMode,
    measured_failsafe: FailsafeMode,
}

impl Controller {
    pub fn new(
        target: ControlView,
        measured: MeasurementView,
        offset: OffsetView,
        min_charge: Ampere,
        target_failsafe: FailsafeMode,
        measured_failsafe: FailsafeMode,
    ) -> Controller {
        Controller {
            target,
            measured,
            offset,
            min_charge,
            target_failsafe,
            measured_failsafe,
        }
    }

    /// Per-phase household current to report, as the raw `f32` triple the Modbus frame
    /// carries (the wire boundary).
    ///
    /// A stale **target** (#7) or **measurement** (#25) engages its configured
    /// [`FailsafeMode`] (#51): `full_charge` → report 0 A (the meterless-box default,
    /// SPECS §9), `pause` → the ceiling (zero headroom → the box stops), `hold_last` →
    /// no override (the held value flows through the loop below). When both engage with a
    /// forced value, the safest (least-charge, i.e. highest report) wins. Below the
    /// min-charge floor we hard-pause (#23). Otherwise we close the loop on the live
    /// measurement and the **soft-ramped** offset (#24).
    pub fn reported_frame(&self) -> [f32; 3] {
        let max = self.target.max_box_ampere;
        let mut forced: Option<Ampere> = None;
        if self.target.failsafe_active() {
            forced = safest(forced, self.target_failsafe.forced_report(max));
        }
        if self.measured.failsafe_active() {
            forced = safest(forced, self.measured_failsafe.forced_report(max));
        }
        if let Some(report) = forced {
            return [report.0; 3];
        }
        if self.target.effective_target().0 < self.min_charge.0 {
            return [max.0; 3];
        }
        let reported = reported_from_offset(max, self.offset.offset(), self.measured.measured());
        [reported.0; 3]
    }

    /// Failsafe-aware effective target for the status topic's `target_ampere` (docs/mqtt.md).
    pub fn effective_target(&self) -> Ampere {
        self.target.effective_target()
    }

    /// Last live measured current consumed, for the status topic's `measured_ampere`.
    pub fn measured(&self) -> Ampere {
        self.measured.measured()
    }

    /// Current soft-ramped offset, for the status topic's `offset_ampere`.
    pub fn offset(&self) -> Ampere {
        self.offset.offset()
    }

    /// Whether the soft-ramped offset is still chasing its setpoint (`max − target`), for
    /// the status topic's `ramping` (#24). Settled means the ramp has snapped exactly onto
    /// the setpoint, so a small epsilon guards only float noise, not a real gap.
    pub fn ramping(&self) -> bool {
        let setpoint = self.target.max_box_ampere - self.effective_target();
        (self.offset.offset().0 - setpoint.0).abs() > 0.05
    }

    /// Whether the target-staleness full-charge failsafe is engaged (status `failsafe`).
    pub fn failsafe_active(&self) -> bool {
        self.target.failsafe_active()
    }

    /// Whether the measurement-loss full-charge failsafe is engaged (status
    /// `measurement_failsafe`, #25).
    pub fn measurement_failsafe_active(&self) -> bool {
        self.measured.failsafe_active()
    }

    /// Age of the live measured input for the status topic's `measurement_age_s` (#25).
    pub fn measurement_age(&self) -> Duration {
        self.measured.age()
    }

    /// Approximated evcc charge status (`A`/`B`/`C`) for the status topic's `charge_state`
    /// (#28), derived from the per-poll served value and the live measurement.
    pub fn charge_state(&self) -> &'static str {
        crate::charge_state(
            Ampere(self.reported_frame()[0]),
            self.target.max_box_ampere,
            self.measured.measured(),
        )
    }
}

/// Wire the MQTT command stream to the bytes the slave serves (SPECS §6/§9).
///
/// `max_box_ampere` is the box's DIP-set ceiling; until the first command arrives the
/// view serves it as the target → `reported = 0` → full charge, so startup is already
/// safe. `failsafe_after` is the staleness window before the same full-charge fallback
/// re-engages.
pub fn channel(max_box_ampere: Ampere, failsafe_after: Duration) -> (TargetSink, ControlView) {
    let (tx, rx) = watch::channel(Sample {
        target: max_box_ampere,
        at: Instant::now(),
    });
    (
        TargetSink { tx },
        ControlView {
            rx,
            max_box_ampere,
            failsafe_after,
        },
    )
}
