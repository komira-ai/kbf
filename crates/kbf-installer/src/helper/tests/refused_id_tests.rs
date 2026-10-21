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
/// in the status over the passes that follow (it is in `helper.json`, which every pass reads),
/// and goes when an order is carried out, from the status and from `helper.json` both, so that
/// the file an older build reads is the one it has always read.
///
/// Mutants planted: the id never written; the id kept after an order that was carried out; the
/// id written for the order that came before.
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
    assert!(memory_text(&rig).contains(r#""refused_order_id":4"#));

    rig.cycle(T0 + 3);
    assert_eq!(rig.status()["refused_order_id"], 4, "kept when quiet");

    *rig.fetcher.fail.lock().expect("lock") = None;
    rig.order(5, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 6);
    let status = rig.status();
    assert_eq!(status["state"], "staged");
    assert!(status.get("refused_order_id").is_none(), "{status}");
    let memory = memory_text(&rig);
    assert!(!memory.contains("refused_order_id"), "{memory}");
}

/// Catches the id left out for the node's orders (it is one record format, one status), and
/// left on for a refusal that is no order's: a record the reader would not take, and a restart
/// that failed, answer nobody's order.
///
/// Mutants planted: the id written only for the proxy's orders; the id not cleared by a refusal
/// that names none.
#[test]
fn a_refused_node_order_names_itself_and_a_refusal_with_no_order_names_none() {
    let mut rig = Rig::new("refused-id-node");
    *rig.fetcher.fail.lock().expect("lock") =
        Some(FetchError::Unavailable("source unreachable".to_owned()));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    let status = rig.status();
    assert_eq!(status["state"], "refused");
    assert_eq!(status["refused_order_id"], 5);

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
