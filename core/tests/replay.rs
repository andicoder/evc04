//! Replay harness (evc04#135 steps 3+4): the *real* `core` controller against the
//! `boxsim` box model, an EV model and the measured-value pipeline as deployed
//! today — proving offline that the controller reproduces the measured sessions in
//! `tests/fixtures/sessions/`, most importantly the #119 symptom ("target 6,
//! charged ~15 A"). Scenario timings/values are hand-copied from those fixtures
//! (cited per test); the CSVs remain the provenance record.
//!
//! The EV and grid models live here (std test code) rather than in the `no_std`
//! lib: they are simulation scaffolding, not product.

use evc04_cn28_core::charge::boxsim::{BoxSim, BoxSimParams};
use evc04_cn28_core::charge::control::{
    grant_tracking_current, ramp_step, reported_current, trim_decay, trim_step, Ampere,
    ControlInputs, FailsafeMode, GrantControlInputs,
};

// Mirrors of the firmware constants (firmware/src/charge.rs) — the replay must run
// exactly the shipped configuration.
const MAX_BOX: f32 = 16.0;
const MIN_CHARGE: f32 = 6.0;
const PAUSE_MARGIN: f32 = 4.0;
const RAMP_PER_S: f32 = 0.5;
const TRIM_KI: f32 = 0.5;
const TRIM_MAX: f32 = 8.0;
const TRIM_PERIOD_S: u32 = 5;
/// Today's HA automation publishes `max(grid_w/690, 0)` every 5 s (#134 H2).
const MEASURED_PERIOD_S: u32 = 5;
/// The box recomputes the CN28 LOG metering fields (incl. `lb_current`) only ~every
/// 5 s (#119) — the V4 feedback is that stale.
const CN28_PERIOD_S: u32 = 5;
/// V4 caps the over-report at +2: the strongest *measured* shed rate (−2 A/eval)
/// while staying clearly below the cut threshold (>2, ≤4).
const LB_TRACKING_MAX_OVER: f32 = 2.0;

/// The car: follows the box's grant with a finite ramp; below the IEC 6 A pilot
/// minimum it draws nothing. Rates fitted from the fixtures (~0.5 A/s up, fast
/// ramp-down on a cut).
struct Ev {
    amps: f32,
    /// Contactor lag: how long a ≥6 A offer must stand before the car draws.
    /// The flag-day capture (2026-07-03) showed the real car needs 10–30 s; the
    /// fixture-fitted scenarios predate that measurement and run with 0.
    start_lag_s: f32,
    offered_s: f32,
}

impl Ev {
    fn new(start_lag_s: f32) -> Self {
        Ev {
            amps: 0.0,
            start_lag_s,
            offered_s: 0.0,
        }
    }
    fn tick(&mut self, dt: f32, lb: f32) {
        if lb >= MIN_CHARGE {
            self.offered_s += dt;
        } else {
            self.offered_s = 0.0;
        }
        let ready = self.offered_s >= self.start_lag_s;
        let target = if lb >= MIN_CHARGE && ready {
            lb.min(16.0)
        } else {
            0.0
        };
        let rate = if target >= self.amps { 0.5 } else { 2.5 };
        let step = rate * dt;
        if (target - self.amps).abs() <= step {
            self.amps = target;
        } else if target > self.amps {
            self.amps += step;
        } else {
            self.amps -= step;
        }
    }
}

/// How the controller turns its state into the meter answer.
enum Reporting {
    /// The shipped path: `reported_current` (upper clamp at `MAX_BOX` — H1).
    AsShipped,
    /// Variant V1 (#135 step 5): allow reporting slightly *above* the limit, capped
    /// below the box's cut threshold, so the box can see "over limit" without
    /// dropping the session. `trim` disabled — the open clamp replaces its job.
    OpenClamp { cap: f32 },
    /// Variant V4 (#135 step 5): regulate the box's grant directly on the ~5 s
    /// CN28 `lb_current` feedback — no offset ramp, no measured, no trim.
    LbTracking,
}

