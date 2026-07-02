//! Behavioural simulator of the EVC04's Power Optimizer (#135).
//!
//! Black-box model of the box's *internal* charge-limit loop, fitted to the
//! measured sessions in `core/tests/fixtures/sessions/` (analysis #134). The box's
//! firmware is closed; what we model is the observed rule: on a ~5–10 s cadence the
//! box grants the car `car_draw + (DIP limit − reported meter value)` — apparent
//! headroom is *added on top of the current draw* — holds while the meter sits
//! exactly at the limit, and hard-cuts once the meter exceeds the limit by a margin
//! (#57). Whether a *gentle* down-regulation exists between "at the limit" and the
//! cut threshold is unknown (our upper clamp never let the box see that region —
//! #134 H1), so it is a parameter (`down_step`, default 0 = never observed) that a
//! planned live test will measure.
//!
//! Pure and clock-free like the rest of `core`: the caller owns time and feeds
//! elapsed seconds. Deviations knowingly not modelled: the ~1 A down-trickle seen
//! after minutes at the limit (twice per hour, harmless direction) and the ~10 s
//! ramp shape of a cut (modelled as immediate).

use super::control::Ampere;

/// Nearest-integer rounding for granted amps (the box's LOG only ever shows whole
/// amps). Hand-rolled because `f32::round` is libm-gated in `no_std`; inputs are
/// non-negative currents.
fn round_amp(x: f32) -> f32 {
    (x + 0.5) as i32 as f32
}

/// Fit parameters of the box model. All tunable because they are *measured*, not
/// documented — the replay tests against the session fixtures pin them down.
#[derive(Clone, Copy, Debug)]
pub struct BoxSimParams {
    /// The DIP-set Power Optimizer limit (§2); 16 A on the real install.
    pub max_dip: Ampere,
    /// How often the box re-evaluates its grant (~5–10 s observed).
    pub eval_period_s: f32,
    /// Meter excess over `max_dip` at which the box cuts the charge (#57: 2–4 A).
    pub cut_margin: Ampere,
    /// Gentle down-regulation per eval while `max_dip < reported < max_dip +
    /// cut_margin`. Never observed (H1 masked the region); 0 = hold.
    pub down_step: Ampere,
}

/// The box's charge-limit state: the current grant (`lb_current` in the CN28 LOG)
/// and whether a session is active.
#[derive(Debug)]
pub struct BoxSim {
    params: BoxSimParams,
    lb: Ampere,
    charging: bool,
    since_eval_s: f32,
}

impl BoxSim {
    pub fn new(params: BoxSimParams) -> Self {
        Self {
            params,
            lb: Ampere(0.0),
            charging: false,
            since_eval_s: 0.0,
        }
    }

    /// The current grant to the car (`lb_current`).
    pub fn lb(&self) -> Ampere {
        self.lb
    }

    pub fn charging(&self) -> bool {
        self.charging
    }

    /// A plugged car asks to charge. The box grants the apparent headroom
    /// (`max_dip − reported`, whole amps) as the opening limit — observed as the
    /// start-grant `lb=7` at `reported≈9.5` in both fixture sessions. No grant, no
    /// session (returns `false`).
    pub fn try_start(&mut self, reported: Ampere) -> bool {
        let grant = round_amp(self.params.max_dip.0 - reported.0);
        if grant <= 0.0 {
            return false;
        }
        self.lb = Ampere(grant);
        self.charging = true;
        self.since_eval_s = 0.0;
        true
    }

    /// Advance the model by `dt_s` seconds with the meter reading the box polls
    /// (`reported`) and the car's actual draw. Grant changes only on the eval
    /// cadence; a cut ends the session immediately.
    pub fn tick(&mut self, dt_s: f32, reported: Ampere, car_draw: Ampere) {
        if !self.charging {
            return;
        }
        self.since_eval_s += dt_s;
        while self.charging && self.since_eval_s >= self.params.eval_period_s {
            self.since_eval_s -= self.params.eval_period_s;
            self.eval(reported, car_draw);
        }
    }

