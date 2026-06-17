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

use crate::mqtt::TargetError;
use crate::{reported_household, Ampere};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

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

/// Read half (cloneable, lock-free reads): the slave's reported-frame source and the
/// status publisher's view of the effective target and failsafe state.
#[derive(Clone)]
pub struct ControlView {
    rx: watch::Receiver<Sample>,
    max_box_ampere: Ampere,
    failsafe_after: Duration,
}

impl ControlView {
    /// Per-phase household current to report for the current effective target, as the
    /// raw `f32` triple the Modbus frame carries (the wire boundary).
    pub fn reported_frame(&self) -> [f32; 3] {
        [reported_household(self.max_box_ampere, self.target()).0; 3]
    }

    /// Effective target, post-clamp and failsafe-aware: the last commanded value while
    /// fresh, else full charge (`MAX_BOX_AMPERE`) once stale. This is what the status
    /// topic reports as `target_a` (docs/mqtt.md).
    pub fn effective_target(&self) -> Ampere {
        self.target().clamp(Ampere(0.0), self.max_box_ampere)
    }

    /// True while the last accepted target is older than `failsafe_after`, so the
    /// slave is serving full charge rather than a live command.
    pub fn failsafe_active(&self) -> bool {
        self.rx.borrow().at.elapsed() > self.failsafe_after
    }

    /// The effective (failsafe-aware) target before clamping. A stale or absent
    /// command falls back to `MAX_BOX_AMPERE` → `reported = 0` → full charge.
    fn target(&self) -> Ampere {
        let sample = *self.rx.borrow();
        if sample.at.elapsed() > self.failsafe_after {
            self.max_box_ampere
        } else {
            sample.target
        }
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
/// the loop. The closed loop (#23) reads [`Self::measured`]; the failsafe (#25) reads
/// [`Self::age`].
#[derive(Clone)]
pub struct MeasurementView {
    rx: watch::Receiver<Measurement>,
}

impl MeasurementView {
    /// Latest measured per-phase current. A single published value applies to all three
    /// phases (docs/mqtt.md), so this scalar is broadcast downstream.
    pub fn measured(&self) -> Ampere {
        self.rx.borrow().amps
    }

    /// Time since the last accepted measurement — the input to the staleness
    /// failsafe (#25) and the status topic's `measurement_age_s`.
    pub fn age(&self) -> Duration {
        self.rx.borrow().at.elapsed()
    }
}

/// Wire the MQTT measured-current stream to a lock-free view the closed loop reads on
/// the poll path (#22). `initial` is held until the first measurement arrives.
pub fn measurement_channel(initial: Ampere) -> (MeasurementSink, MeasurementView) {
    let (tx, rx) = watch::channel(Measurement {
        amps: initial,
        at: Instant::now(),
    });
    (MeasurementSink { tx }, MeasurementView { rx })
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
