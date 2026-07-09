//! Behavioural simulator of the EVC04's Power Optimizer (#135).
//!
//! Black-box model of the box's *internal* charge-limit loop, fitted to the
//! measured sessions in `core/tests/fixtures/sessions/`. The 2026-07-05
//! characterization campaign (sweep fixtures + `2026-07-05-sweep2-rep0-wire.log`)
//! collapsed the earlier three-branch model (headroom ratchet / dead zone /
//! proportional shed) into **one grant law evaluated on two clocks**:
//!
//! ```text
//! grant ← round(car_draw + (max_dip − reported))     — signed headroom on top of
//!                                                      the live draw
//! ```
//!
//! * **Down moves and cut checks run fast** (~4–6 s: rep-0 shed steps). The
//!   measured "−floor(excess) per eval" behaviour (probe 2026-07-02) *emerges*
//!   from this law with the car tracking its grant down; the ≤0.5 A "dead zone"
//!   is nothing but the rounding.
//! * **Session starts and up moves run slow** (~30 s: re-offer/up-grant gaps
//!   28.2 s in rep-0, engage latencies 17–66 s in sweep1b, post-cut re-engage
//!   28–32 s on the flag-day capture — the old separate restart cooldown was
//!   this same clock, reset by the cut).
//! * **Cuts**: a pause report (`reported ≥ max + cut_margin`) or a computed
//!   grant below the 6 A IEC pilot minimum. The latter is what really ended the
//!   probe +1.0 ride (fixture header: "session drop + self-recovery" — the old
//!   model wrongly held at 6) and the flag-day staircase (grant 4 at lb 8,
//!   reported 18).
//! * **Start law**: the box opens only when the opening grant
//!   `ceil(max_dip − reported)` *exceeds* `start_threshold`. Steady-state that
//!   threshold is 8 A = MAX/2 (sweep1/1c: eleven refusals at ≤8.0 A headroom
//!   over 5–6 min each; engages at ≥8.4 A). Right after meter silence the box
//!   opens down to the pilot floor instead (golden window opened at 6.5 A
//!   headroom on the OTA-heavy 2026-07-02; the 2026-07-05 morning post-OTA
//!   session at ~6 A) — the threshold is a fitted regime parameter, not a
//!   constant.
//!
//! Pure and clock-free like the rest of `core`: the caller owns time and feeds
//! elapsed seconds. Deviations knowingly not modelled: the ~10 s PWM ramp shape
//! of a cut (modelled as immediate), and the one measured opening grant of 5 at
//! 5.955 A headroom (2026-07-05 morning, post-OTA — a floor, where every other
//! measured opening is a ceil; single contaminated point, left as an outlier).

use super::control::Ampere;

/// Nearest-integer rounding for granted amps (the box's LOG only ever shows whole
/// amps). Hand-rolled because `f32::round` is libm-gated in `no_std`. Negative
/// inputs (deficit larger than the car draw) truncate toward zero, which is fine:
/// every such value is far below the pilot floor and cuts regardless.
fn round_amp(x: f32) -> f32 {
    (x + 0.5) as i32 as f32
}

/// Ceiling for the opening grant (sweep1b: 8.4→9, 8.5→9, 8.6→9, 8.9→9, 11→11).
/// Inputs are non-negative headrooms; `f32::ceil` is libm-gated in `no_std`.
fn ceil_amp(x: f32) -> f32 {
    let t = (x as i32) as f32;
    if x > t {
        t + 1.0
    } else {
        t
    }
}

/// Fit parameters of the box model. All tunable because they are *measured*, not
/// documented — the unit tests and the replay tests against the session fixtures
/// pin them down.
#[derive(Clone, Copy, Debug)]
pub struct BoxSimParams {
    /// The DIP-set Power Optimizer limit (§2); 16 A on the real install.
    pub max_dip: Ampere,
    /// The slow decision clock: session starts and upward grant moves (~30 s;
    /// rep-0 gaps 28.2 s, flag-day re-engage 28–32 s, engage latencies 17–66 s).
    pub up_period_s: f32,
    /// The fast decision clock: downward grant moves and cut checks (~4–6 s;
    /// rep-0 shed steps, probe −1 A per ~6 s).
    pub down_period_s: f32,
    /// Meter excess over `max_dip` at which the box cuts the charge (measured
    /// between +2 (still modulating) and +4 (cuts, #57)).
    pub cut_margin: Ampere,
    /// The opening grant must *exceed* this for a session to start: 8 (= MAX/2)
    /// steady-state (sweep1/1b/1c), 0 right after meter silence (golden window,
    /// post-OTA morning session).
    pub start_threshold: Ampere,
}

