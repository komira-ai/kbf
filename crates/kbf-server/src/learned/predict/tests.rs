//! Unit tests of the predictions (`super`): what one is made of, when there is none, what
//! each mode does with it, and how a finished run falls against it in shadow.

use kbf_estimator::{Basis, GIB, MIB, Outcome as RunOutcome};
use kbf_front::Described;
use kbf_types::{FarmTime, LeaseId, OperationId};

use super::*;
use crate::action_usage::Subject;
use crate::learned::{Attempt, LearnedSizes, key_of};
use crate::machine::Platform;

const DAY: u64 = 86_400_000;
const MINUTE: u64 = 60_000_000;
const EPOCH: u64 = 20_000 * DAY;

const ALL: [Unset; 8] = [
    Unset::Missing,
    Unset::Stale,
    Unset::Cold,
    Unset::Corrupt,
    Unset::OverCap,
    Unset::Kind,
    Unset::NoOs,
    Unset::NoSubject,
];
const AXES: [Axis; 3] = [Axis::Cpu, Axis::Mem, Axis::Life];
const FITS: [Fit; 3] = [Fit::Under, Fit::Fit, Fit::Over];

fn bounds() -> Bounds {
    Bounds {
        default_memory: 4 * GIB,
        host_memory: 64 * GIB,
        node_cpu_millis: 8_000,
    }
}

fn key() -> Key {
    let (subject, _) = Subject::of(Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    });
    let platform = Platform {
        os_family: "linux",
        isa: "-".to_owned(),
    };
    key_of("main", &platform, Basis::Cgroup, &subject).expect("a key")
}

/// A quiet success of `peak` bytes that ran `wall_us` and used `cpu_us` of CPU time.
fn run(wall_us: u64, peak: Option<u64>, cpu_us: u64) -> Run {
    Run {
        outcome: RunOutcome::Success,
        wall_us,
        peak_memory_bytes: peak,
        booked_memory_bytes: 4 * GIB,
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: cpu_us,
        node_cpu_pressure_permille: Some(0),
    }
}

/// The run the keys are taught: two seconds, 500 MiB, one and a half cores.
fn taught() -> Run {
    run(2_000_000, Some(500 * MIB), 3_000_000)
}

fn learned(mode: PlacementPredictions) -> Learned {
    Learned::new(LearnedSizes::Shadow, 4 * GIB, EPOCH).with_predictions(mode)
}

/// A state that has seen `runs` of `taught()` for the key.
fn taught_with(mode: PlacementPredictions, runs: u64) -> Learned {
    let mut learned = learned(mode);
    for _ in 0..runs {
        let attempt = Attempt {
            key: key(),
            run: taught(),
        };
        learned.observe(&attempt, &bounds());
    }
    learned
}

fn warm(mode: PlacementPredictions) -> Learned {
    taught_with(mode, 6)
}

fn unset_counts(learned: &Learned) -> Vec<u64> {
    ALL.iter()
        .map(|why| learned.predictions_unset(*why))
        .collect()
}

fn vs_run(learned: &Learned) -> [[u64; 3]; 3] {
    AXES.map(|axis| FITS.map(|fit| learned.predictions_vs_run(axis, fit)))
}

/// Catches a prediction that is not the estimator's answer for the key (a swapped CPU and
/// memory, a memory the request's default replaced, a cold one, a life of another key), and
/// one that is not counted as made.
#[test]
fn a_warm_key_gives_the_estimators_figures() {
    let mut learned = warm(PlacementPredictions::On);
    let hint = Hint::default();
    let day = learned.day();
    let expected = learned
        .summaries()
        .estimate(&key(), day, &bounds(), &hint)
        .expect("a warm key");
    let got = learned
        .predict(&key(), &hint, &bounds())
        .expect("a prediction");
    assert_eq!(got, Prediction::from_estimate(&expected));
    assert_eq!(got.cpu_millis, 1_500, "the P90 of the cores the runs used");
    assert_eq!(got.memory_bytes, expected.memory_bytes);
    assert!(got.memory_bytes > 500 * MIB && got.memory_bytes < 4 * GIB);
    assert_eq!(got.life_minutes, 1, "runs of two seconds live one minute");
    assert!(!got.cold, "never the scheduler's cold prediction");
    assert_eq!(learned.predictions_made(), 1);
    assert_eq!(unset_counts(&learned), [0; 8]);
}

