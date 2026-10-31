//! Kill the updater at every boundary of a full update and resume: the end is the same.
//!
//! The boundaries are every point of every write of the step record (before it touched the disk,
//! after it was durable) and every run of a step the updater dies in the middle of, with the
//! step's unit left alive. Each scenario is played once uninterrupted, then once per boundary.

mod support;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kbf_step_runner::{CrashPoint, Finished};
use support::{DEFER, FAIL, IRREVERSIBLE_OK, OK, PENDING, Rig};
use update_hook::Step;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Success,
    VerifyFails,
    CommitThenVerifyFails,
    DeferredVerify,
    PendingPromotion,
}

type Seen = Arc<Mutex<Vec<(Step, u64, bool)>>>;

/// Plays the scenario and watches every run start: the record must say the step is `running`, and
/// no (step, attempt, watcher) is run twice, so a step the record holds as done never runs again.
fn arm(rig: &Rig, kind: Kind) -> Seen {
    let seen: Seen = Arc::default();
    let cleaned = Arc::new(AtomicBool::new(false));
    match kind {
        Kind::Success => {}
        Kind::VerifyFails => rig.fake.say(Step::Verify, &[FAIL; 4]),
        Kind::CommitThenVerifyFails => {
            rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK; 3]);
            rig.fake.say(Step::Verify, &[FAIL; 4]);
        }
        Kind::DeferredVerify => rig.fake.say(Step::Verify, &[DEFER, OK]),
        Kind::PendingPromotion => {
            // The world decides, so a step run again answers as it did: pending until cleaned up.
            let done = cleaned.clone();
            rig.fake.reply_with(move |s| {
                (s == Step::Verify && !done.load(Ordering::SeqCst)).then_some(PENDING)
            });
        }
    }
    let (record, log) = (rig.state.join("step-record.json"), seen.clone());
    rig.fake.on_run(move |call| {
        if call.invocation.step == Step::Cleanup {
            cleaned.store(true, Ordering::SeqCst);
        }
        let rec: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
        assert_eq!(rec["state"]["state"], "running");
        let attempt = rec["attempt"].as_u64().unwrap();
        let watching = !rec["progress"]["watching"].is_null();
        log.lock()
            .unwrap()
            .push((call.invocation.step, attempt, watching));
    });
    seen
}

/// The steps run, a step run again straight after itself counted once.
fn sequence(rig: &Rig) -> Vec<Step> {
    let mut steps = rig.fake.steps();
    steps.dedup();
    steps
}

struct Baseline {
    done: Finished,
    writes: u64,
    runs: u64,
    steps: Vec<Step>,
}

fn baseline(kind: Kind) -> Baseline {
    let rig = Rig::new(&format!("crash-base-{kind:?}"));
    arm(&rig, kind);
    let most = Arc::new(AtomicU64::new(0));
    let seen = most.clone();
    let gate = Arc::new(move |p| {
        if let CrashPoint::BeforeWrite(n) = p {
            seen.fetch_max(n, Ordering::SeqCst);
        }
        false
    });
    let done = rig.runner_gated(gate).run(&rig.order()).unwrap();
    Baseline {
        done,
        writes: most.load(Ordering::SeqCst),
        runs: rig.fake.runs().len() as u64,
        steps: sequence(&rig),
    }
}

fn crash_once(kind: Kind, id: &str, at: Option<CrashPoint>, lose_run: u64, want: &Baseline) {
    let rig = Rig::new(&format!("crash-{id}"));
    let seen = arm(&rig, kind);
    rig.fake.lose_run.store(lose_run, Ordering::SeqCst);
    let order = rig.order();
    let first = rig
        .runner_gated(Arc::new(move |p| Some(p) == at))
        .run(&order);
    let got = match first {
        Ok(done) => done,
        // The updater died. A restart asks what the last update ended as, and runs the order
        // again only when it did not end.
        Err(_) => match rig.runner().last_result().unwrap() {
            Some(done) => done,
            None => rig.runner().run(&order).unwrap(),
        },
    };
    let what = format!("{kind:?} killed at {at:?} / run {lose_run}");
    assert_eq!(got, want.done, "{what}");
    assert_eq!(sequence(&rig), want.steps, "{what}");
    let mut keys = seen.lock().unwrap().clone();
    let all = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), all, "a step recorded done ran again: {what}");
    if lose_run != 0 {
        // The step the updater died in had its unit stopped by name before anything went on.
        assert_eq!(rig.fake.stops().len(), 1, "{what}");
    }
}

fn sweep(kind: Kind) {
    let want = baseline(kind);
    let (writes, runs) = (want.writes, want.runs);
    assert!(
        writes >= 8 && runs >= 4,
        "{kind:?}: {writes} writes, {runs} runs"
    );
    for n in 1..=writes {
        for at in [CrashPoint::BeforeWrite(n), CrashPoint::AfterWrite(n)] {
            crash_once(kind, &format!("{kind:?}-{n}-{at:?}"), Some(at), 0, &want);
        }
    }
    for run in 1..=runs {
        crash_once(kind, &format!("{kind:?}-run{run}"), None, run, &want);
    }
}

/// Catches a resume that goes somewhere an uninterrupted update does not: a clean update killed
/// at any boundary ends as it would have.
#[test]
fn a_successful_update_resumes_from_every_boundary() {
    sweep(Kind::Success);
}

/// Catches a resume that loses a rollback: killed at any boundary of an update whose `verify`
/// fails, the update still rolls back and ends `Failed`.
#[test]
fn a_rolled_back_update_resumes_from_every_boundary() {
    sweep(Kind::VerifyFails);
}

/// Catches a resume that forgets the commit: killed at any boundary after `cutover` said
/// `reversible: false`, the verdict is still `after_commit`.
#[test]
fn a_committed_update_resumes_from_every_boundary() {
    sweep(Kind::CommitThenVerifyFails);
}

/// Catches a resume that loses a deferral or a watcher: a `verify` that defers once, and one that
/// reports a pending promotion, end as they would have after a kill at any boundary.
#[test]
fn deferred_and_watched_updates_resume_from_every_boundary() {
    sweep(Kind::DeferredVerify);
    sweep(Kind::PendingPromotion);
}
