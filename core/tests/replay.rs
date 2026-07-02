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
    ramp_step, reported_current, trim_decay, trim_step, Ampere, ControlInputs, FailsafeMode,
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

/// The car: follows the box's grant with a finite ramp; below the IEC 6 A pilot
/// minimum it draws nothing. Rates fitted from the fixtures (~0.5 A/s up, fast
/// ramp-down on a cut).
struct Ev {
    amps: f32,
}

impl Ev {
    fn new() -> Self {
        Ev { amps: 0.0 }
    }
    fn tick(&mut self, dt: f32, lb: f32) {
        let target = if lb >= MIN_CHARGE { lb.min(16.0) } else { 0.0 };
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
    let mut ev = Ev::new();
    let mut target: Option<f32> = None;
    let mut enabled = true;
    let mut offset = MAX_BOX; // firmware cold-start
    let mut trim = 0.0_f32;
    let mut measured = 0.0_f32;
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
                (Reporting::AsShipped, Some(tgt)) => trim_step(
                    Ampere(trim),
                    Ampere(ev.amps),
                    Ampere(tgt),
                    TRIM_KI,
                    Ampere(TRIM_MAX),
                )
                .0,
                (Reporting::AsShipped, None) => trim_decay(Ampere(trim), Ampere(1.0)).0,
                (Reporting::OpenClamp { .. }, _) => 0.0,
            };
        }

        let reported = match sc.reporting {
            Reporting::AsShipped => reported_current(&ControlInputs {
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
            .0,
            Reporting::OpenClamp { cap } => {
                if !enabled || target.is_none() || target.is_some_and(|t| t < MIN_CHARGE) {
                    MAX_BOX + PAUSE_MARGIN
                } else {
                    (offset + measured).clamp(0.0, cap)
                }
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
        eval_period_s: 10.0,
        cut_margin: Ampere(PAUSE_MARGIN),
        down_step: Ampere(0.0), // never observed below the cut (H1 masked it)
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
    }
}

#[test]
fn replay_2026_06_30_reproduces_pinned_high_charge() {
    let trace = run(&scenario_2026_06_30());
    let end = &trace[850];
    assert_eq!(end.target, Some(13.0));
    // Bug reproduced: the car sits well above the target (fixture: ~15.2 A) …
    assert!(
        end.car >= 14.0,
        "expected pinned-high charge, got {end:?}"
    );
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
/// "slightly over the limit" (below its cut threshold). Whether that helps depends
/// entirely on an unmeasured box property — a gentle down-step in that region.
#[test]
fn open_clamp_with_box_down_step_converges_to_target() {
    let mut sc = scenario_2026_06_30();
    sc.reporting = Reporting::OpenClamp { cap: 19.0 };
    sc.box_params.down_step = Ampere(1.0);
    let end = &run(&sc)[850];
    assert!(
        (end.car - 13.0).abs() <= 1.5,
        "with a real down-step the loop should settle near the 13 A target, got {end:?}"
    );
}

#[test]
fn open_clamp_without_box_down_step_stays_pinned() {
    let mut sc = scenario_2026_06_30();
    sc.reporting = Reporting::OpenClamp { cap: 19.0 };
    let end = &run(&sc)[850];
    assert!(
        end.car >= 14.0,
        "with no box down response even the open clamp cannot lower the charge, got {end:?}"
    );
}
