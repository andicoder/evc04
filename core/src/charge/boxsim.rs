//! Behavioural simulator of the EVC04's Power Optimizer (#135).
//!
//! Black-box model of the box's *internal* charge-limit loop, fitted to the
//! measured sessions in `core/tests/fixtures/sessions/` (analysis #134). The box's
//! firmware is closed; what we model is the observed rule: on a ~6 s cadence the
//! box grants the car `car_draw + (DIP limit − reported meter value)` — apparent
//! headroom is *added on top of the current draw* — holds while the meter sits
//! exactly at the limit, and hard-cuts once the meter exceeds the limit by a margin
//! (#57). The region between "at the limit" and the cut threshold was measured on
//! hardware (#135 step 6, `2026-07-02-probe-measurement.log`): a dead zone up to
//! ~0.5 A over, then a *proportional* down-regulation of `floor(excess)` amps per
//! eval (+1.0/+1.5 → −1 A per ~6 s, +2.0 → −2 A per ~6 s), with no cut at +2.0.
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
    /// Meter excess over `max_dip` at which the box cuts the charge (measured
    /// between +2 (still modulating) and +4 (cuts, #57)).
    pub cut_margin: Ampere,
    /// Excess over `max_dip` the box ignores before down-regulating (measured
    /// ≤0.5 A, #135 step 6). Above it the box sheds `floor(excess)` per eval.
    pub down_dead_zone: Ampere,
    /// How long the box refuses a new session after a cut (measured ~30 s on the
    /// flag-day capture 2026-07-03: cut → cp back to C after 28–32 s, every cycle).
    pub restart_cooldown_s: f32,
}

/// Car draw below which the box treats the session as "not charging": at the
/// ceiling it then withdraws the PWM instead of dead-zone-holding (flag-day
/// capture 2026-07-03 — the hold was only ever measured with the car drawing).
const IDLE_CAR_FLOOR: f32 = 1.0;

/// The box's charge-limit state: the current grant (`lb_current` in the CN28 LOG)
/// and whether a session is active.
#[derive(Debug)]
pub struct BoxSim {
    params: BoxSimParams,
    lb: Ampere,
    charging: bool,
    since_eval_s: f32,
    cooldown_s: f32,
}

