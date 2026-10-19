//! Rule R1: the gate on its own, from the snapshots a reader would build, and the gate
//! inside the core, where a refusal must fail the update and drop the stage.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use hyper::StatusCode;
use serde_json::json;

use super::compat::R1Gate;
use super::core::{Core, Reply};
use super::fakes::{Rig, empty_fleet, facts, req, staged};
use super::fleet::{Enrolled, FleetSnapshot, Population, Unreadable};
use super::model::StatusBody;
use super::traits::{Compat, CompatGate, Staged};
use crate::proxy::model::{Phase, StagedFacts, Step, Verb};
use crate::testkit::{B, c};

fn set(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|s| (*s).to_owned()).collect()
}

/// A view whose listed nodes are `names`, the oldest of which speaks `oldest`.
fn pop(names: &[&str], oldest: Option<u32>, unknown: u32) -> Population {
    Population {
        names: set(names),
        oldest,
        unknown,
    }
}

/// Daemons that are all at `daemons`, members all at `members`.
fn at(daemons: u32, members: u32) -> FleetSnapshot {
    FleetSnapshot {
        nodes: Ok(pop(&["mac-1"], Some(daemons), 0)),
        members: Ok(pop(&["store-1:8982"], Some(members), 0)),
        enrolled: Ok(Enrolled::default()),
    }
}

fn judged(min_protocol: u32, fleet: &FleetSnapshot) -> Compat {
    let facts = StagedFacts {
        min_protocol,
        ..facts()
    };
    R1Gate.check(
        &c(B),
        Some(Staged {
            facts: &facts,
            fleet,
        }),
    )
}

/// The detail of a refusal by R1; anything else is a failed test.
fn refusal(verdict: Compat) -> String {
    match verdict {
        Compat::Incompatible { rule, detail } => {
            assert_eq!(rule, "R1");
            detail
        }
        other => panic!("expected a refusal by R1, got {other:?}"),
    }
}

#[test]
fn a_release_whose_min_protocol_is_above_the_oldest_daemon_is_refused() {
    let detail = refusal(judged(4, &at(3, 9)));
    assert_eq!(
        detail,
        "the release's min_protocol 4 is above the oldest protocol of the daemons, 3"
    );
}

#[test]
fn a_min_protocol_up_to_the_oldest_is_compatible() {
    for min in [1, 2, 3] {
        assert_eq!(judged(min, &at(3, 3)), Compat::Compatible, "min {min}");
    }
}

#[test]
fn the_oldest_member_counts_as_well_as_the_oldest_daemon() {
    let detail = refusal(judged(3, &at(3, 2)));
    assert!(detail.contains("storage members, 2"), "{detail}");
    let detail = refusal(judged(3, &at(2, 3)));
    assert!(detail.contains("daemons, 2"), "{detail}");
    assert_eq!(judged(3, &at(3, 3)), Compat::Compatible);
}

#[test]
fn a_daemon_that_names_no_protocol_refuses() {
    // No daemon names one: `oldest` is None, which on its own would be vacuous.
    let mut fleet = at(9, 9);
    fleet.nodes = Ok(pop(&["old-1"], None, 1));
    let detail = refusal(judged(1, &fleet));
    assert_eq!(
        detail,
        "1 of the daemons name no protocol, so they cannot be judged"
    );
}

#[test]
fn a_member_that_names_no_protocol_refuses_whatever_the_others_say() {
    // The members say 9 for the rest, and two name nothing.
    let mut fleet = at(9, 9);
    fleet.members = Ok(pop(&["a:1", "b:1", "c:1"], Some(9), 2));
    let detail = refusal(judged(1, &fleet));
    assert_eq!(
        detail,
        "2 of the storage members name no protocol, so they cannot be judged"
    );
}

