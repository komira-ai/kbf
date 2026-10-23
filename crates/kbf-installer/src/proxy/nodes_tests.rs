//! What the intent records of the plan's `nodes` flag and source. The swap sends the nodes no
//! order of its own. Against the fakes of [`super::fakes`].

use super::fakes::*;
use super::model::*;
use super::swap::Swap;

fn go(w: &World, plan: Plan, old: ProcIdent) -> Result<Phase, Halt> {
    Swap::begin(w.ports(), plan, Some(old))?.run()
}

/// Asserts that the intent (`update.json`) keeps the source of the update, and that the plan a
/// restart builds from the intent names the same source, so that an update whose release came
/// from the host's package is still one after the proxy restarted; and that the intent of an
/// update that fetched says `fetch`.
#[test]
fn the_intent_keeps_the_source_of_the_update_and_the_plan_of_a_restart_names_it() {
    let (w, old) = World::with_old();
    let staged = Plan {
        source: Source::Staged,
        ..plan(Mode::Frontend)
    };
    assert_eq!(go(&w, staged, old), Ok(Phase::Done));
    let intent = w.last_intent();
    assert_eq!(intent.source, Source::Staged);
    let resumed = Plan::resumed(&intent, Timing::frontend());
    assert_eq!(resumed.source, Source::Staged);

    let (w, old) = World::with_old();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(w.last_intent().source, Source::Fetch);
}

/// Asserts that an update that asks for its nodes records the flag, ends `done` with no failure,
/// and never enters the `nodes` phase or records the nodes as told: the swap sends no order to
/// them.
#[test]
fn an_update_that_asks_for_its_nodes_ends_done_without_a_nodes_phase() {
    let (w, old) = World::with_old();
    assert!(plan(Mode::Frontend).nodes);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    let intent = w.last_intent();
    assert!(intent.nodes);
    assert!(!intent.nodes_posted);
    assert!(intent.failure.is_none());
    assert!(!w.phases().contains(&Phase::Nodes));
}
