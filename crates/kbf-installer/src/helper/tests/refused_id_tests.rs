//! Which order a refusal answers (`refused_order_id` in `status.json`): what lets the frontend
//! proxy tell the refusal of its own order from an older one, so that an update whose order the
//! installer refused ends with the installer's word and not at its staging deadline.

use super::*;

/// The text of `helper.json`.
fn memory_text(rig: &Rig) -> String {
    fs::read_to_string(rig.root.join("var/lib/kbf-installer/helper.json")).expect("memory")
}

/// Catches a refusal of the proxy's order that does not say whose it is: the update waits out
/// its staging deadline for a refusal that was written at once. The id is the order's, stays
/// in the status over the passes that follow (the helper keeps it from one pass to the next),
/// and goes when an order is carried out. It is never written to `helper.json`: that file is
/// read by builds that refuse a key they do not know, and a host put back on an older package
/// after a refusal would otherwise fail every pass.
///
/// Mutants planted: the id never written; the id kept after an order that was carried out; the
/// id written for the order that came before; the id written to `helper.json`.
#[test]
fn a_refused_frontend_order_names_itself_until_another_order_is_carried_out() {
    let mut rig = Rig::frontend("refused-id-frontend");
    *rig.fetcher.fail.lock().expect("lock") =
        Some(FetchError::Unavailable("source unreachable".to_owned()));
    rig.order(4, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert_eq!(status["refused_order_id"], 4);
    let memory = memory_text(&rig);
    assert!(!memory.contains("refused_order_id"), "{memory}");

    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["refused_order_id"], 4, "kept when quiet");

    *rig.fetcher.fail.lock().expect("lock") = None;
    rig.order(5, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 6);
    let status = rig.status();
    assert_eq!(status["state"], "staged");
    assert!(status.get("refused_order_id").is_none(), "{status}");
}

/// Catches a refusal of the node's order being taken for the proxy's. The two kinds of order
/// are numbered by different counters, and the proxy blames a refusal on its update by the id
/// alone, so a node order 5 refused on a host that runs both would end the proxy's update 5 at
/// once with a refusal that is not its own. The status of a refused node order names none.
///
/// Mutants planted: the id written for the node's orders as well.
#[test]
fn a_refused_node_order_names_no_order_even_on_a_host_with_a_frontend() {
    let mut rig = Rig::frontend("refused-id-node");
    *rig.fetcher.fail.lock().expect("lock") =
        Some(FetchError::Unavailable("source unreachable".to_owned()));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert!(status.get("refused_order_id").is_none(), "{status}");
}

/// Catches a node order refused for its epoch (before it is acted on) naming its id: the same
/// rule as above, for the refusals made before an order is carried out.
///
/// Mutant planted: the id written for the node's orders in the older-epoch refusal.
#[test]
fn a_node_order_refused_for_its_epoch_names_no_order() {
    let mut rig = Rig::new("refused-id-node-epoch");
    rig.put(&rig.rec(9, 2, 1, Verb::Activate, B));
    rig.cycle(T0);
    assert_eq!(rig.current(), Some(c(B)));

    rig.put(&rig.rec(10, 1, 1, Verb::Activate, A));
    rig.cycle(T0 + 3);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert!(status.get("refused_order_id").is_none(), "{status}");
}

/// Catches an id left on for a refusal that is no order's: a record the reader would not take
/// answers nobody's order, and the id of the refusal before it must not stay on it.
///
/// Mutant planted: the id not cleared by a refusal that names none.
#[test]
fn a_refusal_with_no_order_clears_the_id_of_the_one_before() {
    let mut rig = Rig::frontend("refused-id-cleared");
    *rig.fetcher.fail.lock().expect("lock") =
        Some(FetchError::Unavailable("source unreachable".to_owned()));
    rig.order(4, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    assert_eq!(rig.status()["refused_order_id"], 4);

    rig.put_raw(1, 6, b"not an order");
    rig.cycle(T0 + 3);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert!(status.get("refused_order_id").is_none(), "{status}");
}

/// Catches the refusals made before an order is acted on (its epoch is older than the last
/// acted on, or jumps too far above it) naming no order: they are the order's refusals too,
/// and the update that wrote it would wait out its staging deadline for them.
///
/// Mutants planted: the id left out of the older-epoch refusal; left out of the jump refusal.
#[test]
fn an_order_refused_for_its_epoch_names_itself() {
    let mut rig = Rig::frontend("refused-id-epoch");
    rig.put(&rig.rec(9, 2, 1, Verb::FrontendUpdate, B));
    rig.cycle(T0);
    assert_eq!(rig.state().staged, Some(c(B)));

    rig.put(&rig.rec(10, 1, 1, Verb::FrontendUpdate, B));
    rig.cycle(T0 + 3);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert_eq!(status["refused_order_id"], 10);

    rig.put(&rig.rec(11, 100_000, 1, Verb::FrontendUpdate, B));
    rig.cycle(T0 + 6);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert_eq!(status["refused_order_id"], 11);
}
