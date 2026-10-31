//! Kill the updater at every boundary of a full update and resume: the end is the same.
//!
//! The boundaries are every point of every write of the step record (before it touched the disk,
//! after it was durable) and every run of a step the updater dies in the middle of, with the
//! step's unit left alive. Each scenario is played once uninterrupted, then once per boundary.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

fn script(rig: &Rig, kind: Kind) {
    match kind {
        Kind::Success => {}
        Kind::VerifyFails => rig.fake.say(Step::Verify, &[FAIL; 4]),
        Kind::CommitThenVerifyFails => {
            rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK; 3]);
            rig.fake.say(Step::Verify, &[FAIL; 4]);
        }
        Kind::DeferredVerify => rig.fake.say(Step::Verify, &[DEFER, OK]),
        Kind::PendingPromotion => rig.fake.say(Step::Verify, &[PENDING, OK]),
    }
}

/// The verdict, the number of record writes and the number of step runs of an uninterrupted update.
fn baseline(kind: Kind) -> (Finished, u64, u64) {
    let rig = Rig::new(&format!("crash-base-{kind:?}"));
    script(&rig, kind);
    let most = Arc::new(AtomicU64::new(0));
    let seen = most.clone();
    let gate = Arc::new(move |p| {
        if let CrashPoint::BeforeWrite(n) = p {
            seen.fetch_max(n, Ordering::SeqCst);
        }
        false
    });
    let got = rig.runner_gated(gate).run(&rig.order()).unwrap();
    (
        got,
        most.load(Ordering::SeqCst),
        rig.fake.runs().len() as u64,
    )
}

fn crash_once(kind: Kind, id: &str, at: Option<CrashPoint>, lose_run: u64, want: &Finished) {
    let rig = Rig::new(&format!("crash-{id}"));
    script(&rig, kind);
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
    assert_eq!(&got, want, "{kind:?} killed at {at:?} / run {lose_run}");
    if lose_run != 0 {
        // The step the updater died in had its unit stopped by name before anything went on.
        assert_eq!(rig.fake.stops().len(), 1, "{kind:?} lost run {lose_run}");
    }
}

fn sweep(kind: Kind) {
    let (want, writes, runs) = baseline(kind);
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