    /// One grant decision (the ~5–10 s cadence). The order encodes the observed
    /// priorities: a clear over-limit cuts; visible headroom is added on top of the
    /// live draw (the fast-up ratchet from #134); the region just above the limit
    /// is the parameterised unknown; exactly at the limit the box holds (#57).
    fn eval(&mut self, reported: Ampere, car_draw: Ampere) {
        let p = self.params;
        if reported.0 >= p.max_dip.0 + p.cut_margin.0 {
            self.lb = Ampere(0.0);
            self.charging = false;
        } else if reported.0 < p.max_dip.0 {
            let headroom = p.max_dip.0 - reported.0;
            self.lb = Ampere(round_amp(car_draw.0 + headroom)).clamp(Ampere(0.0), p.max_dip);
        } else if reported.0 > p.max_dip.0 {
            self.lb = Ampere(self.lb.0 - p.down_step.0).clamp(Ampere(0.0), p.max_dip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> BoxSimParams {
        BoxSimParams {
            max_dip: Ampere(16.0),
            eval_period_s: 10.0,
            cut_margin: Ampere(4.0),
            down_step: Ampere(0.0),
        }
    }

    fn charging_box(reported: f32) -> BoxSim {
        let mut b = BoxSim::new(params());
        assert!(b.try_start(Ampere(reported)));
        b
    }

    #[test]
    fn start_grant_is_rounded_headroom() {
        // Golden window 18:35:43: reported ≈ 9.5 → box opened at lb = 7.
        let mut b = BoxSim::new(params());
        assert!(b.try_start(Ampere(9.5)));
        assert_eq!(b.lb(), Ampere(7.0));
        assert!(b.charging());
    }

    #[test]
    fn start_denied_without_headroom() {
        let mut b = BoxSim::new(params());
        assert!(!b.try_start(Ampere(16.0)));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn eval_adds_headroom_to_car_draw() {
        // Golden window 18:36:07: car ≈ 7, reported ≈ 9 → lb jumped 7 → 14.
        let mut b = charging_box(9.5);
        b.tick(10.0, Ampere(9.0), Ampere(7.0));
        assert_eq!(b.lb(), Ampere(14.0));
    }

    #[test]
    fn up_grant_caps_at_dip_limit() {
        let mut b = charging_box(9.5);
        b.tick(10.0, Ampere(9.0), Ampere(12.0));
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn holds_while_meter_sits_at_limit() {
        // Golden window 18:36–18:38: reported pinned at 16 → lb held at 14.
        let mut b = charging_box(9.5);
        b.tick(10.0, Ampere(9.0), Ampere(7.0)); // -> 14
        b.tick(10.0, Ampere(16.0), Ampere(14.0));
        b.tick(10.0, Ampere(16.0), Ampere(14.0));
        assert_eq!(b.lb(), Ampere(14.0));
    }

    #[test]
    fn no_change_between_evals() {
        let mut b = charging_box(9.5);
        b.tick(5.0, Ampere(9.0), Ampere(7.0)); // half an eval period
        assert_eq!(b.lb(), Ampere(7.0));
    }

    #[test]
    fn cuts_when_reported_exceeds_limit_plus_margin() {
        // Pause failsafe reports MAX+4 = 20 → the box drops the session.
        let mut b = charging_box(9.5);
        b.tick(10.0, Ampere(20.0), Ampere(7.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn slightly_over_limit_holds_when_down_step_is_zero() {
        // H1 region (max..max+cut): with down_step 0 the box holds — the
        // conservative default until the live test measures a real down-step.
        let mut b = charging_box(9.5);
        b.tick(10.0, Ampere(9.0), Ampere(7.0)); // -> 14
        b.tick(10.0, Ampere(17.5), Ampere(14.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(14.0));
    }

    #[test]
    fn slightly_over_limit_down_steps_when_configured() {
        let mut b = BoxSim::new(BoxSimParams {
            down_step: Ampere(1.0),
            ..params()
        });
        assert!(b.try_start(Ampere(9.5)));
        b.tick(10.0, Ampere(9.0), Ampere(7.0)); // -> 14
        b.tick(10.0, Ampere(17.5), Ampere(14.0));
        b.tick(10.0, Ampere(17.5), Ampere(13.0));
        assert_eq!(b.lb(), Ampere(12.0));
        assert!(b.charging());
    }

    #[test]
    fn long_tick_spans_multiple_evals() {
        // Replay feeds ~1 s ticks, but the model must stay correct for coarser dt.
        let mut b = BoxSim::new(BoxSimParams {
            down_step: Ampere(1.0),
            ..params()
        });
        assert!(b.try_start(Ampere(9.5)));
        b.tick(10.0, Ampere(9.0), Ampere(7.0)); // -> 14
        b.tick(20.0, Ampere(17.5), Ampere(14.0)); // two evals -> two down-steps
        assert_eq!(b.lb(), Ampere(12.0));
    }

    #[test]
    fn idle_box_ignores_ticks() {
        let mut b = BoxSim::new(params());
        b.tick(30.0, Ampere(0.0), Ampere(0.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }
}
