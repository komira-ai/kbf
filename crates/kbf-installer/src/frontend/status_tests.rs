//! The `frontend` section of `status.json`: which frontend order each outcome belongs to.
//! `kbf-server` closes its `update/frontend` row on `frontend.order_id` equal to the row's
//! and a terminal `state`; the section is its only evidence, so each test reads the file the
//! way the server does.

use serde_json::{Value, json};

use super::pending::Instance;
use super::sim::{T0, World, record};
use crate::helper::fetcher::FetchError;
use crate::testkit::{A, B, C};

const A_: Instance = Instance::A;
const B_: Instance = Instance::B;

fn pass(h: &mut crate::helper::cycle::Helper<super::sim::SimExec>, n: u64) {
    h.cycle(T0 + 3 * n).expect("a pass");
}

fn section(w: &World) -> Value {
    w.status()["frontend"].clone()
}

/// The status of a host that never had a frontend order has no section (an installer without
/// one says the same), and the order in flight is `pending` under its own id and epoch, not
/// the node floor's.
///
/// Mutants planted: the section always written (null); the order id taken from
/// `acked_order_id`; the epoch dropped.
#[test]
fn no_section_until_a_frontend_order_and_pending_names_the_order() {
    let w = World::new("fs-pending", A_);
    let mut h = w.helper();
    pass(&mut h, 0);
    assert!(w.status().get("frontend").is_none(), "{}", w.status());
    w.put(&record(5, 3, 1, B));
    pass(&mut h, 1);
    assert_eq!(
        section(&w),
        json!({"order_id": 5, "epoch": 3, "commit": B, "state": "pending"})
    );
    assert_eq!(
        w.status()["acked_order_id"],
        0,
        "the node floor is not the frontend's"
    );
}

/// The kept update: `confirmed` under the order's id, no `refusal` key, the file still
/// readable by the node client (which ignores the section), and a newer order that is refused
/// replaces it, so a stale outcome never stands for the newer order.
///
/// Mutants planted: `last` not replaced by the next order's end (the first order's
/// `confirmed` stays); the confirm written under the wrong order id.
#[test]
fn a_kept_update_is_confirmed_under_its_order_and_a_later_order_replaces_it() {
    let w = World::new("fs-confirmed", A_);
    let mut h = w.helper();
    w.put(&record(5, 3, 1, B));
    for n in 0..4 {
        pass(&mut h, n);
    }
    assert_eq!(
        section(&w),
        json!({"order_id": 5, "epoch": 3, "commit": B, "state": "confirmed"})
    );
    let read = kbf_update::helper::read(&w.root.join(super::sim::UPDATE).join("status.json"));
    assert!(
        matches!(read, kbf_update::helper::Helper::Reported(_)),
        "{read:?}"
    );

    // Instance `a` still drains: the next order is refused busy, and it is the one named.
    w.put(&record(6, 3, 2, C));
    pass(&mut h, 4);
    let s = section(&w);
    assert_eq!(
        (&s["order_id"], &s["state"]),
        (&json!(6), &json!("refused")),
        "{s}"
    );
    assert!(
        s["refusal"]
            .as_str()
            .expect("text")
            .starts_with("frontend_busy: ")
    );
    assert_eq!(s["commit"], C);
}

/// The undone update: `reverted` under its order with the reason; a new order is `pending`
/// under its own id, not the earlier order's end.
///
/// Mutants planted: the pending record not outranking `last` (the earlier `reverted` shows
/// for the new order); the reason dropped.
#[test]
fn an_undone_update_is_reverted_under_its_order() {
    let w = World::new("fs-reverted", A_);
    w.sim.borrow_mut().stands_down = false;
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    h.cycle(T0 + 601).expect("past the window");
    h.cycle(T0 + 604).expect("b is down");
    let s = section(&w);
    assert_eq!(
        (&s["order_id"], &s["state"]),
        (&json!(5), &json!("reverted")),
        "{s}"
    );
    assert!(
        s["refusal"]
            .as_str()
            .expect("text")
            .contains("not confirmed within the window"),
        "{s}"
    );
    // A new order is pending under its own id: the revert of 5 is not its word.
    w.put(&record(8, 1, 2, B));
    h.cycle(T0 + 700).expect("next order");
    assert_eq!(
        section(&w),
        json!({"order_id": 8, "epoch": 1, "commit": B, "state": "pending"})
    );
}

/// A refusal before anything moved, and an order for the release that already serves, each end
/// under their order: `refused` with the reason, `confirmed` with none.
///
/// Mutants planted: the refusal not recorded (section absent); `already running` left
/// unrecorded (the row would wait for its deadline).
#[test]
fn a_refused_order_and_one_already_running_end_under_their_orders() {
    let w = World::new("fs-refused", A_);
    *w.fetcher.fail.lock().expect("l") = Some(FetchError::Unavailable("down".to_owned()));
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    assert_eq!(
        section(&w),
        json!({"order_id": 5, "epoch": 1, "commit": B, "state": "refused",
               "refusal": "fetch: fetch: down"})
    );

    let w = World::new("fs-already", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, A));
    pass(&mut h, 0);
    assert_eq!(
        section(&w),
        json!({"order_id": 5, "epoch": 1, "commit": A, "state": "confirmed"})
    );
}

/// An update that needs an operator is `needs_operator` with the reason: not terminal, so a
/// reader that closes on confirmed, reverted or refused keeps the row.
#[test]
fn an_update_that_needs_an_operator_says_so() {
    let w = World::new("fs-operator", A_);
    w.sim.borrow_mut().stop_hangs[0] = true;
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    w.sim.borrow_mut().die(B_);
    for n in 1..100 {
        pass(&mut h, n);
    }
    h.cycle(T0 + 3 + 300).expect("at the bound");
    let s = section(&w);
    assert_eq!(
        (&s["order_id"], &s["state"]),
        (&json!(5), &json!("needs_operator")),
        "{s}"
    );
    assert!(
        s["refusal"]
            .as_str()
            .expect("text")
            .contains("nothing was killed")
    );
}

/// A node outcome never touches the section, and a reboot (a new helper, the status file gone)
/// rebuilds it from `frontend.json`, in flight and ended alike.
///
/// Mutants planted: a node activate writing the section; the section kept only in memory of
/// the pass (empty after the restart).
#[test]
fn node_outcomes_leave_the_section_alone_and_a_reboot_rebuilds_it() {
    let w = World::new("fs-reboot", A_);
    let mut h = w.helper();
    w.put(&record(5, 3, 1, B));
    pass(&mut h, 0);
    let pending = section(&w);
    let status = w.root.join(super::sim::UPDATE).join("status.json");
    std::fs::remove_file(&status).expect("rm");
    let mut h2 = w.helper();
    pass(&mut h2, 1);
    assert_eq!(section(&w), pending, "rebuilt while in flight");

    for n in 2..5 {
        pass(&mut h2, n);
    }
    let ended = section(&w);
    assert_eq!(ended["state"], "confirmed");
    // A node order (here a refusal on a live lease, then an activation) leaves it alone.
    w.leases.set(1);
    w.put_node_activate(9, C);
    pass(&mut h2, 5);
    assert_eq!(w.status()["state"], "refused");
    assert_eq!(section(&w), ended);
    w.leases.set(0);
    w.put_node_activate(10, C);
    pass(&mut h2, 6);
    assert_eq!(w.state().current, Some(crate::testkit::c(C)));
    assert_eq!(section(&w), ended);
    std::fs::remove_file(&status).expect("rm");
    let mut h3 = w.helper();
    pass(&mut h3, 7);
    assert_eq!(section(&w), ended, "rebuilt after the end");
}
