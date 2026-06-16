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
//! answering — with the current derived from `FAILSAFE_TARGET_A` — rather than
//! going quiet. Staleness is derived on read against the last command's timestamp.

use crate::mqtt::TargetError;
use crate::reported_current;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

/// The last command and when it was accepted, so any reader can judge staleness.
#[derive(Clone, Copy)]
struct Sample {
    target_a: f32,
    at: Instant,
}

/// Write half: hand to the MQTT task. Each accepted target replaces the last and
/// refreshes the staleness clock; a rejected command holds the previous value.
pub struct TargetSink {
    tx: watch::Sender<Sample>,
}

impl TargetSink {
    /// Adopt an accepted target; hold the last good value on a rejected command
    /// (docs/mqtt.md). Every accepted command restarts the failsafe window.
    pub fn apply(&self, target: Result<f32, TargetError>) {
        if let Ok(target_a) = target {
            let _ = self.tx.send(Sample {
                target_a,
                at: Instant::now(),
            });
        }
    }
}

/// Read half (cloneable, lock-free reads): the slave's `currents` source and the
/// status publisher's view of the effective target and failsafe state.
#[derive(Clone)]
pub struct ControlView {
    rx: watch::Receiver<Sample>,
    fuse_limit_a: f32,
    failsafe_target_a: f32,
    failsafe_after: Duration,
}

impl ControlView {
    /// Per-phase household current to report for the current effective target.
    pub fn currents(&self) -> [f32; 3] {
        [reported_current(self.fuse_limit_a, self.target_a()); 3]
    }

    /// Effective target (amps), post-clamp and failsafe-aware: the last commanded
    /// value while fresh, else `FAILSAFE_TARGET_A` once stale. This is what the
    /// status topic reports as `target_a` (docs/mqtt.md).
    pub fn effective_target_a(&self) -> f32 {
        self.target_a().clamp(0.0, self.fuse_limit_a)
    }

    /// True while the last accepted target is older than `failsafe_after`, so the
    /// slave is serving `FAILSAFE_TARGET_A` rather than a live command.
    pub fn failsafe_active(&self) -> bool {
        self.rx.borrow().at.elapsed() > self.failsafe_after
    }

    /// The effective (failsafe-aware) target before clamping.
    fn target_a(&self) -> f32 {
        let sample = *self.rx.borrow();
        if sample.at.elapsed() > self.failsafe_after {
            self.failsafe_target_a
        } else {
            sample.target_a
        }
    }
}

/// Wire the MQTT command stream to the bytes the slave serves (SPECS §6/§9).
///
/// `initial_target_a` is served until the first command arrives (the daemon
/// passes the failsafe value, so startup and the grace window are already safe);
/// `failsafe_after` is the staleness window before the fallback engages.
pub fn channel(
    fuse_limit_a: f32,
    failsafe_target_a: f32,
    failsafe_after: Duration,
    initial_target_a: f32,
) -> (TargetSink, ControlView) {
    let (tx, rx) = watch::channel(Sample {
        target_a: initial_target_a,
        at: Instant::now(),
    });
    (
        TargetSink { tx },
        ControlView {
            rx,
            fuse_limit_a,
            failsafe_target_a,
            failsafe_after,
        },
    )
}
