//! The control seam (SPECS.md §6): the MQTT target current drives the household
//! current the meter slave serves.
//!
//! A `watch` channel carries the latest target (mirroring the [`crate::slave`]
//! `LinkHealth` watch idiom). The slave recomputes [`crate::reported_current`]
//! from it on every poll, so a new target is reflected in the very next served
//! frame without any extra signalling. Reads on the poll path stay lock-free.

use crate::mqtt::TargetError;
use crate::reported_current;
use tokio::sync::watch;

/// Wire the MQTT command stream to the bytes the slave serves.
///
/// Returns `(apply, currents)`: `apply` is the [`crate::mqtt::run_mqtt`] seam — it
/// adopts each `Ok` target and holds the last good value on `Err` (docs/mqtt.md);
/// `currents` is the [`crate::slave::run_link`] seam, yielding the per-phase
/// household current to report for the live target. `initial_target_a` is the
/// value served until the first command arrives (the retained MQTT target, or the
/// failsafe — the caller decides).
pub fn channel(
    fuse_limit_a: f32,
    initial_target_a: f32,
) -> (impl Fn(Result<f32, TargetError>), impl Fn() -> [f32; 3]) {
    let (tx, rx) = watch::channel(initial_target_a);

    let apply = move |target: Result<f32, TargetError>| {
        if let Ok(amps) = target {
            let _ = tx.send(amps);
        }
    };
    let currents = move || [reported_current(fuse_limit_a, *rx.borrow()); 3];

    (apply, currents)
}
