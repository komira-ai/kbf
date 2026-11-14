//! `PutDirPrep`: the put directory of a run, from the mint, the slack and the closure budget
//! alone, with none of the write target, the credentials or the read listener's URL and key.

use kbf_front::prepare_run::PutDirPrep;

use super::*;

fn only(store: &Arc<Watched>) -> PutDirPrep {
    let storage: Arc<dyn Storage> = store.clone();
    PutDirPrep::new(
        Backend::remote(storage),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    )
}

/// Asserts that a preparer made without any write or read setting gives the run the same put
/// directory, kept closure and expiry as a full preparer of the same mint, so the ingest that
/// verifies with that mint accepts it.
#[tokio::test]
async fn a_put_dir_only_preparer_gives_the_dir_a_full_preparer_gives() {
    let (parts, store, full) = standard().await;
    let prep = only(&store);
    let run = prep
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("prepared");
    let same = full
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("prepared");
    assert_eq!(run.dir, same.dir);
    assert_eq!(run.not_after, NOW_SECS + WALL.as_secs() + SLACK.as_secs());
    assert_eq!(run.kept.directory_count(), same.kept.directory_count());
    assert_eq!(
        prep.mint().verify_put_dir(&run.dir, &parts.action),
        Ok(now())
    );
}

/// Asserts that it refuses as a full preparer does: a lease that is no build id, a wall over the
/// ceiling, a time before the epoch, and an input the farm does not hold.
#[tokio::test]
async fn a_put_dir_only_preparer_refuses_what_a_full_one_refuses() {
    let (parts, store, _) = standard().await;
    let prep = only(&store);
    let bad_lease = prep
        .prepare_put_dir("a.b", &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert!(matches!(bad_lease, PrepareError::Lease(_)), "{bad_lease}");
    let long = prep
        .prepare_put_dir(LEASE, &parts.action, now(), MAX_EXPIRY)
        .await
        .expect_err("refused");
    assert!(matches!(long, PrepareError::Expiry(_)), "{long}");
    let early = UNIX_EPOCH - Duration::from_secs(1);
    let clock = prep
        .prepare_put_dir(LEASE, &parts.action, early, WALL)
        .await
        .expect_err("refused");
    assert!(matches!(clock, PrepareError::Clock), "{clock}");
    let (_, holed, _) = missing(&[parts.tree.b]).await;
    let missing_input = only(&holed)
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert_eq!(
        missing_input.fault(),
        PrepareFault::Refused,
        "{missing_input}"
    );
}