/// Catches a client's hint above the largest node's cores reaching the scheduler as it is:
/// the estimate keeps a hint as a floor, and the prediction clamps it to the node.
#[test]
fn the_cores_are_clamped_to_the_largest_registered_node() {
    let mut learned = warm(PlacementPredictions::On);
    let hint = Hint {
        memory_bytes: None,
        cpu_millis: Some(16_000),
    };
    let day = learned.day();
    let estimate = learned
        .summaries()
        .estimate(&key(), day, &bounds(), &hint)
        .expect("a warm key");
    assert_eq!(estimate.cpu_millis, 16_000, "the estimate keeps the hint");
    let got = learned
        .predict(&key(), &hint, &bounds())
        .expect("a prediction");
    assert_eq!(got.cpu_millis, 8_000);
    assert_eq!(got.memory_bytes, estimate.memory_bytes);
}

/// Catches an estimator fallback that gives a prediction, one counted under another reason,
/// and one counted twice: each of the estimator's answers (no row, a row seen too long ago, a
/// row with one sample, a size that would take a whole host) gives none and one count.
#[test]
fn each_fallback_of_the_estimator_gives_none_and_one_count_of_its_reason() {
    let mut stale = warm(PlacementPredictions::On);
    stale.clock(FarmTime::from_millis(61 * DAY));
    let small = Bounds {
        host_memory: 512 * MIB,
        ..bounds()
    };
    let cases = [
        (
            Unset::Missing,
            learned(PlacementPredictions::Shadow),
            bounds(),
        ),
        (
            Unset::Cold,
            taught_with(PlacementPredictions::Shadow, 1),
            bounds(),
        ),
        (Unset::Stale, stale, bounds()),
        (Unset::OverCap, warm(PlacementPredictions::On), small),
    ];
    for (why, mut learned, bounds) in cases {
        let got = learned.predict(&key(), &Hint::default(), &bounds);
        assert_eq!(got, None, "{why:?}");
        let counts = unset_counts(&learned);
        let want: Vec<u64> = ALL.iter().map(|w| u64::from(*w == why)).collect();
        assert_eq!(counts, want, "{why:?}");
        assert_eq!(learned.predictions_made(), 0, "{why:?}");
    }
}

/// Catches `off` that asks the estimator (a count of its calls), counts, or predicts: with
/// predictions off nothing happens, whatever the key has learned.
#[test]
fn off_asks_nothing_counts_nothing_and_gives_none() {
    let mut learned = warm(PlacementPredictions::Off);
    let calls = learned.calls();
    assert_eq!(learned.predict(&key(), &Hint::default(), &bounds()), None);
    learned.unpredicted(Unset::Kind);
    assert_eq!(learned.calls(), calls);
    assert_eq!(learned.predictions_made(), 0);
    assert_eq!(unset_counts(&learned), [0; 8]);
    assert_eq!(learned.to_set(OperationId(1), None), None);
}

/// Catches an operation the farm does not ask the estimator about that is not counted under
/// its own reason, and one counted while predictions are off.
#[test]
fn an_operation_not_asked_about_is_counted_by_its_reason_unless_off() {
    let mut learned = learned(PlacementPredictions::Shadow);
    for why in [Unset::Kind, Unset::NoOs, Unset::NoSubject, Unset::NoSubject] {
        learned.unpredicted(why);
    }
    let want: Vec<u64> = ALL
        .iter()
        .map(|w| match w {
            Unset::Kind | Unset::NoOs => 1,
            Unset::NoSubject => 2,
            _ => 0,
        })
        .collect();
    assert_eq!(unset_counts(&learned), want);
    learned.set_predictions(PlacementPredictions::Off);
    learned.unpredicted(Unset::Kind);
    assert_eq!(learned.predictions_unset(Unset::Kind), 1);
}

fn figures() -> Prediction {
    Prediction {
        cpu_millis: 1_500,
        memory_bytes: 600 * MIB,
        cold: false,
        life_minutes: 6,
    }
}

/// Catches `on` that keeps its prediction from the scheduler, `shadow` that gives it (the
/// scheduler must see nothing) or does not keep it to compare with the run, and `off` that
/// does either.
#[test]
fn only_on_hands_the_prediction_to_the_scheduler_and_shadow_keeps_it() {
    let given = |mode| {
        let mut learned = learned(mode);
        let set = learned.to_set(OperationId(1), Some(figures()));
        (set, learned.predictions_pending())
    };
    assert_eq!(given(PlacementPredictions::On), (Some(figures()), 0));
    assert_eq!(given(PlacementPredictions::Shadow), (None, 1));
    assert_eq!(given(PlacementPredictions::Off), (None, 0));
}