struct Scenario {
    /// (second, target_amps, enabled) — controller inputs, from the fixture.
    events: Vec<(u32, Option<f32>, bool)>,
    /// Constant non-car household draw feeding the grid meter [W].
    house_w: f32,
    /// Constant PV production [W]; export makes the grid reading negative, which
    /// today's HA pipeline clamps to 0 (#134 H2).
    pv_w: f32,
    duration_s: u32,
    reporting: Reporting,
    box_params: BoxSimParams,
    ev_start_lag_s: f32,
}

#[derive(Debug)]
struct Sample {
    t: u32,
    target: Option<f32>,
    reported: f32,
    lb: f32,
    car: f32,
}

/// Run the full loop at 1 s ticks and record one sample per second.
fn run(sc: &Scenario) -> Vec<Sample> {
    let mut boxsim = BoxSim::new(sc.box_params);
    let mut ev = Ev::new(sc.ev_start_lag_s);
    let mut target: Option<f32> = None;
    let mut enabled = true;
    let mut offset = MAX_BOX; // firmware cold-start
    let mut trim = 0.0_f32;
    let mut measured = 0.0_f32;
    let mut cn28_lb = 0.0_f32;
    let mut cn28_car = 0.0_f32;
    let mut out = Vec::new();

    for t in 0..sc.duration_s {
        for &(at, tgt, en) in &sc.events {
            if at == t {
                target = tgt;
                enabled = en;
            }
        }

        // HA pipeline: grid = car + house − PV, published every 5 s, clamped ≥ 0.
        if t % MEASURED_PERIOD_S == 0 {
            let grid_w = ev.amps * 690.0 + sc.house_w - sc.pv_w;
            measured = (grid_w / 690.0).max(0.0);
        }

        if let Some(tgt) = target {
            offset = ramp_step(
                Ampere(offset),
                Ampere(MAX_BOX - tgt),
                Ampere(RAMP_PER_S * 1.0),
            )
            .0;
        }

        // #119 trim on its 5 s cadence, fed by the box-measured car draw (CN28).
        if t % TRIM_PERIOD_S == 0 {
            trim = match (&sc.reporting, target) {
                (Reporting::AsShipped, Some(tgt)) => {
                    trim_step(
                        Ampere(trim),
                        Ampere(ev.amps),
                        Ampere(tgt),
                        TRIM_KI,
                        Ampere(TRIM_MAX),
                    )
                    .0
                }
                (Reporting::AsShipped, None) => trim_decay(Ampere(trim), Ampere(1.0)).0,
                (Reporting::OpenClamp { .. } | Reporting::LbTracking, _) => 0.0,
            };
        }

        // CN28 telemetry: the box refreshes the LOG metering fields only ~every
        // 5 s, so the V4 feedback lags the true grant by up to one period.
        if t % CN28_PERIOD_S == 0 {
            cn28_lb = boxsim.lb().0;
            cn28_car = ev.amps;
        }

        let reported = match sc.reporting {
            Reporting::AsShipped => {
                reported_current(&ControlInputs {
                    max: Ampere(MAX_BOX),
                    min_charge: Ampere(MIN_CHARGE),
                    pause_margin: Ampere(PAUSE_MARGIN),
                    target: target.map(Ampere),
                    offset: Ampere(offset),
                    trim: Ampere(trim),
                    measured: Ampere(measured),
                    enabled,
                    target_stale: false,
                    measured_stale: false,
                    target_failsafe: FailsafeMode::Pause,
                    measured_failsafe: FailsafeMode::Pause,
                })
                .0
            }
            Reporting::OpenClamp { cap } => {
                if !enabled || target.is_none() || target.is_some_and(|t| t < MIN_CHARGE) {
                    MAX_BOX + PAUSE_MARGIN
                } else {
                    (offset + measured).clamp(0.0, cap)
                }
            }
            Reporting::LbTracking => {
                grant_tracking_current(&GrantControlInputs {
                    max: Ampere(MAX_BOX),
                    min_charge: Ampere(MIN_CHARGE),
                    pause_margin: Ampere(PAUSE_MARGIN),
                    max_over: Ampere(LB_TRACKING_MAX_OVER),
                    target: target.map(Ampere),
                    lb: Ampere(cn28_lb),
                    car: Ampere(cn28_car),
                    lb_stale: false,
                    grid_stale: false,
                    enabled,
                })
                .0
            }
        };

        if enabled && !boxsim.charging() {
            boxsim.try_start(Ampere(reported));
        }
        boxsim.tick(1.0, Ampere(reported), Ampere(ev.amps));
        ev.tick(1.0, boxsim.lb().0);

        out.push(Sample {
            t,
            target,
            reported,
            lb: boxsim.lb().0,
            car: ev.amps,
        });
    }
    out
}