#[test]
fn a_view_that_lists_nodes_but_names_no_oldest_protocol_refuses() {
    // Each listed node either names a protocol, which sets `oldest`, or is counted in
    // `unknown`. Nodes listed, `oldest` unset and `unknown` 0 is a combination the server
    // cannot send; the numbers are not to be relied on, and `None` is not "all are fine".
    let mut daemons = at(9, 9);
    daemons.nodes = Ok(pop(&["d1"], None, 0));
    let mut members = at(9, 9);
    members.members = Ok(pop(&["a:1"], None, 0));
    let mut wrong = Vec::new();
    for (fleet, expect) in [
        (
            daemons,
            "the daemons are listed but the view names no oldest protocol, so they cannot be judged",
        ),
        (
            members,
            "the storage members are listed but the view names no oldest protocol, so they cannot be judged",
        ),
    ] {
        match judged(1, &fleet) {
            Compat::Incompatible { rule, detail } if rule == "R1" && detail == expect => {}
            other => wrong.push(format!("{expect}: {other:?}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn an_enrolled_daemon_the_child_has_not_seen_refuses() {
    let mut fleet = at(5, 5);
    fleet.nodes = Ok(pop(&["d1"], Some(5), 0));
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&["d1", "d2"]),
        storage: set(&[]),
    });
    let detail = refusal(judged(1, &fleet));
    assert!(detail.contains("enrolled daemons"), "{detail}");
    assert!(
        detail.ends_with(": d2"),
        "only the missing one is named: {detail}"
    );
    // Seen, it is judged like any other.
    fleet.nodes = Ok(pop(&["d1", "d2"], Some(5), 0));
    assert_eq!(judged(1, &fleet), Compat::Compatible);
}

#[test]
fn an_enrolled_storage_node_the_child_has_not_seen_refuses() {
    let mut fleet = at(5, 5);
    fleet.members = Ok(pop(&["store-1:8982"], Some(5), 0));
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&[]),
        storage: set(&["store-1:8982", "store-2:8982"]),
    });
    let detail = refusal(judged(1, &fleet));
    assert!(detail.contains("enrolled storage members"), "{detail}");
    assert!(detail.ends_with(": store-2:8982"), "{detail}");
    fleet.members = Ok(pop(&["store-1:8982", "store-2:8982"], Some(5), 0));
    assert_eq!(judged(1, &fleet), Compat::Compatible);
}

#[test]
fn a_listed_node_that_is_not_enrolled_is_judged_but_not_missing() {
    // A daemon with a hand-issued certificate is listed and enrolled nowhere.
    let mut fleet = at(5, 5);
    fleet.nodes = Ok(pop(&["hand-issued", "d1"], Some(5), 0));
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&["d1"]),
        storage: set(&[]),
    });
    assert_eq!(judged(5, &fleet), Compat::Compatible);
    assert!(matches!(judged(6, &fleet), Compat::Incompatible { .. }));
}

#[test]
fn a_child_from_before_the_protocol_fields_refuses_the_update() {
    // Both views as an older child writes them: the list and no word about protocols. Read
    // as "no node names one" they would pass any release.
    let nodes = json!({ "server": {}, "nodes": [{ "node_id": "mac-1", "protocol": 1 }] });
    let members = json!({ "generation": 3, "members": [{ "id": 1, "addr": "store-1:8982" }] });
    let fleet = FleetSnapshot {
        nodes: Population::from_nodes(StatusCode::OK, nodes.to_string().as_bytes()),
        members: Population::from_members(StatusCode::OK, members.to_string().as_bytes()),
        enrolled: Ok(Enrolled::default()),
    };
    let detail = refusal(judged(1, &fleet));
    assert!(
        detail.starts_with("the daemons could not be listed: GET /v1/nodes: "),
        "{detail}"
    );
    let fleet = FleetSnapshot {
        nodes: Ok(Population::default()),
        ..fleet
    };
    let detail = refusal(judged(1, &fleet));
    assert!(
        detail.starts_with("the storage members could not be listed: GET /v1/members: "),
        "{detail}"
    );
}