/// Catches the predictions of shadow growing without bound: past the cap the operation
/// remembered longest ago is forgotten first, and one still held is compared.
#[test]
fn shadow_remembers_a_bounded_number_of_operations_oldest_out_first() {
    let mut learned = learned(PlacementPredictions::Shadow);
    for operation in 0..=STARTED_CAP as u64 {
        learned.to_set(OperationId(operation), Some(figures()));
    }
    assert_eq!(learned.predictions_pending(), STARTED_CAP);
    let forgotten = LeaseId::new(1, 1);
    let held = LeaseId::new(1, 2);
    for (lease, operation) in [(forgotten, 0), (held, STARTED_CAP as u64)] {
        let attempt = Attempt {
            key: key(),
            run: run(90_000_000, Some(100 * MIB), 135_000_000),
        };
        learned.stage(lease, Some(attempt));
        learned.stage_prediction(lease, OperationId(operation));
        learned.finish(lease, true, &bounds());
    }
    let counted: u64 = vs_run(&learned).iter().flatten().sum();
    assert_eq!(counted, 3, "only the operation still held was compared");
}

/// Where `used` falls against `predicted` at each edge of the band.
#[test]
fn the_fit_band_is_half_the_prediction_up_to_the_prediction() {
    for (used, predicted, want) in [
        (5, 5, Fit::Fit),
        (6, 5, Fit::Under),
        (3, 6, Fit::Fit),
        (2, 6, Fit::Over),
        (0, 0, Fit::Fit),
        (1, 0, Fit::Under),
        (0, 1, Fit::Over),
        (u64::MAX, u64::MAX, Fit::Fit),
    ] {
        assert_eq!(Fit::of(used, predicted), want, "{used} against {predicted}");
    }
}

/// Stages `run` as the report of operation `operation` on lease number `seq`, as the farm
/// does, and answers it as the scheduler `taken`.
fn report(learned: &mut Learned, seq: u64, operation: u64, run: Run, taken: bool) {
    let lease = LeaseId::new(1, seq);
    learned.stage(lease, Some(Attempt { key: key(), run }));
    learned.stage_prediction(lease, OperationId(operation));
    learned.finish(lease, taken, &bounds());
}

/// Catches a run counted in a bucket of another axis or fit: a run of 90 seconds that used
/// 100 MiB and one and a half cores against 600 MiB, one and a half cores and six minutes is
/// fit in cores, over in memory and over in life, and nothing else is counted.
#[test]
fn a_run_is_counted_on_each_axis_in_its_bucket() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    report(
        &mut learned,
        1,
        1,
        run(90_000_000, Some(100 * MIB), 135_000_000),
        true,
    );
    assert_eq!(
        vs_run(&learned),
        [
            [0, 1, 0], // cpu: 1,500 against 1,500
            [0, 0, 1], // mem: 100 MiB against 600 MiB
            [0, 0, 1], // life: minute 2 against 6
        ]
    );
    assert_eq!(learned.predictions_pending(), 0);
    // The operation's later runs are not compared: its prediction was forgotten.
    report(
        &mut learned,
        2,
        1,
        run(90_000_000, Some(100 * MIB), 1),
        true,
    );
    assert_eq!(vs_run(&learned).iter().flatten().sum::<u64>(), 3);
}

/// Catches the other two buckets: more than predicted is under on every axis, and a run
/// of 100 minutes is life 30 (the estimator's horizon), which a prediction of 30 fits.
#[test]
fn a_run_above_the_prediction_is_under_and_life_stops_at_the_horizon() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    report(
        &mut learned,
        1,
        1,
        run(10 * MINUTE, Some(3 * GIB), 1_800_000_000),
        true,
    );
    assert_eq!(vs_run(&learned), [[1, 0, 0], [1, 0, 0], [1, 0, 0]]);
    let long = Prediction {
        life_minutes: 30,
        ..figures()
    };
    learned.to_set(OperationId(2), Some(long));
    report(&mut learned, 2, 2, run(100 * MINUTE, None, 0), true);
    assert_eq!(vs_run(&learned)[2], [1, 1, 0], "life 30 fits a life of 30");
}