/// The IEC 61851 6 A pilot minimum. A computed grant below it drops the session:
/// the probe +1.0 ride cut right after reaching 6 (next eval computes 5 — fixture
/// header "session drop + self-recovery"), the flag-day staircase cut at computed
/// 4 (lb 8, car ~6, reported 18), and an offer the idle car never takes at zero
/// headroom computes 0 (flag-day start-cut).
const PILOT_MIN_A: f32 = 6.0;

/// The box's charge-limit state: the current grant (`lb_current` in the CN28 LOG)
/// and whether a session is active.
#[derive(Debug)]
pub struct BoxSim {
    params: BoxSimParams,
    lb: Ampere,
    charging: bool,
    since_up_s: f32,
    since_down_s: f32,
}

impl BoxSim {
    pub fn new(params: BoxSimParams) -> Self {
        Self {
            params,
            lb: Ampere(0.0),
            charging: false,
            since_up_s: 0.0,
            since_down_s: 0.0,
        }
    }

    /// The current grant to the car (`lb_current`).
    pub fn lb(&self) -> Ampere {
        self.lb
    }

    pub fn charging(&self) -> bool {
        self.charging
    }

    /// Advance the model by `dt_s` seconds with the meter reading the box polls
    /// (`reported`) and the car's actual draw. A plugged car asking to charge is
    /// implicit (every fixture session has one); the box refuses it through the
    /// start law alone. Coarse ticks apply one goal per boundary — the measured
    /// staircase shapes emerge from the car draw moving *between* ticks.
    pub fn tick(&mut self, dt_s: f32, reported: Ampere, car_draw: Ampere) {
        self.since_down_s += dt_s;
        self.since_up_s += dt_s;
        while self.since_down_s >= self.params.down_period_s {
            self.since_down_s -= self.params.down_period_s;
            self.down_eval(reported, car_draw);
        }
        while self.since_up_s >= self.params.up_period_s {
            self.since_up_s -= self.params.up_period_s;
            self.up_eval(reported, car_draw);
        }
    }

    /// The unified grant law: signed headroom on top of the live draw.
    fn goal(&self, reported: Ampere, car_draw: Ampere) -> f32 {
        round_amp(car_draw.0 + self.params.max_dip.0 - reported.0)
    }

    /// One fast decision: cut on a pause report, otherwise lower the grant to the
    /// goal — or drop the session when the goal falls below the pilot floor.
    fn down_eval(&mut self, reported: Ampere, car_draw: Ampere) {
        if !self.charging {
            return;
        }
        if reported.0 >= self.params.max_dip.0 + self.params.cut_margin.0 {
            return self.cut();
        }
        let goal = self.goal(reported, car_draw);
        if goal < self.lb.0 {
            if goal < PILOT_MIN_A {
                self.cut();
            } else {
                self.lb = Ampere(goal);
            }
        }
    }

    /// One slow decision: raise the grant to the goal, or open a session when the
    /// start law allows one.
    fn up_eval(&mut self, reported: Ampere, car_draw: Ampere) {
        if self.charging {
            let goal = self.goal(reported, car_draw).min(self.params.max_dip.0);
            if goal > self.lb.0 {
                self.lb = Ampere(goal);
            }
        } else {
            let opening = ceil_amp(self.params.max_dip.0 - reported.0);
            if opening > self.params.start_threshold.0 && opening >= PILOT_MIN_A {
                self.lb = Ampere(opening.min(self.params.max_dip.0));
                self.charging = true;
            }
        }
    }

