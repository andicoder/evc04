//! Replay harness (evc04#135 steps 3+4): the *real* `core` controller against the
//! `boxsim` box model, an EV model and the measured-value pipeline as deployed
//! today — proving offline that the controller reproduces the measured sessions in
//! `tests/fixtures/sessions/`, most importantly the #119 symptom ("target 6,
//! charged ~15 A") and the 2026-07-05 campaign wire captures. Scenario
//! timings/values are hand-copied from those fixtures (cited per test); the
//! CSVs/logs remain the provenance record.
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
/// The firmware's `lb_current` feedback follows the box's `Cmax` within 0.2–2.4 s
/// (2026-07-05-sweep2-rep0-wire.log: Cmax change → `charge/status` lb) — much
/// fresher than the ~5 s LOG metering cadence assumed for #119.
const CN28_PERIOD_S: u32 = 2;
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
    /// Chronic overdraw: the car pulls ~1–2 % above its grant when it draws at
    /// all (campaign 2026-07-05: 8.15 @ 8, 6.14 @ 6). 1.0 = exact tracking.
    overshoot: f32,
    offered_s: f32,
}

impl Ev {
    fn new(start_lag_s: f32) -> Self {
        Ev {
            amps: 0.0,
            start_lag_s,
            overshoot: 1.0,
            offered_s: 0.0,
        }
    }
    fn with_overshoot(start_lag_s: f32, overshoot: f32) -> Self {
        Ev {
            overshoot,
            ..Ev::new(start_lag_s)
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
            (lb * self.overshoot).min(16.0)
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
    /// Variant V4 (#135 step 5): regulate the box's grant directly on the ~2 s
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
    ev: Ev,
    /// MID standby noise on the box's own car-circuit meter (campaign 2026-07-05:
    /// 18–45 mA per phase at idle) — rides on the CN28 car feedback the V4
    /// controller consumes, not on the true draw.
    mid_noise_a: f32,
}

#[derive(Debug)]
struct Sample {
    t: u32,
    target: Option<f32>,
    reported: f32,
    lb: f32,
    car: f32,
}

/// Run the full loop at 1 s ticks and record one sample per second. Session
/// starts are the box's own decision now (start law on the slow clock) — the
/// harness never nudges it.
fn run(sc: Scenario) -> Vec<Sample> {
    let mut boxsim = BoxSim::new(sc.box_params);
    let mut ev = sc.ev;
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

        // CN28 telemetry: the V4 feedback follows the box's grant within ~2 s.
        if t % CN28_PERIOD_S == 0 {
            cn28_lb = boxsim.lb().0;
            cn28_car = ev.amps + sc.mid_noise_a;
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

/// The steady-state box (campaign 2026-07-05): starts and up moves on the ~30 s
/// clock, down moves and cut checks on the ~5 s clock, session opens only above
/// MAX/2 headroom.
fn fitted_box() -> BoxSimParams {
    BoxSimParams {
        max_dip: Ampere(MAX_BOX),
        up_period_s: 30.0,
        down_period_s: 5.0,
        cut_margin: Ampere(PAUSE_MARGIN),
        start_threshold: Ampere(MAX_BOX / 2.0),
    }
}

/// Fixture `2026-06-30-target6-pinned15.csv`: evcc offers 6 (18:21:17), 16
/// (18:21:48), 15, 14, then 13 (18:23:17); house ≈ 350 W, no PV. Observed: the box
/// ratchets to 16, then sits at ~15 A for the rest of the session although evcc
/// wanted 13 — the #119 symptom this harness must reproduce deterministically.
fn scenario_2026_06_30() -> Scenario {
    Scenario {
        events: vec![
            // The fixture window opens mid-session (the box was already charging
            // when evcc offered 6 at 18:21:17) — warm the session up first, or the
            // steady-state start law would delay the start into the window and
            // change the whole grant path.
            (0, Some(16.0), true),
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
        ev: Ev::new(0.0),
        mid_noise_a: 0.0,
    }
}

#[test]
fn replay_2026_06_30_reproduces_pinned_high_charge() {
    let trace = run(scenario_2026_06_30());
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
        // 2026-07-02 was the OTA-heavy dev day: every session followed meter
        // silence, and the box opened at ~6.5 A headroom — the post-meter-silence
        // start regime, not the steady-state MAX/2 threshold of the 2026-07-05
        // sweeps.
        box_params: BoxSimParams {
            start_threshold: Ampere(0.0),
            ..fitted_box()
        },
        ev: Ev::new(0.0),
        mid_noise_a: 0.0,
    }
}

#[test]
fn replay_golden_window_reproduces_ratchet_to_full() {
    let trace = run(scenario_golden_window());
    let start = trace.iter().find(|s| s.lb > 0.0).expect("charge starts");
    assert!(
        (6.0..=8.0).contains(&start.lb),
        "start grant should be the ceil of the headroom (~7), got {start:?}"
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
    let end = &run(sc)[850];
    assert!(
        (end.car - 13.0).abs() <= 1.5,
        "with the measured down response the loop should settle near the 13 A target, got {end:?}"
    );
}

/// The #135 step-5 acceptance run for variant V4: the planned live-test staircase
/// (16→12→10→8→6→8→16) under deep PV export — the exact scenario that broke the
/// shipped controller (H2: measured clamped to 0, loop open). V4 never looks at
/// `measured`, so the export can't hurt it; the box's measured down response does
/// the actual shedding.
///
/// The two decision clocks are only known as ranges (up ~28–36 s, down ~4–6 s)
/// and the ~2 s CN28 feedback lag beats against them, so the proof must hold
/// across the whole band — including the phase alignments where a stale grant
/// makes V4 over-report one eval too long.
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
    for up_period_s in [24.0, 30.0, 36.0] {
        for down_period_s in [4.0, 5.0, 6.0] {
            // Lag 0 = the fixture-fitted instant car; 15 s = the measured
            // contactor lag of the real car (flag-day capture 2026-07-03).
            for ev_start_lag_s in [0.0, 15.0] {
                let sc = Scenario {
                    events: steps.iter().map(|&(t, a)| (t, Some(a), true)).collect(),
                    house_w: 300.0,
                    pv_w: 12_000.0, // deep export: the H2 scenario the old loop couldn't survive
                    duration_s: 840,
                    reporting: Reporting::LbTracking,
                    box_params: BoxSimParams {
                        up_period_s,
                        down_period_s,
                        ..fitted_box()
                    },
                    ev: Ev::new(ev_start_lag_s),
                    mid_noise_a: 0.0,
                };
                let trace = run(sc);

                // The session must survive the whole staircase: once the car
                // draws, the box never cuts (lb never returns to 0) and the car
                // never stalls below 6 A.
                let start = trace.iter().find(|s| s.car >= 1.0).expect("charge starts");
                for s in trace.iter().skip(start.t as usize + 30) {
                    assert!(
                        s.lb > 0.0,
                        "box cut the session (up {up_period_s}, down {down_period_s}, lag {ev_start_lag_s}) at {s:?}"
                    );
                    assert!(
                        s.car >= MIN_CHARGE,
                        "car stalled (up {up_period_s}, down {down_period_s}, lag {ev_start_lag_s}) at {s:?}"
                    );
                }

                // Each step has settled onto its target (±1 A, grants are whole
                // amps) well before the next one — no pin at 15/16, no undershoot.
                for &(at, tgt) in &steps {
                    let settled = &trace[(at + 110) as usize];
                    assert!(
                        (settled.car - tgt).abs() <= 1.0,
                        "target {tgt} not held 110 s after the step (up {up_period_s}, down {down_period_s}, lag {ev_start_lag_s}), got {settled:?}"
                    );
                    assert!(
                        (settled.lb - tgt).abs() <= 1.0,
                        "grant off target {tgt} 110 s after the step (up {up_period_s}, down {down_period_s}, lag {ev_start_lag_s}), got {settled:?}"
                    );
                }
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
    for up_period_s in [24.0, 30.0, 36.0] {
        let sc = Scenario {
            events: vec![(0, Some(16.0), true)],
            house_w: 300.0,
            pv_w: 12_000.0,
            duration_s: 300,
            reporting: Reporting::LbTracking,
            box_params: BoxSimParams {
                up_period_s,
                ..fitted_box()
            },
            ev: Ev::new(15.0),
            mid_noise_a: 0.0,
        };
        let trace = run(sc);
        let late = &trace[240];
        assert!(
            late.car >= MIN_CHARGE,
            "car never started (up {up_period_s}): {late:?}"
        );
        // Once the car draws, the session must hold — no further grant/cut cycle.
        let first_draw = trace.iter().find(|s| s.car >= 1.0).unwrap().t as usize;
        for s in trace.iter().skip(first_draw) {
            assert!(
                s.lb > 0.0,
                "cut after the car started (up {up_period_s}): {s:?}"
            );
        }
    }
}

/// Replay of the #135 step-6 probe measurement (`2026-07-02-probe-measurement.log`):
/// the staged over-limit reports are fed verbatim; the fitted model must reproduce
/// the measured lb rides within one eval step — including the +1.0 stage's ending
/// (fixture header: "session drop + self-recovery"), which the pre-campaign model
/// wrongly survived by holding at 6.
#[test]
fn replay_probe_measurement_reproduces_the_down_rides_and_the_drop() {
    fn stage(boxsim: &mut BoxSim, ev: &mut Ev, secs: u32, reported: f32) -> f32 {
        for _ in 0..secs {
            boxsim.tick(1.0, Ampere(reported), Ampere(ev.amps));
            ev.tick(1.0, boxsim.lb().0);
        }
        boxsim.lb().0
    }

    // The probe session followed an OTA reboot (meter silence) like everything on
    // 2026-07-02 — the low-threshold start regime.
    let mut bx = BoxSim::new(BoxSimParams {
        start_threshold: Ampere(0.0),
        ..fitted_box()
    });
    let mut ev = Ev::new(0.0);

    // Warm-up at reported 0: the box opens on its slow clock, full grant, the car
    // ramps to 16 A (21:33:54 baseline).
    assert_eq!(stage(&mut bx, &mut ev, 100, 0.0), 16.0);
    assert!((ev.amps - 16.0).abs() < 0.01);
    // Stage +0.5 (60 s): lb held at 16 — round(16 − 0.5) = 16, the dead zone is
    // the rounding.
    assert_eq!(stage(&mut bx, &mut ev, 60, 16.5), 16.0);
    assert_eq!(stage(&mut bx, &mut ev, 50, 0.0), 16.0);
    // Stage +1.0 (60 s): measured ride 16→6, −1 A per eval with the car tracking
    // the grant down — then the session DROPS: the next eval at 6 computes 5,
    // below the pilot floor (fixture 21:37:41, cp C→B with the car still at ~6).
    stage(&mut bx, &mut ev, 60, 17.0);
    assert!(
        !bx.charging(),
        "the +1.0 ride must end in the measured drop"
    );
    assert_eq!(bx.lb(), Ampere(0.0));
    // Self-recovery (log: lb back at 16 within ~40 s of the probe ending): the
    // start clock reopens at full headroom.
    assert_eq!(stage(&mut bx, &mut ev, 40, 0.0), 16.0);
    assert!(bx.charging());
    let _ = stage(&mut bx, &mut ev, 20, 0.0); // let the car top out again
                                              // Stage +1.5 (24 s): measured 16→12 — the same −1 A/eval as +1.0 (round(16 −
                                              // 1.5) = 15, half-up).
    assert_eq!(stage(&mut bx, &mut ev, 24, 17.5), 12.0);
    assert_eq!(stage(&mut bx, &mut ev, 40, 0.0), 16.0);
    // Stage +2.0 (15 s): −2 A per eval, still no cut — the cut threshold sits
    // above +2 (#57: pause at +4 cuts).
    let lb = stage(&mut bx, &mut ev, 15, 18.0);
    assert!((10.0..=12.0).contains(&lb), "expected ~−2 A/eval, got {lb}");
    assert!(bx.charging());
}

/// Replay of sweep2 rep 0 (`2026-07-05-sweep2-rep0-wire.log`): engage at target
/// 11, settle, flip the target to 6. Measured: the box walked the grant down with
/// the falling car and **held at exactly 6, session alive** — the V4 down-shed
/// works. The `[10, 6, 0]` "cut" in sweep2.jsonl was an external enable flap 34 s
/// later (69 s outage): the firmware's pause report, not a shed failure. The same
/// flap pattern explains every "spontaneous" cut and the "car stops ramping" of
/// reps 1–5 (minute-aligned enable=false all afternoon, 300 s outages during the
/// later reps) — so there is no cut-history mechanism to model.
#[test]
fn replay_sweep2_rep0_flip_converges_and_only_the_enable_flap_cuts() {
    let sc = Scenario {
        events: vec![
            (0, Some(11.0), true),
            (150, Some(6.0), true),  // wire t=357.3: evcc-side flip 11 → 6
            (190, Some(6.0), false), // wire t=391.3: the external enable flap
            (259, Some(6.0), true),  // flap restored after 69 s
        ],
        house_w: 300.0,
        pv_w: 12_000.0,
        duration_s: 340,
        reporting: Reporting::LbTracking,
        box_params: fitted_box(),
        // Rep 0 engaged in 65.5 s wall clock incl. the car's contactor lag; the
        // car then drew chronically ~1–2 % over grant (10.18 @ 11 aside, which the
        // Tesla capped itself).
        ev: Ev::with_overshoot(15.0, 1.015),
        mid_noise_a: 0.045,
    };
    let trace = run(sc);

    // Engage on the slow clock. The wire recorded 11 here (pre-kick firmware: deficit
    // report 5 → ceil headroom 11); the kick serves the full offer instead, so the box
    // opens at its ceiling and the pin walks it back down to the target.
    let start = trace.iter().find(|s| s.lb > 0.0).expect("engages");
    assert!(start.t <= 35, "engage within one up period, got {start:?}");
    assert_eq!(start.lb, MAX_BOX);

    // After the flip the grant converges next to the target and HOLDS — no cut.
    // The wire shows 6 exactly (the box out-shed the firmware's floor-capped
    // over-report on stale feedback); with fresh feedback V4's deliberate
    // never-shed-into-the-pilot-floor cap settles one amp higher — both inside
    // the ±1 A acceptance. Allow the shed transient one down eval + car ramp.
    for s in trace.iter().take(190).skip(165) {
        assert!(
            s.lb == 6.0 || s.lb == 7.0,
            "expected the shed to hold at 6..7, got {s:?}"
        );
        assert!(s.car >= 6.0, "session must stay alive, got {s:?}");
    }

    // The enable flap pause-cuts within one fast eval …
    let cut = trace.iter().find(|s| s.t >= 190 && s.lb == 0.0).unwrap();
    assert!(cut.t <= 196, "pause must cut within ~5 s, got {cut:?}");
    // … and stays cut for as long as the flap holds enable low.
    for s in trace.iter().take(259).skip(200) {
        assert_eq!(s.lb, 0.0, "a pause must not re-engage, got {s:?}");
    }
    // Once enable returns, target 6 is a cold start again: the campaign's pre-kick
    // firmware never re-engaged here (deficit report 10 → opening 6 ≤ MAX/2), which is
    // the stall this loop must not reproduce. The kick reopens within one up period.
    let reengage = trace
        .iter()
        .find(|s| s.t > 259 && s.lb > 0.0)
        .expect("the kick must reopen after the flap clears");
    assert!(
        reengage.t <= 295,
        "reopen within one up period, got {reengage:?}"
    );
}

/// The steady-state start law makes low targets unreachable from a standing stop:
/// sweep1/1c censored every attempt at ≤ 8.0 A headroom over 5–6 minutes (11 data
/// points), so the deficit report `max − target` cannot *open* a pv-surplus session
/// at 6–8 A. The cold-start kick is the controller-side answer: with no grant it
/// serves the full offer, the box opens at its ceiling, and the pin sheds to target.
#[test]
fn lb_tracking_kick_opens_a_session_below_half_max_headroom() {
    let sc = Scenario {
        events: vec![(0, Some(6.0), true)],
        house_w: 300.0,
        pv_w: 12_000.0,
        duration_s: 360,
        reporting: Reporting::LbTracking,
        box_params: fitted_box(),
        ev: Ev::new(15.0),
        mid_noise_a: 0.045,
    };
    let trace = run(sc);

    let start = trace.iter().find(|s| s.lb > 0.0).expect("the kick engages");
    assert!(start.t <= 35, "engage within one up period, got {start:?}");
    assert_eq!(
        start.lb, MAX_BOX,
        "the full offer opens the box at its ceiling"
    );

    // The car is still in its contactor lag when the ceiling grant lands, so it never
    // draws it; the pin sheds the grant to the 6 A target on the fast clock.
    for s in trace.iter().skip(120) {
        assert!(
            s.lb == 6.0 || s.lb == 7.0,
            "the session must settle at the target, got {s:?}"
        );
    }
}

/// Noise robustness (campaign findings 5): the car draws ~1–2 % above the grant
/// and the box's MID meter shows 18–45 mA standby noise on the CN28 feedback. The
/// V4 loop must neither oscillate nor lose the session over either — in
/// particular the whole-amp pin during the contactor lag must not be tipped by
/// the noise (that regression killed session starts live on 2026-07-05).
#[test]
fn lb_tracking_survives_car_overshoot_and_mid_standby_noise() {
    let steps: [(u32, f32); 3] = [(0, 16.0), (120, 8.0), (240, 6.0)];
    let sc = Scenario {
        events: steps.iter().map(|&(t, a)| (t, Some(a), true)).collect(),
        house_w: 300.0,
        pv_w: 12_000.0,
        duration_s: 360,
        reporting: Reporting::LbTracking,
        box_params: fitted_box(),
        ev: Ev::with_overshoot(15.0, 1.02),
        mid_noise_a: 0.045,
    };
    let trace = run(sc);
    let start = trace.iter().find(|s| s.car >= 1.0).expect("charge starts");
    for s in trace.iter().skip(start.t as usize + 30) {
        assert!(s.lb > 0.0, "box cut the session at {s:?}");
        assert!(s.car >= MIN_CHARGE, "car stalled at {s:?}");
    }
    for &(at, tgt) in &steps {
        // V4's floor cap settles target 6 at grant 7 by design; with the 2 %
        // overdraw on top the car sits at ~7.14 — the acceptance is ±1 A on the
        // grant plus the overdraw on the car.
        let settled = &trace[(at + 110) as usize];
        assert!(
            (settled.car - tgt).abs() <= 1.2 && (settled.lb - tgt).abs() <= 1.0,
            "target {tgt} not held under noise, got {settled:?}"
        );
    }
}