fn fitted_box() -> BoxSimParams {
    BoxSimParams {
        max_dip: Ampere(MAX_BOX),
        // Measured on hardware (#135 step 6, 2026-07-02-probe-measurement.log):
        // −1 A per ~6 s at +1.0/+1.5 over, −2 A per ~6 s at +2.0, dead zone ≤0.5.
        eval_period_s: 6.0,
        cut_margin: Ampere(PAUSE_MARGIN),
        down_dead_zone: Ampere(0.5),
        // Measured on the flag-day capture 2026-07-03: cut → re-engage ~30 s.
        restart_cooldown_s: 30.0,
    }
}

/// Fixture `2026-06-30-target6-pinned15.csv`: evcc offers 6 (18:21:17), 16
/// (18:21:48), 15, 14, then 13 (18:23:17); house ≈ 350 W, no PV. Observed: the box
/// ratchets to 16, then sits at ~15 A for the rest of the session although evcc
/// wanted 13 — the #119 symptom this harness must reproduce deterministically.
fn scenario_2026_06_30() -> Scenario {
    Scenario {
        events: vec![
            (137, Some(6.0), true),
            (168, Some(16.0), true),
            (195, Some(15.0), true),
            (197, Some(14.0), true),
            (257, Some(13.0), true),
        ],
        house_w: 352.0,
        pv_w: 0.0,
        duration_s: 900,
        reporting: Reporting::AsShipped,
        box_params: fitted_box(),
        ev_start_lag_s: 0.0,
    }
}

#[test]
fn replay_2026_06_30_reproduces_pinned_high_charge() {
    let trace = run(&scenario_2026_06_30());
    let end = &trace[850];
    assert_eq!(end.target, Some(13.0));
    // Bug reproduced: the car sits well above the target (fixture: ~15.2 A) …
    assert!(end.car >= 14.0, "expected pinned-high charge, got {end:?}");
    // … while the meter answer is pinned at the upper clamp (H1): the box is never
    // told "over the limit", so it never comes down.
    assert!(
        (end.reported - MAX_BOX).abs() < 0.01,
        "expected reported pinned at MAX, got {end:?}"
    );
}

/// Fixture `2026-07-02-golden-window.csv`: targets step 6→7→8→9→10→6 while the
/// published measured value stayed 0 the whole window (PV export + the ≥0 clamp —
/// H2). Observed: start-grant ~7, ratchet to 14 within a minute, full 16 later; cut
/// after enable=false. With measured dead, the loop is open and only the trim
/// pushes — into saturation.
fn scenario_golden_window() -> Scenario {
    Scenario {
        events: vec![
            // Pre-window state (fixture 16:30): evcc stopped (enable off) with the
            // last target 6 latched, so the offset sits at 10 — the start-grant ~7
            // only reproduces from that state, not from a cold start.
            (0, Some(6.0), false),
            (60, Some(6.0), true),
            (72, Some(7.0), true),
            (132, Some(8.0), true),
            (192, Some(9.0), true),
            (252, Some(10.0), true),
            (312, Some(6.0), true),
            (546, Some(6.0), false),
        ],
        house_w: 300.0,
        pv_w: 12_000.0, // deep export: grid negative → measured clamps to 0 (H2)
        duration_s: 600,
        reporting: Reporting::AsShipped,
        box_params: fitted_box(),
        ev_start_lag_s: 0.0,
    }
}