impl BoxSim {
    pub fn new(params: BoxSimParams) -> Self {
        Self {
            params,
            lb: Ampere(0.0),
            charging: false,
            since_eval_s: 0.0,
            cooldown_s: 0.0,
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
        if self.cooldown_s > 0.0 {
            return false;
        }
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
        self.cooldown_s = (self.cooldown_s - dt_s).max(0.0);
        if !self.charging {
            return;
        }
        self.since_eval_s += dt_s;
        while self.charging && self.since_eval_s >= self.params.eval_period_s {
            self.since_eval_s -= self.params.eval_period_s;
            self.eval(reported, car_draw);
        }
    }

    /// One grant decision (the ~6 s cadence). The order encodes the observed
    /// priorities: a clear over-limit cuts; visible headroom is added on top of the
    /// live draw (the fast-up ratchet from #134); past the dead zone the box sheds
    /// the excess proportionally (#135 step 6); at the limit (and inside the dead
    /// zone) it holds (#57).
    fn eval(&mut self, reported: Ampere, car_draw: Ampere) {
        let p = self.params;
        if reported.0 >= p.max_dip.0 + p.cut_margin.0
            || (reported.0 >= p.max_dip.0 && car_draw.0 < IDLE_CAR_FLOOR)
        {
            self.lb = Ampere(0.0);
            self.charging = false;
            self.cooldown_s = p.restart_cooldown_s;
        } else if reported.0 < p.max_dip.0 {
            let headroom = p.max_dip.0 - reported.0;
            self.lb = Ampere(round_amp(car_draw.0 + headroom)).clamp(Ampere(0.0), p.max_dip);
        } else if reported.0 > p.max_dip.0 + p.down_dead_zone.0 {
            // floor(excess), at least 1 past the dead zone: the measured shed rate
            // (−1 at +1.0 *and* +1.5, −2 at +2.0). Truncation is floor here —
            // excess is positive (`f32::floor` is libm-gated in no_std).
            let excess = reported.0 - p.max_dip.0;
            let step = if excess < 1.0 {
                1.0
            } else {
                excess as i32 as f32
            };
            self.lb = Ampere(self.lb.0 - step).clamp(Ampere(0.0), p.max_dip);
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
            down_dead_zone: Ampere(0.5),
            restart_cooldown_s: 30.0,
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

    /// Box charging flat-out at 16 A, as in the probe measurement session.
    fn box_at_full_grant() -> BoxSim {
        let mut b = BoxSim::new(params());
        assert!(b.try_start(Ampere(0.0)));
        assert_eq!(b.lb(), Ampere(16.0));
        b
    }

    #[test]
    fn holds_inside_the_dead_zone_over_the_limit() {
        // Probe stage +0.5 (21:34:48–21:35:48): 60 s at reported 16.5, lb stayed 16.
        let mut b = box_at_full_grant();
        b.tick(60.0, Ampere(16.5), Ampere(16.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn one_amp_over_steps_one_amp_per_eval() {
        // Probe stage +1.0 (21:36:40–21:37:41): lb walked 16→…→6, −1 A per ~6 s.
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(17.0), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(15.0));
        b.tick(10.0, Ampere(17.0), Ampere(15.0));
        assert_eq!(b.lb(), Ampere(14.0));
        assert!(b.charging());
    }

    #[test]
    fn fractional_excess_still_steps_whole_amps() {
        // Probe stage +1.5 (21:39:23–21:39:47): same −1 A per eval as +1.0 — the
        // step is the floor of the excess, not its rounding.
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(17.5), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(15.0));
    }

    #[test]
    fn two_amps_over_steps_two_amps_per_eval_without_cutting() {
        // Probe stage +2.0 (21:41:33–21:41:48): −2 A per eval, still no cut — the
        // cut threshold sits above +2 (#57: pause at +4 cuts).
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(18.0), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(14.0));
        assert!(b.charging());
    }

    #[test]
    fn down_ride_floors_at_zero() {
        let mut b = box_at_full_grant();
        for _ in 0..20 {
            let lb = b.lb();
            // Keep the car drawing at the grant (min 1 A) so the ride is the
            // proportional shed, not the idle-at-ceiling cut.
            b.tick(10.0, Ampere(18.0), Ampere(lb.0.max(1.0)));
        }
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn an_idle_car_at_the_ceiling_cuts_within_one_eval() {
        // Flag-day capture 2026-07-03 (2026-07-03-flagday-start-cut.log): grant 16,
        // meter jumps to 16 while the car still draws 0 → PWM withdrawn one eval
        // (~4–6 s) later, repeated every cycle.
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(16.0), Ampere(0.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn an_idle_car_below_the_ceiling_keeps_the_grant() {
        // Same capture: reported 0/4 with the car at 0 never cut — the box waits
        // with the offer standing as long as the meter shows headroom.
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(4.0), Ampere(0.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(12.0));
    }

    #[test]
    fn restart_is_refused_during_the_post_cut_cooldown() {
        // Capture: cut → cp back to C ~30 s later, every cycle (144→174, 188→218,
        // 272→305, 441→472).
        let mut b = box_at_full_grant();
        b.tick(10.0, Ampere(16.0), Ampere(0.0)); // idle-at-ceiling cut
        assert!(!b.try_start(Ampere(0.0)));
        b.tick(20.0, Ampere(0.0), Ampere(0.0)); // 20 s of cooldown left
        assert!(!b.try_start(Ampere(0.0)));
        b.tick(15.0, Ampere(0.0), Ampere(0.0)); // past the ~30 s
        assert!(b.try_start(Ampere(0.0)));
    }

    #[test]
    fn long_tick_spans_multiple_evals() {
        // Replay feeds ~1 s ticks, but the model must stay correct for coarser dt.
        let mut b = box_at_full_grant();
        b.tick(20.0, Ampere(17.0), Ampere(16.0)); // two evals -> two down-steps
        assert_eq!(b.lb(), Ampere(14.0));
    }

    #[test]
    fn idle_box_ignores_ticks() {
        let mut b = BoxSim::new(params());
        b.tick(30.0, Ampere(0.0), Ampere(0.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }
}