#[test]
fn a_node_is_the_same_whatever_the_case_of_its_address() {
    // Through the constructors, as the reader builds it: the member registered with one
    // spelling of the host and the enrollment recorded another.
    let members = json!({ "generation": 1, "oldest_protocol": 4, "protocol_unknown": 0,
        "members": [{ "id": 1, "addr": "Store-1.Example:8982", "state": "active",
                      "weight": 1, "protocol": 4 }] });
    let registry = json!({ "nodes": [{ "id": "s1", "roles": ["storage"], "state": "active",
        "advertise": "STORE-1.example:8982" }] });
    let fleet = FleetSnapshot {
        nodes: Ok(Population::default()),
        members: Population::from_members(StatusCode::OK, members.to_string().as_bytes()),
        enrolled: Enrolled::from_registry(StatusCode::OK, registry.to_string().as_bytes()),
    };
    assert_eq!(judged(4, &fleet), Compat::Compatible);
}

#[test]
fn a_read_that_failed_refuses_and_says_which() {
    type Break = fn(&mut FleetSnapshot, Unreadable);
    let cases: [(Break, &str); 3] = [
        (|f, e| f.nodes = Err(e), "the daemons could not be listed"),
        (
            |f, e| f.members = Err(e),
            "the storage members could not be listed",
        ),
        (
            |f, e| f.enrolled = Err(e),
            "the enrolled nodes could not be listed",
        ),
    ];
    // What each case answered instead of a refusal naming it; empty when all are right.
    let mut wrong = Vec::new();
    for (brk, expect) in cases {
        let mut fleet = empty_fleet();
        brk(
            &mut fleet,
            Unreadable("GET /v1/x answered 503 Service Unavailable".to_owned()),
        );
        match judged(1, &fleet) {
            Compat::Incompatible { rule, detail }
                if rule == "R1"
                    && detail.starts_with(expect)
                    && detail.ends_with("503 Service Unavailable") => {}
            other => wrong.push(format!("{expect}: {other:?}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn a_fleet_with_no_node_in_it_is_compatible_whatever_the_min_protocol() {
    // Nothing listed, nothing enrolled: no one can be locked out.
    assert_eq!(judged(9, &empty_fleet()), Compat::Compatible);
}

#[test]
fn before_the_release_is_staged_nothing_is_refused() {
    // Its min_protocol is not known yet, so R1 has nothing to compare.
    assert_eq!(R1Gate.check(&c(B), None), Compat::Compatible);
}

#[test]
fn refusals_come_in_the_documented_order() {
    // Daemons: unknown, then missing, then too old. All three hold at once.
    let mut fleet = at(1, 1);
    fleet.nodes = Ok(pop(&["d1"], Some(1), 1));
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&["d2"]),
        storage: set(&[]),
    });
    assert!(refusal(judged(9, &fleet)).contains("name no protocol"));
    fleet.nodes = Ok(pop(&["d1"], Some(1), 0));
    assert!(refusal(judged(9, &fleet)).contains("enrolled daemons"));
    fleet.enrolled = Ok(Enrolled::default());
    assert!(refusal(judged(9, &fleet)).contains("min_protocol 9"));
    // A failed read comes before any of them, and the daemons before the members.
    fleet.members = Err(Unreadable("down".to_owned()));
    assert!(refusal(judged(9, &fleet)).contains("storage members could not be listed"));
    assert!(refusal(judged(9, &at(1, 1))).contains("the daemons, 1"));
}

#[test]
fn a_long_list_of_missing_nodes_is_cut_and_counted() {
    let mut fleet = empty_fleet();
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&["d1", "d2", "d3", "d4", "d5", "d6", "d7"]),
        storage: set(&[]),
    });
    let detail = refusal(judged(1, &fleet));
    assert!(
        detail.ends_with(": d1, d2, d3, d4, d5 and 2 more"),
        "{detail}"
    );
    // Exactly as many as are shown: nothing more to count.
    fleet.enrolled = Ok(Enrolled {
        daemons: set(&["d1", "d2", "d3", "d4", "d5"]),
        storage: set(&[]),
    });
    let detail = refusal(judged(1, &fleet));
    assert!(detail.ends_with(": d1, d2, d3, d4, d5"), "{detail}");
}