#[test]
fn replay_golden_window_reproduces_ratchet_to_full() {
    let trace = run(&scenario_golden_window());
    let start = trace.iter().find(|s| s.lb > 0.0).expect("charge starts");
    assert!(
        (6.0..=8.0).contains(&start.lb),
        "start grant should be the rounded headroom (~7), got {start:?}"
    );
    // Within ~90 s the ratchet has pushed the grant far above the ≤8 A target.
    let early = &trace[start.t as usize + 90];
    assert!(
        early.lb >= 12.0,
        "expected the headroom ratchet despite target ≤8, got {early:?}"
    );
    // The session ends via the enable=false pause, not earlier.
    let cut = trace.iter().find(|s| s.lb == 0.0 && s.t > start.t).unwrap();
    assert!(
        cut.t >= 546 && cut.t <= 570,
        "expected cut only after enable=false, got {cut:?}"
    );
}

/// Bridge to #135 step 5 (variant V1): open the upper clamp so the box can see
/// "slightly over the limit" (below its cut threshold). With the *measured* down
/// response in the box model, the open clamp alone already converges.
#[test]
fn open_clamp_converges_to_target() {
    let mut sc = scenario_2026_06_30();
    sc.reporting = Reporting::OpenClamp { cap: 19.0 };
    let end = &run(&sc)[850];
    assert!(
        (end.car - 13.0).abs() <= 1.5,
        "with the measured down response the loop should settle near the 13 A target, got {end:?}"
    );
}

/// The #135 step-5 acceptance run for variant V4: the planned live-test staircase
/// (16→12→10→8→6→8→16) under deep PV export — the exact scenario that broke the
/// shipped controller (H2: measured clamped to 0, loop open). V4 never looks at
/// `measured`, so the export can't hurt it; the box's measured down response
/// (#135 step 6) does the actual shedding.
///
/// The eval period is only known as ~5–10 s (fitted: 6 s), and the ~5 s CN28
/// feedback lag beats against it, so the proof must hold across the whole range —
/// including the phase alignments where a stale grant makes V4 over-report one
/// eval too long.
#[test]
fn lb_tracking_holds_every_target_across_the_band() {
    let steps: [(u32, f32); 7] = [
        (0, 16.0),
        (120, 12.0),
        (240, 10.0),
        (360, 8.0),
        (480, 6.0),
        (600, 8.0),
        (720, 16.0),
    ];
    for eval_period_s in [5.0, 6.0, 7.0, 8.0, 9.0, 10.0] {
        // Lag 0 = the fixture-fitted instant car; 15 s = the measured contactor
        // lag of the real car (flag-day capture 2026-07-03).
        for ev_start_lag_s in [0.0, 15.0] {
            let sc = Scenario {
                events: steps.iter().map(|&(t, a)| (t, Some(a), true)).collect(),
                house_w: 300.0,
                pv_w: 12_000.0, // deep export: the H2 scenario the old loop couldn't survive
                duration_s: 840,
                reporting: Reporting::LbTracking,
                box_params: BoxSimParams {
                    eval_period_s,
                    ..fitted_box()
                },
                ev_start_lag_s,
            };
            let trace = run(&sc);

            // The session must survive the whole staircase: once the car draws,
            // the box never cuts (lb never returns to 0) and the car never
            // stalls below 6 A.
            let start = trace
                .iter()
                .find(|s| s.car >= 1.0)
                .expect("charge starts");
            for s in trace.iter().skip(start.t as usize + 30) {
                assert!(
                    s.lb > 0.0,
                    "box cut the session (eval {eval_period_s}, lag {ev_start_lag_s}) at {s:?}"
                );
                assert!(
                    s.car >= MIN_CHARGE,
                    "car stalled (eval {eval_period_s}, lag {ev_start_lag_s}) at {s:?}"
                );
            }

            // Each step has settled onto its target (±1 A, grants are whole amps)
            // well before the next one — no pin at 15/16, no undershoot.
            for &(at, tgt) in &steps {
                let settled = &trace[(at + 110) as usize];
                assert!(
                    (settled.car - tgt).abs() <= 1.0,
                    "target {tgt} not held 110 s after the step (eval {eval_period_s}, lag {ev_start_lag_s}), got {settled:?}"
                );
                assert!(
                    (settled.lb - tgt).abs() <= 1.0,
                    "grant off target {tgt} 110 s after the step (eval {eval_period_s}, lag {ev_start_lag_s}), got {settled:?}"
                );
            }
        }
    }
}

