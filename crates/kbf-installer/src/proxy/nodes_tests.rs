//! Step 9 of the swap, the order for the nodes: the source it names, when it is sent again,
//! and what a refusal of it does to the update. Against the fakes of [`super::fakes`].

use super::fakes::*;
use super::model::*;
use super::swap::Swap;
use crate::testkit::B;

fn go(w: &World, plan: Plan, old: ProcIdent) -> Result<Phase, Halt> {
    Swap::begin(w.ports(), plan, Some(old))?.run()
}

/// The plan of an update whose release came from the host's package.
fn staged_plan() -> Plan {
    Plan {
        source: Source::Staged,
        ..plan(Mode::Frontend)
    }
}

/// A world where the old child drains late, so that the end of the update is well after the
/// order for the nodes.
fn draining_world() -> (World, ProcIdent) {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 3, Some(secs(400)));
    w.busy(Slot::A, 2, Some(secs(500)));
    w.sessions(Slot::A, 1, Some(secs(600)));
    (w, old)
}

/// The log's lines for the order for the nodes.
fn desired(w: &World) -> Vec<String> {
    w.log()
        .into_iter()
        .filter(|line| line.starts_with("admin:desired"))
        .collect()
}

/// Catches a source that stops at the plan: the order for the nodes names the source of the
/// update when it is not a fetch, and is the event it has always been when it is. The intent
/// (`update.json`) keeps the source too, because the order is sent again after a restart.
///
/// Catches: the order sent with source `fetch` whatever the plan says; the intent
/// built with source `fetch`; the plan of a restart built with source `fetch`.
#[test]
fn the_order_for_the_nodes_names_the_source_of_the_update_and_the_intent_keeps_it() {
    let (w, old) = World::with_old();
    assert_eq!(go(&w, staged_plan(), old), Ok(Phase::Done));
    let want = format!("admin:desired B {} staged", &B[..4]);
    assert_eq!(desired(&w), [want]);
    let intent = w.last_intent();
    assert_eq!(intent.source, Source::Staged);
    let resumed = Plan::resumed(&intent, Timing::frontend());
    assert_eq!(resumed.source, Source::Staged);

    let (w, old) = World::with_old();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    let want = format!("admin:desired B {}", &B[..4]);
    assert_eq!(desired(&w), [want]);
    assert_eq!(w.last_intent().source, Source::Fetch);
}

/// Catches an order for the nodes that a child refused for good being taken for a success:
/// the update would end `done` with the nodes never told, and the operator would be told the
/// release was rolled out. The refusal is recorded with the child's code, is not sent again,
/// and ends the update `failed` at `nodes`. The frontend itself was swapped, and stays so: the
/// staged release is kept, the new child serves, and the old one is drained and retired as in
/// any update.
///
/// Catches: the refusal noted and the update `done`; the refusal sent again; the
/// failure recorded as step `confirm`.
#[test]
fn a_refusal_of_the_order_for_the_nodes_ends_the_update_failed_at_nodes() {
    let (w, old) = draining_world();
    w.st.borrow_mut().desired_refused = Some("not_own".to_owned());
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let f = w.last_intent().failure.expect("the failure is surfaced");
    assert_eq!(f.step, Step::Nodes);
    assert!(f.reason.starts_with("not_own: "), "{}", f.reason);
    let tries = w.count("admin:desired");
    assert_eq!(tries, 1, "a refusal for good is not repeated");
    assert!(!w.last_intent().nodes_posted);
    assert_eq!(w.count("order revert"), 0, "the staged release is kept");
    assert_eq!(w.count("admin:resume"), 0);
    assert_eq!(w.count("kill"), 0);
    assert!(w.when("terminate pid 100").expect("old retired") >= secs(600));
    assert!(!w.alive(100));
    assert!(w.alive(101));
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.phases().last(), Some(&Phase::Failed));
    assert!(w.phases().contains(&Phase::Draining));
}

/// Catches an order for the nodes that gets no answer being given up on at once, as it was
/// before: it is sent again, one poll apart, until the child answers, and the update ends as
/// the plan said it would.
///
/// Catches: one try only (the update ends `done`, the nodes never told).
#[test]
fn an_order_for_the_nodes_that_gets_no_answer_is_sent_again_until_it_does() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().desired_fails = 2;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(w.count("admin:desired"), 3);
    assert!(w.last_intent().nodes_posted);
    assert!(w.last_intent().failure.is_none());
    let times: Vec<u64> =
        w.st.borrow()
            .events
            .iter()
            .filter(|e| e.1.starts_with("admin:desired"))
            .map(|e| (e.0 - START).as_secs())
            .collect();
    let gaps: Vec<u64> = times.windows(2).map(|p| p[1] - p[0]).collect();
    assert_eq!(gaps, vec![1, 1], "one poll apart");
}

/// Catches a child that never answers holding the update for ever: the order is sent again
/// for `desired_within` and then the update ends `failed` at `nodes` with the reason
/// `timeout`, the frontend as it is (the new child serves).
///
/// Catches: no bound; the bound counted from the start of the update.
#[test]
fn a_child_that_never_answers_ends_the_update_failed_at_nodes_after_the_bound() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().desired_err = true;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let f = w.last_intent().failure.expect("the failure is surfaced");
    assert_eq!(f.step, Step::Nodes);
    assert!(f.reason.starts_with("timeout"), "{}", f.reason);
    let t = Timing::frontend();
    let tries = w.count("admin:desired") as u64;
    assert_eq!(tries, t.desired_within.as_secs() / t.poll.as_secs() + 1);
    assert_eq!(w.holders(), vec![Slot::B]);
    assert!(!w.last_intent().nodes_posted);
}
