//! Control math for the EVC04 meter emulation.
//!
//! The EVC04 Power Optimizer allows `charge_current ≤ fuse_limit − household_current`.
//! We don't measure a household — we fabricate the "household current" we report so the
//! box leaves exactly the headroom we want the car to draw. See `SPECS.md` §6.

/// Household current (amps, per phase) to report so the EVC04 permits `target` charge amps.
///
/// `reported = fuse_limit − target`, with `target` clamped to `[0, fuse_limit]`:
/// - `target = fuse_limit` → report `0` → maximum charge.
/// - `target = 0` → report `fuse_limit` → zero headroom → charging pauses.
pub fn reported_current(fuse_limit: f32, target: f32) -> f32 {
    fuse_limit - target.clamp(0.0, fuse_limit)
}

pub mod config;
pub mod frame;
pub mod mqtt;
pub mod slave;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_fuse_means_paused() {
        assert_eq!(reported_current(16.0, 0.0), 16.0);
    }

    #[test]
    fn zero_report_means_max_charge() {
        assert_eq!(reported_current(16.0, 16.0), 0.0);
    }
}
