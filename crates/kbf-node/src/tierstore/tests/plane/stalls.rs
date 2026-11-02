//! Reads held at the tierstore: what a stalled read becomes, what cancelling held and waiting
//! reads gives back, and leases reading at once while the tierstore fails. Progress is told by
//! the store's count of held reads and a gate the test opens; nothing waits on a clock.

use super::*;

/// The most reads that ask the packs at once (`kbf_daemon`'s `MAX_ASKING`, which
/// `docs/deploy/kbf-modes.md` gives as 16).
const ASKING: usize = 16;

/// Asserts that a read held at the tierstore that then fails (a request that timed out) is read
/// from the frontend and succeeds, counted as unasked.
/// Catches a stalled tierstore read that ends in a failed read rather than a fallback.
#[tokio::test]
async fn a_stalled_read_that_times_out_falls_back_to_the_frontend() {
    let rig = Arc::new(Rig::new());
    let digest = rig.pack(b"stalled").await;
    rig.front.insert(b"stalled");
    rig.tier.set(Mode::HeldThenFail);
    let reading = {
        let rig = Arc::clone(&rig);
        let digest = digest.clone();
        tokio::spawn(async move { rig.cas.get(&digest).await })
    };
    rig.tier.held(1).await;
    rig.tier.open();
    assert_eq!(reading.await.expect("joined"), Ok(b"stalled".to_vec()));
    assert_eq!(rig.front.calls(), ["get"]);
    assert_eq!(rig.counter.stats().unasked, 1);
}

/// Asserts that cancelling reads held at the tierstore, and the reads waiting for their turn to
/// ask, gives back every turn: once all are cancelled, as many reads as may ask at once, and one
/// more, all reach the tierstore and are served from it with the frontend never asked.
/// Catches a turn kept by a cancelled read, which would leave later reads waiting for good.
#[tokio::test]
async fn cancelled_reads_give_back_their_turns() {
    let rig = Arc::new(Rig::new());
    let digest = rig.pack(b"turns").await;
    rig.tier.set(Mode::HeldThenServe);
    let spawn = |rig: &Arc<Rig>| {
        let rig = Arc::clone(rig);
        let digest = digest.clone();
        tokio::spawn(async move { rig.cas.get(&digest).await })
    };
    // As many as may ask, all held at the store, and two more waiting for a turn.
    let held: Vec<_> = (0..ASKING + 2).map(|_| spawn(&rig)).collect();
    rig.tier.held(ASKING).await;
    for reading in &held {
        reading.abort();
    }
    for reading in held {
        assert!(reading.await.expect_err("cancelled").is_cancelled());
    }
    rig.tier.held(0).await;
    rig.tier.set(Mode::Serve);
    let again: Vec<_> = (0..=ASKING).map(|_| spawn(&rig)).collect();
    for reading in again {
        assert_eq!(reading.await.expect("joined"), Ok(b"turns".to_vec()));
    }
    assert_eq!(rig.front.calls(), Vec::<&str>::new());
    assert_eq!(rig.counter.stats().served, ASKING as u64 + 1);
}

/// Asserts that leases reading at once while the tierstore is down all complete from the
/// frontend, and that, held at a tierstore that then answers, they are all served from it.
#[tokio::test]
async fn leases_reading_at_once_complete_while_the_tierstore_fails_and_after() {
    let rig = Arc::new(Rig::new());
    let digest = rig.pack(b"shared input").await;
    rig.front.insert(b"shared input");
    let leases: Vec<_> = (1..=4)
        .map(|n| rig.cas.lease(&kbf_types::LeaseId::new(1, n)))
        .collect();
    rig.tier.set(Mode::Down);
    let reads = leases.iter().map(|lease| lease.get(&digest));
    for read in futures::future::join_all(reads).await {
        assert_eq!(read, Ok(b"shared input".to_vec()));
    }
    let from_front = rig
        .front
        .calls()
        .iter()
        .filter(|call| **call == "get")
        .count();
    assert_eq!(from_front, 4);

    rig.tier.set(Mode::HeldThenServe);
    let reading: Vec<_> = leases
        .into_iter()
        .map(|lease| {
            let digest = digest.clone();
            tokio::spawn(async move { lease.get(&digest).await })
        })
        .collect();
    rig.tier.held(4).await;
    rig.tier.open();
    for read in reading {
        assert_eq!(read.await.expect("joined"), Ok(b"shared input".to_vec()));
    }
    let from_front = rig
        .front
        .calls()
        .iter()
        .filter(|call| **call == "get")
        .count();
    assert_eq!(from_front, 4);
    assert_eq!(rig.counter.stats().served, 4);
}

/// The plane over the daemon's own tierstore client, reaching a far end that `far` says, with
/// the places of `rig`'s index and `rig`'s frontend under it.
fn over_the_client(rig: &Rig, far: crate::tierstore::tests::wire::Far) -> Arc<PackedCas<Front>> {
    use crate::tierstore::client::{Dial, TierClient};
    use crate::tierstore::tests::wire::Scripted;
    let endpoint = loopback("http://127.0.0.1:9300").expect("a loopback endpoint");
    let keys = crate::pack_read::KeyPair {
        id: "tier-key".to_owned(),
        secret: zeroize::Zeroizing::new("tier-secret".to_owned()),
    };
    let dial: Arc<dyn Dial> = Scripted::new(vec![far]);
    let timeouts = (
        crate::pack_read::CONNECT_TIMEOUT,
        crate::pack_read::REQUEST_TIMEOUT,
    );
    let client = TierClient::new(
        &endpoint,
        "tierstore".to_owned(),
        keys,
        "us-east-1",
        timeouts,
        dial,
    );
    let index: Arc<dyn PackIndex> = rig.asked.clone();
    over(
        Arc::new(rig.front.clone()),
        Arc::new(client),
        index,
        &rig.counter,
    )
}

/// Asserts that a read whose tierstore takes the request and never answers, and one whose
/// tierstore answers the head and part of the body and then nothing more, each end at the
/// daemon's own request timeout (the paused clock moves only to the client's timer) and are read
/// from the frontend, successfully, counted as unasked.
/// Catches a stalled tierstore that holds a lease's input fetch for good, or fails it.
#[tokio::test(start_paused = true)]
async fn a_stalled_tierstore_ends_at_the_daemons_deadline_and_the_frontend_answers() {
    use crate::tierstore::tests::wire::Far;
    let head = b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-6/7\r\ncontent-length: 7\r\n\r\nsta";
    for (n, far) in [Far::Silent, Far::Stalls(head.to_vec())]
        .into_iter()
        .enumerate()
    {
        let rig = Rig::new();
        let digest = rig.forge(b"stalled", None).await;
        rig.front.insert(b"stalled");
        let cas = over_the_client(&rig, far);
        assert_eq!(cas.get(&digest).await, Ok(b"stalled".to_vec()), "case {n}");
        assert_eq!(rig.front.calls(), ["get"], "case {n}");
        assert_eq!(rig.counter.stats().unasked, 1, "case {n}");
    }
}