/// Catches a figure counted for a run that did not measure it (no peak, no CPU time), and
/// every figure counted for a run the estimator would not take as a sample.
#[test]
fn a_figure_the_run_did_not_measure_is_not_counted() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    report(&mut learned, 1, 1, run(2_000_000, None, 0), true);
    assert_eq!(vs_run(&learned), [[0; 3], [0; 3], [0, 0, 1]], "life only");
    learned.to_set(OperationId(2), Some(figures()));
    report(&mut learned, 2, 2, run(2_000_000, Some(0), 3_000_000), true);
    assert_eq!(vs_run(&learned)[1], [0; 3], "a peak of zero is no peak");
    assert_eq!(vs_run(&learned)[0], [0, 1, 0]);
}

/// Catches a prediction compared with a run that says nothing about the operation's size:
/// a run under a second, a failure and a report the scheduler did not take count nothing;
/// a kill (which is requeued) keeps the operation's prediction for its next run.
#[test]
fn a_run_that_is_no_sample_is_not_compared() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    report(&mut learned, 1, 1, run(999_999, Some(MIB), 1_000_000), true);
    let failed = Run {
        outcome: RunOutcome::Failed,
        ..taught()
    };
    learned.to_set(OperationId(2), Some(figures()));
    report(&mut learned, 2, 2, failed, true);
    learned.to_set(OperationId(3), Some(figures()));
    report(&mut learned, 3, 3, taught(), false);
    assert_eq!(vs_run(&learned), [[0; 3]; 3]);
    assert_eq!(
        learned.predictions_pending(),
        1,
        "only the report the scheduler did not take leaves its operation's prediction"
    );
}

/// Catches the shadow's map filling with the predictions of operations that ended: every
/// report the scheduler takes (a requeue is another path) frees its operation's entry,
/// whether or not the run was a sample or a kill that ended the operation, so the map
/// holds what is in flight.
#[test]
fn an_operation_that_ends_is_forgotten_whatever_its_run() {
    let mut learned = learned(PlacementPredictions::Shadow);
    let ends = [
        run(999_999, Some(MIB), 1_000_000),
        Run {
            outcome: RunOutcome::Failed,
            ..taught()
        },
        Run {
            outcome: RunOutcome::OwnLimitKill,
            ..taught()
        },
        taught(),
    ];
    for (n, ended) in (1..).zip(ends) {
        learned.to_set(OperationId(n), Some(figures()));
        assert_eq!(learned.predictions_pending(), 1, "run {n} is in flight");
        report(&mut learned, n, n, ended, true);
        assert_eq!(learned.predictions_pending(), 0, "run {n} ended");
    }
}

/// Catches an operation whose run named no instrument (so nothing was staged to learn
/// from) keeping its prediction after the scheduler took the report that ended it.
#[test]
fn a_run_with_no_instrument_still_ends_the_operation() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    let lease = LeaseId::new(1, 1);
    learned.stage(lease, None);
    learned.stage_prediction(lease, OperationId(1));
    learned.finish(lease, true, &bounds());
    assert_eq!(learned.predictions_pending(), 0);
    assert_eq!(vs_run(&learned), [[0; 3]; 3], "no run, nothing compared");
}

/// Catches a killed run that drops its operation's prediction: the operation is requeued
/// and its next run is compared with the prediction made at submit.
#[test]
fn a_killed_run_leaves_the_prediction_for_the_next() {
    let mut learned = learned(PlacementPredictions::Shadow);
    learned.to_set(OperationId(1), Some(figures()));
    let lease = LeaseId::new(1, 4);
    let killed = Run {
        outcome: RunOutcome::OwnLimitKill,
        ..taught()
    };
    learned.stage(
        lease,
        Some(Attempt {
            key: key(),
            run: killed,
        }),
    );
    learned.stage_prediction(lease, OperationId(1));
    learned.requeued(lease, 8 * GIB, &bounds());
    assert_eq!(vs_run(&learned), [[0; 3]; 3]);
    assert_eq!(learned.predictions_pending(), 1);
    report(&mut learned, 5, 1, taught(), true);
    assert_eq!(vs_run(&learned).iter().flatten().sum::<u64>(), 3);
    assert_eq!(learned.predictions_pending(), 0);
}

