//! What a caller does with a refused preparation, and the mint ingest must verify with.
//!
//! A run whose inputs cannot be read now may be prepared later, so it is the farm's to retry;
//! one whose inputs are missing, malformed or over budget, or whose lease or time no key can
//! carry, gets the same answer every time, so it is the caller's to fix.

use super::*;

#[tokio::test]
async fn a_store_that_cannot_be_read_is_an_unavailable_fault() {
    let (parts, store, prep) = standard().await;
    store.reads_down.store(true, SeqCst);
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert_eq!(error.fault(), PrepareFault::Unavailable, "{error}");
}

#[tokio::test]
async fn a_store_whose_records_disagree_is_refused_not_retried() {
    let (parts, store, prep) = standard().await;
    store.reads_broken.store(true, SeqCst);
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert!(
        matches!(&error, PrepareError::Closure(ClosureError::Invalid { .. })),
        "{error}"
    );
    assert_eq!(error.fault(), PrepareFault::Refused, "{error}");
}

#[tokio::test]
async fn a_store_that_cannot_say_what_it_holds_for_good_is_refused() {
    let (parts, store, prep) = standard().await;
    store.asks_broken.store(true, SeqCst);
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert_eq!(error.fault(), PrepareFault::Refused, "{error}");
}

#[tokio::test]
async fn a_missing_input_is_refused_not_unavailable() {
    let parts = Parts::new();
    let (_, _, prep) = missing(&[parts.tree.b]).await;
    let error = refused(prep.prepare_run(LEASE, &parts.action, now()).await);
    assert_eq!(error.fault(), PrepareFault::Refused, "{error}");
}

#[tokio::test]
async fn a_tree_over_budget_is_refused_not_unavailable() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let mut over = config();
    over.limits.max_digests = 8;
    let error = refused(
        prep_with(&store, over)
            .prepare_run(LEASE, &parts.action, now())
            .await,
    );
    assert_eq!(error.fault(), PrepareFault::Refused, "{error}");
}

#[tokio::test]
async fn a_lease_no_key_can_carry_is_refused_not_unavailable() {
    let (parts, _, prep) = standard().await;
    let error = refused(prep.prepare_run("a/b", &parts.action, now()).await);
    assert_eq!(error.fault(), PrepareFault::Refused, "{error}");
}

#[tokio::test]
async fn the_mint_a_preparer_lends_verifies_the_keys_it_issued() {
    let (parts, _, prep) = standard().await;
    let inputs = prep
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    let minted = prep
        .mint()
        .verify(&inputs.key, &parts.action)
        .expect("the preparer's own mint verifies its key");
    assert_eq!(minted, now());
    let stranger = PackKeyMint::new(b"another secret of thirty-two bytes!").expect("a mint");
    assert!(
        stranger.verify(&inputs.key, &parts.action).is_err(),
        "a mint with another secret does not"
    );
}
