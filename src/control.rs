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

/// Read half (cloneable, lock-free reads) of the target stream: the [`Controller`]'s
/// failsafe-aware target source and the status publisher's view of target/failsafe state.
#[derive(Clone)]
pub struct ControlView {
    rx: watch::Receiver<Sample>,
    max_box_ampere: Ampere,
    failsafe_after: Duration,
}

impl ControlView {
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

/// Joins the two inbound streams into the single per-poll answer the slave serves: the
/// failsafe-aware [`ControlView`] target and the live [`MeasurementView`] current, run
/// through the closed-loop [`reported_household`] math (#23). Cloneable, lock-free reads.
#[derive(Clone)]
pub struct Controller {
    target: ControlView,
    measured: MeasurementView,
    min_charge: Ampere,
}

impl Controller {
    pub fn new(target: ControlView, measured: MeasurementView, min_charge: Ampere) -> Controller {
        Controller {
            target,
            measured,
            min_charge,
        }
    }

    /// Per-phase household current to report, as the raw `f32` triple the Modbus frame
    /// carries (the wire boundary).
    ///
    /// Either staleness failsafe falls back to the **static** full charge (report 0 A, the
    /// meterless-box default — SPECS §9), never a pause: a stale *target* (#7) or a stale
    /// *measurement* (#25) both make the closed loop untrustworthy, and serving
    /// `offset + measured` then would throttle — the wrong failsafe direction. Only with a
    /// fresh target *and* a fresh measurement do we close the loop.
    pub fn reported_frame(&self) -> [f32; 3] {
        if self.target.failsafe_active() || self.measured.failsafe_active() {
            return [0.0; 3];
        }
        let reported = reported_household(
            self.target.max_box_ampere,
            self.target.effective_target(),
            self.measured.measured(),
            self.min_charge,
        );
        [reported.0; 3]
    }

    /// Failsafe-aware effective target for the status topic's `target_a` (docs/mqtt.md).
    pub fn effective_target(&self) -> Ampere {
        self.target.effective_target()
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