/// Catches the scrape that is not what the state holds, series by series.
#[test]
fn the_counts_are_copied_into_the_scrape() {
    let mut learned = warm(PlacementPredictions::Shadow);
    learned.predict(&key(), &Hint::default(), &bounds());
    learned.unpredicted(Unset::NoOs);
    learned.to_set(OperationId(1), Some(figures()));
    report(
        &mut learned,
        1,
        1,
        run(90_000_000, Some(100 * MIB), 135_000_000),
        true,
    );
    let totals = PredictionTotals::default();
    learned.publish_predictions(&totals);
    assert_eq!(totals.made(), 1);
    for (reason, why) in ALL.iter().enumerate() {
        assert_eq!(
            totals.unset(reason),
            learned.predictions_unset(*why),
            "{why:?}"
        );
    }
    assert_eq!(totals.unset(6), 1, "no_os is the seventh label");
    for (a, axis) in AXES.iter().enumerate() {
        for (f, fit) in FITS.iter().enumerate() {
            let want = learned.predictions_vs_run(*axis, *fit);
            assert_eq!(totals.vs_run(a, f), want, "{axis:?} {fit:?}");
        }
    }
    assert_eq!(totals.vs_run(1, 2), 1, "memory over");
}

/// Catches a reason published under another's label, and the mapping of an estimator
/// fallback or a refused key to a reason.
#[test]
fn each_reason_has_its_label_and_each_error_its_reason() {
    let labels = [
        "missing",
        "stale",
        "cold",
        "corrupt",
        "over_cap",
        "kind",
        "no_os",
        "no_subject",
    ];
    for (why, label) in ALL.iter().zip(labels) {
        assert_eq!(why.label(), label);
    }
    for (fallback, why) in [
        (Fallback::Missing, Unset::Missing),
        (Fallback::Stale, Unset::Stale),
        (Fallback::Cold, Unset::Cold),
        (Fallback::Corrupt, Unset::Corrupt),
        (Fallback::OverCap, Unset::OverCap),
    ] {
        assert_eq!(Unset::from(fallback), why);
    }
    assert_eq!(Unset::of_key_error(Skip::NoSubject), Unset::NoSubject);
    assert_eq!(Unset::of_key_error(Skip::NoOs), Unset::NoOs);
}

/// Catches a forecast that counts, remembers or changes anything (the farm asks before it
/// knows the operation is new), and one that is not what `predict` answers for the key.
#[test]
fn a_forecast_counts_nothing_and_is_what_predict_answers() {
    let mut learned = warm(PlacementPredictions::Shadow);
    let calls = learned.calls();
    let hint = Hint::default();
    let forecast = learned.forecast(&key(), &hint, &bounds());
    assert_eq!(learned.calls(), calls);
    assert_eq!(learned.predictions_made(), 0);
    assert_eq!(learned.predictions_pending(), 0);
    assert_eq!(unset_counts(&learned), [0; 8]);
    assert_eq!(
        forecast.ok(),
        learned.predict(&key(), &hint, &bounds()),
        "the same figures"
    );
    let cold = Learned::new(LearnedSizes::Shadow, 4 * GIB, EPOCH);
    assert_eq!(
        cold.forecast(&key(), &hint, &bounds()),
        Err(Unset::Missing),
        "an unknown key says why"
    );
}

/// Catches a noted forecast not counted as made or by its reason, a mode that does with it
/// other than `predict` and `to_set` do (`on` hands it, `shadow` keeps it, `off` neither
/// counts nor keeps it).
#[test]
fn a_noted_forecast_is_counted_and_handled_as_the_mode_says() {
    let hint = Hint::default();
    for (mode, handed, pending, made) in [
        (PlacementPredictions::On, true, 0, 1),
        (PlacementPredictions::Shadow, false, 1, 1),
        (PlacementPredictions::Off, false, 0, 0),
    ] {
        let mut learned = warm(mode);
        let forecast = learned.forecast(&key(), &hint, &bounds());
        let set = learned.note_forecast(OperationId(1), forecast);
        assert_eq!(set.is_some(), handed, "{mode:?}");
        assert_eq!(learned.predictions_pending(), pending, "{mode:?}");
        assert_eq!(learned.predictions_made(), made, "{mode:?}");
    }
    let mut learned = learned(PlacementPredictions::On);
    assert_eq!(
        learned.note_forecast(OperationId(1), Err(Unset::Kind)),
        None
    );
    assert_eq!(learned.predictions_unset(Unset::Kind), 1);
}
