//! A post-success watcher that a new order asks to yield (`StepRunner::yield_watcher`).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kbf_step_runner::record::State;
use kbf_step_runner::{Finished, Loaded, RunError};
use support::{PENDING, Rig};
use update_hook::{Step, Verdict};

/// Catches a watcher that holds the service for good: with no ceiling and a promotion that never
/// clears, the watcher stops at its next poll once asked (here during its third `verify`), the run
/// ends `Yielded`, and the record is left in the watcher for the new order to end.
#[test]
fn a_watcher_asked_to_yield_stops_at_its_next_poll() {
    let rig = Rig::new("yield-asked");
    let mut order = rig.order();
    order.bounds.promotion_ceiling_secs = None;
    rig.fake
        .reply_with(|step| (step == Step::Verify).then_some(PENDING));
    let runner = Arc::new(rig.runner());
    let asking = Arc::clone(&runner);
    let verifies = AtomicU32::new(0);
    rig.fake.on_run(move |call| {
        let verify = call.invocation.step == Step::Verify;
        if verify && verifies.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            asking.yield_watcher();
        }
    });
    assert!(matches!(runner.run(&order), Err(RunError::Yielded)));
    let verify_runs = rig.fake.steps().into_iter().filter(|s| *s == Step::Verify);
    assert_eq!(verify_runs.count(), 3);
    let Loaded::Current(rec) = rig.store().load().unwrap() else {
        panic!("a record");
    };
    assert!(rec.progress.watching.is_some());
    assert_eq!(rec.state, State::Pending);
}

/// Catches a request to yield that outlives the watcher it was for: one withdrawn with
/// `keep_watching` stops no watcher, which runs until the promotion clears.
#[test]
fn a_withdrawn_request_to_yield_stops_no_watcher() {
    let rig = Rig::new("yield-withdrawn");
    let runner = rig.runner();
    runner.yield_watcher();
    runner.keep_watching();
    rig.fake.say(Step::Verify, &[PENDING; 3]);
    assert_eq!(
        runner.run(&rig.order()).unwrap(),
        Finished {
            verdict: Verdict::Succeeded {
                promotion_pending: false
            },
            reason: None,
        }
    );
}