    fn cut(&mut self) {
        self.lb = Ampere(0.0);
        self.charging = false;
        // The measured 28–32 s re-engage after every cut is the up clock
        // starting over, not a separate cooldown.
        self.since_up_s = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> BoxSimParams {
        BoxSimParams {
            max_dip: Ampere(16.0),
            up_period_s: 30.0,
            down_period_s: 5.0,
            cut_margin: Ampere(4.0),
            start_threshold: Ampere(8.0),
        }
    }

    /// Run the box to its first start decision (one full up period) at a constant
    /// pre-session meter reading.
    fn after_start_eval(reported: f32) -> BoxSim {
        let mut b = BoxSim::new(params());
        b.tick(30.0, Ampere(reported), Ampere(0.0));
        b
    }

    fn charging_box(reported: f32) -> BoxSim {
        let b = after_start_eval(reported);
        assert!(b.charging(), "setup: box must open at reported {reported}");
        b
    }

    #[test]
    fn opens_on_the_slow_cadence_when_the_grant_clears_half_max() {
        // sweep1b target 8.4: pre-session report 7.6 → engaged (23.2 s), grant 9.
        let mut b = BoxSim::new(params());
        b.tick(29.0, Ampere(7.6), Ampere(0.0));
        assert!(!b.charging(), "no session before the first start eval");
        b.tick(1.0, Ampere(7.6), Ampere(0.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(9.0));
    }

    #[test]
    fn refuses_at_eight_amps_headroom() {
        // sweep1/1c: 8.0 A headroom censored at 300 s twice; 6.05 (report 9.95)
        // censored at 360 s — the threshold sits at MAX/2, exclusive.
        let mut b = BoxSim::new(params());
        for _ in 0..30 {
            b.tick(10.0, Ampere(8.0), Ampere(0.0));
        }
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
        let mut b = BoxSim::new(params());
        for _ in 0..36 {
            b.tick(10.0, Ampere(9.95), Ampere(0.0));
        }
        assert!(!b.charging());
    }

    #[test]
    fn opening_grant_is_the_ceil_of_the_headroom() {
        // sweep1b: report 7.1 → grant 9 (ceil 8.9), report 5 → grant 11.
        assert_eq!(charging_box(7.1).lb(), Ampere(9.0));
        assert_eq!(charging_box(5.0).lb(), Ampere(11.0));
    }

    #[test]
    fn post_meter_silence_regime_opens_at_the_pilot_floor() {
        // Golden window 18:35:43 (2026-07-02, OTA-heavy day): reported ≈ 9.5 →
        // box opened at lb = 7 although the steady-state law refuses ≤ 8.
        let mut b = BoxSim::new(BoxSimParams {
            start_threshold: Ampere(0.0),
            ..params()
        });
        b.tick(30.0, Ampere(9.5), Ampere(0.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(7.0));
        // Even this regime refuses below the 6 A pilot minimum.
        let mut b = BoxSim::new(BoxSimParams {
            start_threshold: Ampere(0.0),
            ..params()
        });
        b.tick(30.0, Ampere(11.5), Ampere(0.0));
        assert!(!b.charging());
    }

    #[test]
    fn up_move_adds_headroom_to_the_live_draw_on_the_slow_clock() {
        // rep-0 wire t=330.9: car ~11.1 with reported 10 → grant 16 at the next
        // up eval, having sat at the opening 11 through the fast evals before.
        let mut b = charging_box(5.0); // opens at 11
        b.tick(29.0, Ampere(10.0), Ampere(11.1));
        assert_eq!(b.lb(), Ampere(11.0), "down clock must not raise the grant");
        b.tick(1.0, Ampere(10.0), Ampere(11.1));
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn up_grant_caps_at_the_dip_limit() {
        let mut b = charging_box(5.0);
        b.tick(30.0, Ampere(0.0), Ampere(12.0));
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn down_moves_ride_the_falling_car_on_the_fast_clock() {
        // rep-0 flip (wire t=357–369): lb 11-ish, car 10.18, reported 18 → 8;
        // car 8.14 → 6; then reported 16, car 6.14 → holds at 6 (34 s measured).
        let mut b = charging_box(5.0); // lb 11
        b.tick(5.0, Ampere(18.0), Ampere(10.18));
        assert_eq!(b.lb(), Ampere(8.0));
        b.tick(5.0, Ampere(18.0), Ampere(8.14));
        assert_eq!(b.lb(), Ampere(6.0));
        for _ in 0..6 {
            b.tick(5.0, Ampere(16.0), Ampere(6.14));
        }
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(6.0));
    }

    /// Box charging flat-out at 16 A, as in the probe measurement session.
    fn box_at_full_grant() -> BoxSim {
        let b = charging_box(0.0);
        assert_eq!(b.lb(), Ampere(16.0));
        b
    }

    #[test]
    fn holds_inside_the_rounding_dead_zone_over_the_limit() {
        // Probe stage +0.5 (21:34:48–21:35:48): 60 s at reported 16.5, lb stayed
        // 16 — round(16 − 0.5) = 16. The dead zone is the rounding.
        let mut b = box_at_full_grant();
        b.tick(60.0, Ampere(16.5), Ampere(16.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn one_amp_over_rides_down_one_amp_per_eval() {
        // Probe stage +1.0 (21:36:40–21:37:41): lb walked 16→…→6, −1 A per eval,
        // the car tracking ~0.15 A above each new grant.
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(17.0), Ampere(16.15));
        assert_eq!(b.lb(), Ampere(15.0));
        b.tick(5.0, Ampere(17.0), Ampere(15.15));
        assert_eq!(b.lb(), Ampere(14.0));
        assert!(b.charging());
    }

    #[test]
    fn fractional_excess_still_steps_whole_amps() {
        // Probe stage +1.5 (21:39:23–21:39:47): same −1 A per eval as +1.0 —
        // round(16 − 1.5) = 15.
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(17.5), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(15.0));
    }

    #[test]
    fn two_amps_over_sheds_two_per_eval_without_cutting() {
        // Probe stage +2.0 (21:41:33–21:41:48): −2 A per eval, no cut — the cut
        // threshold sits above +2 (#57: pause at +4 cuts).
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(18.0), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(14.0));
        b.tick(5.0, Ampere(18.0), Ampere(14.0));
        assert_eq!(b.lb(), Ampere(12.0));
        assert!(b.charging());
    }

    #[test]
    fn the_ride_cuts_below_the_pilot_floor() {
        // Probe +1.0 stage end (21:37:37–21:37:41): lb reached 6, one more eval
        // at reported 17 with the car at ~6.15 computes 5 → session drop
        // (fixture header: "session drop + self-recovery").
        let mut b = box_at_full_grant();
        let mut car = 16.15;
        while b.lb().0 > 6.0 {
            b.tick(5.0, Ampere(17.0), Ampere(car));
            car = b.lb().0 + 0.15;
        }
        assert!(b.charging());
        b.tick(5.0, Ampere(17.0), Ampere(6.15));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn a_shed_landing_below_the_pilot_floor_cuts() {
        // Flag-day staircase (2026-07-03-flagday-staircase.log, t=522–527): lb 8,
        // car ~6, reported 18 → round(6 − 2) = 4 → the box dropped the session.
        let mut b = charging_box(7.1); // lb 9
        b.tick(5.0, Ampere(18.0), Ampere(8.0)); // ride to 6 first
        assert_eq!(b.lb(), Ampere(6.0));
        b.tick(5.0, Ampere(18.0), Ampere(6.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn the_pause_report_cuts_within_one_fast_eval() {
        // rep-0 wire t=391–395: enable flap → pause report 20 → Stop Pwm ~4 s
        // later, with the car still drawing 6 A.
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(20.0), Ampere(16.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn an_idle_car_at_the_ceiling_cuts() {
        // Flag-day capture 2026-07-03 (flagday-start-cut.log): grant 16, meter at
        // 16 with the car at 0 → the computed grant is 0 → PWM withdrawn within
        // one eval.
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(16.0), Ampere(0.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    #[test]
    fn an_idle_car_with_headroom_keeps_the_offer() {
        // rep-0 wire t=302–330: opening grant 11 stood ~28 s at reported 5 with
        // the car not yet drawing (contactor lag) — the computed grant equals the
        // standing offer.
        let mut b = charging_box(5.0); // lb 11
        b.tick(28.0, Ampere(5.0), Ampere(0.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(11.0));
    }

    #[test]
    fn a_cut_resets_the_start_clock() {
        // Flag-day capture: cut → cp back to C ~30 s later, every cycle (144→174,
        // 188→218, 272→305, 441→472) — the restart wait is the up clock, reset by
        // the cut.
        let mut b = box_at_full_grant();
        b.tick(5.0, Ampere(20.0), Ampere(16.0)); // pause-cut
        assert!(!b.charging());
        b.tick(25.0, Ampere(0.0), Ampere(0.0));
        assert!(!b.charging(), "restart only after a full up period");
        b.tick(5.0, Ampere(0.0), Ampere(0.0));
        assert!(b.charging());
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn no_down_move_between_fast_evals() {
        let mut b = box_at_full_grant();
        b.tick(4.0, Ampere(18.0), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(16.0));
    }

    #[test]
    fn a_coarse_tick_applies_the_goal_not_a_step_per_eval() {
        // The staircase shape of a ride comes from the car falling between evals;
        // with the draw frozen for a whole coarse tick the goal is reached once
        // and then holds.
        let mut b = box_at_full_grant();
        b.tick(20.0, Ampere(17.0), Ampere(16.0));
        assert_eq!(b.lb(), Ampere(15.0));
    }

    #[test]
    fn idle_box_ignores_ticks() {
        let mut b = BoxSim::new(params());
        b.tick(300.0, Ampere(16.0), Ampere(0.0));
        assert!(!b.charging());
        assert_eq!(b.lb(), Ampere(0.0));
    }

    /// Close the loop: the firmware's `grant_tracking_current` drives the fitted box,
    /// the car draws toward its grant (contactor lag), and the cold-start kick latch
    /// advances each tick — the same wiring the firmware runs. Returns the box after
    /// `secs` of a cold start at `target`.
    fn cold_start_closed_loop(target: f32, kick: bool, secs: u32) -> BoxSim {
        use crate::charge::control::{
            grant_tracking_current, startup_kick_armed, GrantControlInputs,
        };
        let mut b = BoxSim::new(params()); // steady-state regime: start_threshold 8
        let mut car = 0.0f32;
        let mut armed = true;
        for _ in 0..(secs / 5) {
            let lb = b.lb().0;
            // Enabled, target ≥ floor, fresh feedback: never pausing here.
            armed = startup_kick_armed(armed, false, Ampere(lb));
            let reported = grant_tracking_current(&GrantControlInputs {
                max: Ampere(16.0),
                min_charge: Ampere(6.0),
                pause_margin: Ampere(4.0),
                max_over: Ampere(2.0),
                target: Some(Ampere(target)),
                lb: Ampere(lb),
                car: Ampere(car),
                lb_stale: false,
                grid_stale: false,
                enabled: true,
                startup_kick: kick && armed,
            })
            .0;
            b.tick(5.0, Ampere(reported), Ampere(car));
            // The car ramps toward its grant, never past what it wants (target + 1).
            let want = b.lb().0.min(target + 1.0);
            car += (want - car) * 0.5;
        }
        b
    }

    #[test]
    fn cold_start_at_the_floor_needs_the_kick_to_open() {
        // Live 2026-07-09: at target 6 the deficit report `max − target` = 10 has the box
        // compute an opening of ceil(16 − 10) = 6 ≤ start_threshold 8, so the fitted box
        // never opens — exactly the stuck-B stall. Without the kick it must stay closed.
        let no_kick = cold_start_closed_loop(6.0, false, 200);
        assert!(
            !no_kick.charging(),
            "the deficit report never opens a 6 A cold session"
        );
        assert_eq!(no_kick.lb(), Ampere(0.0));
        // The full-offer kick reports 0 → opening ceil(16) = 16 > 8 → the box opens; once
        // it grants, the latch disarms and the loop settles to a valid session at the floor.
        let kicked = cold_start_closed_loop(6.0, true, 200);
        assert!(
            kicked.charging(),
            "the kick opens the cold session the box otherwise refuses"
        );
        assert!(
            kicked.lb().0 >= 6.0,
            "and the loop holds a valid ≥6 A grant"
        );
    }
}