/// Flag-day regression (capture `2026-07-03-flagday-start-cut.log`): the real car
/// needs 10–30 s before it draws. V4 as shipped snapped `reported` to MAX the
/// moment the box granted; the box saw "meter at the limit, car idle" and withdrew
/// the PWM one eval later — a ~40 s grant/cut cycle ("Ladegerät nicht bereit"),
/// the charge never started.
#[test]
fn lb_tracking_starts_a_slow_starting_car() {
    for eval_period_s in [5.0, 6.0, 7.0, 8.0, 9.0, 10.0] {
        let sc = Scenario {
            events: vec![(0, Some(16.0), true)],
            house_w: 300.0,
            pv_w: 12_000.0,
            duration_s: 300,
            reporting: Reporting::LbTracking,
            box_params: BoxSimParams {
                eval_period_s,
                ..fitted_box()
            },
            ev_start_lag_s: 15.0,
        };
        let trace = run(&sc);
        let late = &trace[240];
        assert!(
            late.car >= MIN_CHARGE,
            "car never started (eval {eval_period_s}): {late:?}"
        );
        // Once the car draws, the session must hold — no further grant/cut cycle.
        let first_draw = trace.iter().find(|s| s.car >= 1.0).unwrap().t as usize;
        for s in trace.iter().skip(first_draw) {
            assert!(
                s.lb > 0.0,
                "cut after the car started (eval {eval_period_s}): {s:?}"
            );
        }
    }
}

/// Replay of the #135 step-6 probe measurement (`2026-07-02-probe-measurement.log`):
/// the staged over-limit reports are fed verbatim; the fitted model must reproduce
/// the measured lb ride within one eval step. This is the fixture that pins the
/// down-regulation parameters.
#[test]
fn replay_probe_measurement_reproduces_the_down_ride() {
    fn stage(boxsim: &mut BoxSim, ev: &mut Ev, secs: u32, reported: f32) -> f32 {
        for _ in 0..secs {
            boxsim.tick(1.0, Ampere(reported), Ampere(ev.amps));
            ev.tick(1.0, boxsim.lb().0);
        }
        boxsim.lb().0
    }

    let mut bx = BoxSim::new(fitted_box());
    let mut ev = Ev::new(0.0);
    assert!(bx.try_start(Ampere(0.0)));

    // Warm-up at reported 0: full grant, car reaches 16 A (21:33:54 baseline).
    assert_eq!(stage(&mut bx, &mut ev, 60, 0.0), 16.0);
    // Stage +0.5 (60 s): dead zone — lb held at 16.
    assert_eq!(stage(&mut bx, &mut ev, 60, 16.5), 16.0);
    assert_eq!(stage(&mut bx, &mut ev, 50, 0.0), 16.0);
    // Stage +1.0 (60 s): measured ride 16→6, −1 A per ~6 s, no cut.
    assert_eq!(stage(&mut bx, &mut ev, 30, 17.0), 11.0);
    assert_eq!(stage(&mut bx, &mut ev, 30, 17.0), 6.0);
    assert!(bx.charging());
    // Probe off: the box re-grants (log: back at 16 within ~40 s).
    assert_eq!(stage(&mut bx, &mut ev, 40, 0.0), 16.0);
    // Stage +1.5 (24 s): measured 16→12 — the same −1 A/eval as +1.0.
    assert_eq!(stage(&mut bx, &mut ev, 24, 17.5), 12.0);
    assert_eq!(stage(&mut bx, &mut ev, 40, 0.0), 16.0);
    // Stage +2.0 (15 s): −2 A per eval, still no cut (log: 16→12→10; the model's
    // 12 after two evals is within the one-sample slack of the 5 s telemetry).
    let lb = stage(&mut bx, &mut ev, 15, 18.0);
    assert!((10.0..=12.0).contains(&lb), "expected ~−2 A/eval, got {lb}");
    assert!(bx.charging());
}
