//! `Scheduler::capacity`, the view `GET /v1/nodes/{id}` reads beside `booked`.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Scheduler};
use kbf_types::{FarmTime, Resources, StateMachine, WorkerId};

const GIB: u64 = 1 << 30;

fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn apply(s: &mut Scheduler, event: Event) {
    assert!(s.apply(Input::new(FarmTime::default(), event)).is_empty());
}

/// Catches: a capacity read from the wrong place (the booking, the first `Hello`'s
/// rather than the newest, zero for a registered worker), and a worker that never
/// registered given a capacity of zero instead of none (which a node page would show
/// as a node with nothing).
#[test]
fn capacity_is_the_newest_report_and_none_for_a_stranger() {
    let mut s = Scheduler::new(1);
    let node = WorkerId::new("a");
    assert_eq!(s.capacity(&node), None, "a worker that never registered");
    apply(
        &mut s,
        Event::WorkerUp {
            worker: node.clone(),
            instance: DaemonInstance::new("a"),
            predecessor_stopped: false,
            capacity: Resources::new(4_000, 8 * GIB),
            caps: caps(),
        },
    );
    assert_eq!(s.capacity(&node), Some(Resources::new(4_000, 8 * GIB)));
    assert_eq!(s.booked(&node), Some(Resources::default()));
    apply(
        &mut s,
        Event::Capacity {
            worker: node.clone(),
            capacity: Resources::new(2_000, 4 * GIB),
            caps: caps(),
        },
    );
    assert_eq!(s.capacity(&node), Some(Resources::new(2_000, 4 * GIB)));
    assert_eq!(s.capacity(&WorkerId::new("b")), None);
}
