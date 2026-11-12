//! `prepare_put_dir`: the put directory of a run that commits into one, and the closure its
//! action tree is written from. No write is signed and no grant is issued.

use kbf_front::IssuedPutDir;
use kbf_front::prepare_run::PutDirRun;

use super::*;

async fn prepared(prep: &RunPrep, action: &Digest, wall: Duration) -> PutDirRun {
    prep.prepare_put_dir(LEASE, action, now(), wall)
        .await
        .expect("prepared")
}

/// Asserts that a run gets the put directory of its lease that the mint verifies for its
/// action (and no other), good until its start, its wall and the slack, and the closure with
/// every `Directory` of its input root kept.
#[tokio::test]
async fn a_run_gets_a_dir_the_mint_verifies_and_the_kept_closure() {
    let (parts, store, prep) = standard().await;
    let run = prepared(&prep, &parts.action, WALL).await;
    let mint = PackKeyMint::new(MINT_SECRET).expect("a mint");
    assert_eq!(run.dir.build(), LEASE);
    assert!(
        run.dir.dir().starts_with(&format!("cas/b/{LEASE}/")),
        "{}",
        run.dir.dir()
    );
    assert_eq!(mint.verify_put_dir(&run.dir, &parts.action), Ok(now()));
    assert!(mint.verify_put_dir(&run.dir, &blob("another").0).is_err());
    assert_eq!(run.not_after, NOW_SECS + WALL.as_secs() + SLACK.as_secs());
    assert_eq!(run.kept.root(), &parts.tree.root);
    assert_eq!(run.kept.directory_count(), 4);
    let inputs: BTreeSet<Digest> = run.kept.closure().inputs().copied().collect();
    assert_eq!(inputs, parts.tree.expected);
    assert!(!store.reads().is_empty());
    let again = prepared(&prep, &parts.action, WALL).await;
    assert_eq!(
        again.dir, run.dir,
        "one lease, action and minute: one directory"
    );
    let _: &IssuedPutDir = &again.dir;
}

/// Asserts each refusal of the preparation, by what it is and whose it is to fix: a lease that
/// is no build id, a wall over the ceiling, a time before the epoch, a missing input (the
/// caller's), and a store that cannot be read (the farm's).
#[tokio::test]
async fn a_put_dir_run_is_refused_as_a_run_of_writes_is() {
    let (parts, store, prep) = standard().await;
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

    let (_, _, holed) = missing(&[parts.tree.b]).await;
    let missing_input = holed
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert_eq!(
        missing_input.fault(),
        PrepareFault::Refused,
        "{missing_input}"
    );

    store.reads_down.store(true, SeqCst);
    let down = prep
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect_err("refused");
    assert_eq!(down.fault(), PrepareFault::Unavailable, "{down}");
}
