//! The fake server's planned faults that meet a server which is already down.

use std::time::Duration;

use kbf_sim::{Event, NodeId, NodeInput};
use kbf_sim_cell::{Server, ServerFault, ServerPlan};
use kbf_types::{FarmTime, StateMachine};

/// The tag of the timer for the plan's `index`-th fault, and of the restart timer.
const FAULT: u64 = 1 << 16;
const START: u64 = 1;

fn at(millis: u64, event: Event<kbf_sim_cell::Msg>) -> NodeInput<kbf_sim_cell::Msg> {
    NodeInput {
        now: FarmTime::from_millis(millis),
        entropy: 1,
        event,
    }
}

/// A fault that meets a server already down does nothing: the server stays down, is not
/// restarted early by it, and comes back at its one planned restart.
#[test]
fn faults_that_meet_a_downed_server_leave_it_down_until_its_restart() {
    let when = FarmTime::from_millis(100);
    let faults = vec![
        (when, ServerFault::Kill),
        (when, ServerFault::Kill),
        (when, ServerFault::PowerCut { keep: 0, names: 0 }),
        (
            when,
            ServerFault::PowerCutAt {
                after: 0,
                keep: 0,
                names: 0,
            },
        ),
        (when, ServerFault::IoErrorAt { after: 0 }),
    ];
    let plan = ServerPlan {
        faults,
        down_for: Duration::from_secs(3),
        tier: None,
    };
    let mut s = Server::new(plan, NodeId::from("store"));
    s.apply(at(0, Event::Start));
    assert!(s.meta().is_some(), "the server comes up");
    for i in 0..5 {
        s.apply(at(100, Event::Timer { tag: FAULT + i }));
        assert!(s.meta().is_none(), "down after fault {i}");
    }
    s.apply(at(3_100, Event::Timer { tag: START }));
    assert!(s.meta().is_some(), "the planned restart brings it back");
}
