//! A declaration that gives no `promotion_ceiling_secs`: the post-success watcher has no ceiling,
//! and the params file names none.

mod support;

use kbf_step_runner::Finished;
use kbf_step_runner::runner::WATCH_POLL_SECS;
use support::{PENDING, Rig};
use update_hook::{Params, Step, Verdict};

/// Catches an omitted ceiling read as some ceiling: with `promotion_ceiling_secs` absent, a
/// promotion pending through ten polls (longer than any ceiling the tests give) is watched until
/// `verify` says it is done, the update succeeds, and no step is told a ceiling.
#[test]
fn an_order_without_a_ceiling_watches_until_the_promotion_and_names_no_ceiling() {
    let rig = Rig::new("ceiling-none");
    let mut order = rig.order();
    order.bounds.promotion_ceiling_secs = None;
    rig.fake.say(Step::Verify, &[PENDING; 10]);
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(
        got,
        Finished {
            verdict: Verdict::Succeeded {
                promotion_pending: false
            },
            reason: None,
        }
    );
    let verifies = rig
        .fake
        .steps()
        .into_iter()
        .filter(|step| *step == Step::Verify);
    assert_eq!(verifies.count(), 11, "ten pending, then done");
    assert!(
        rig.clock
            .slept()
            .iter()
            .all(|secs| *secs == WATCH_POLL_SECS)
    );
    for event in rig.fake.runs() {
        if let support::Event::Run { params, .. } = event {
            assert_eq!(
                Params::parse(&params).unwrap().promotion_ceiling_secs(),
                None
            );
        }
    }
}