// ---- inside the core ----------------------------------------------------------------

fn status_of(core: &mut Core) -> StatusBody {
    match core.status() {
        Reply::Status(b) => *b,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn written(rig: &Rig) -> Vec<(Verb, String)> {
    rig.orders
        .written
        .lock()
        .unwrap()
        .iter()
        .map(|l| (l.verb, l.commit.to_string()))
        .collect()
}

/// A core with the real gate and an update for B accepted; `staged(B)` carries
/// `min_protocol` 3.
fn accepted(rig: &Rig) -> Core {
    let mut core = rig.core_with(Box::new(R1Gate));
    assert!(matches!(core.post(req(B, "r1")), Ok(Reply::Update { .. })));
    core
}

#[test]
fn a_refusal_by_r1_fails_the_update_at_compat_and_drops_the_stage() {
    let rig = Rig::new("u9e-core-refused");
    *rig.fleet.snapshot.lock().unwrap() = at(2, 9);
    let mut core = accepted(&rig);
    rig.set_status(staged(B));
    let s = status_of(&mut core);
    assert_eq!(s.phase, Some(Phase::Failed));
    let f = s.failure.expect("a failure");
    assert_eq!(f.step, Step::Compat);
    assert_eq!(
        f.reason,
        "R1: the release's min_protocol 3 is above the oldest protocol of the daemons, 2"
    );
    assert_eq!(rig.sup.starts(), 0, "no child is started");
    assert_eq!(
        written(&rig),
        vec![
            (Verb::FrontendUpdate, B.to_owned()),
            (Verb::Revert, B.to_owned())
        ]
    );
}

#[test]
fn a_release_the_fleet_can_run_is_handed_to_the_supervisor() {
    let rig = Rig::new("u9e-core-passed");
    // The staged release's min_protocol is 3: equal to the oldest is enough.
    *rig.fleet.snapshot.lock().unwrap() = at(3, 3);
    let mut core = accepted(&rig);
    rig.set_status(staged(B));
    let s = status_of(&mut core);
    assert_ne!(s.phase, Some(Phase::Failed));
    assert!(s.failure.is_none());
    assert_eq!(rig.sup.starts(), 1);
    assert_eq!(written(&rig).len(), 1, "no revert");
}

#[test]
fn a_fleet_that_could_not_be_read_fails_the_update_closed() {
    let rig = Rig::new("u9e-core-unreadable");
    let mut snapshot = at(9, 9);
    snapshot.nodes = Err(Unreadable("GET /v1/nodes: timed out".to_owned()));
    *rig.fleet.snapshot.lock().unwrap() = snapshot;
    let mut core = accepted(&rig);
    rig.set_status(staged(B));
    let s = status_of(&mut core);
    assert_eq!(s.phase, Some(Phase::Failed));
    let f = s.failure.expect("a failure");
    assert_eq!(f.step, Step::Compat);
    assert!(
        f.reason.starts_with("R1: the daemons could not be listed"),
        "{}",
        f.reason
    );
    assert_eq!(rig.sup.starts(), 0);
    assert_eq!(written(&rig)[1], (Verb::Revert, B.to_owned()));
}

#[test]
fn the_fleet_is_read_once_and_only_to_judge_the_staged_release() {
    let rig = Rig::new("u9e-core-reads");
    let reads = || rig.fleet.reads.load(Ordering::SeqCst);
    let mut core = accepted(&rig);
    assert_eq!(reads(), 0, "POST judges the commit alone");
    let _ = status_of(&mut core);
    assert_eq!(reads(), 0, "nothing is staged yet");
    rig.set_status(staged(B));
    let _ = status_of(&mut core);
    assert_eq!(reads(), 1, "the staged release is judged against one read");
    let _ = status_of(&mut core);
    assert_eq!(
        reads(),
        1,
        "the update was handed over; it is not judged again"
    );
}
