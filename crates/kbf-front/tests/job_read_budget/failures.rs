//! The budget and the scope memory when reads fail, are dropped or fail to walk, and the bound of
//! the scope walk: what the other tests of this file leave out.

use kbf_front::job_read::STREAM_OVER_BYTES;

use super::*;

/// Catches: a walk that collects a whole level of Directories before it looks at the bound
/// (a tenant's tree of tens of thousands of Directories that each name tens of thousands of
/// files would pile up billions of digests first): with 40 child Directories of ten files each
/// and a bound of 100 digests, the walk gives no scope as soon as the bound is crossed, and
/// the Directories it had not yet begun to read are not read.
#[tokio::test]
async fn a_level_over_the_scope_bound_stops_the_walk_before_its_directories_are_all_read() {
    let mut blobs = HashMap::new();
    let mut children = Vec::new();
    for i in 0..40u8 {
        let files = (0..10u8)
            .map(|j| FileNode {
                name: format!("f{j}"),
                digest: Some(digest_to_proto(&Digest::new(
                    kbf_types::DigestFunction::Sha256,
                    [
                        i, j, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                        1, 1, 1, 1, 1, 1, 1,
                    ],
                    5,
                ))),
                ..FileNode::default()
            })
            .collect();
        let child = Directory {
            files,
            ..Directory::default()
        };
        children.push(put(&mut blobs, child.encode_to_vec()));
    }
    let directories = children
        .iter()
        .enumerate()
        .map(|(i, digest)| DirectoryNode {
            name: format!("d{i}"),
            digest: Some(digest_to_proto(digest)),
        })
        .collect();
    let root = Directory {
        directories,
        ..Directory::default()
    };
    let root = put(&mut blobs, root.encode_to_vec());
    let command = put(&mut blobs, b"the command".to_vec());
    let action = Action {
        command_digest: Some(digest_to_proto(&command)),
        input_root_digest: Some(digest_to_proto(&root)),
        ..Action::default()
    };
    let action = put(&mut blobs, action.encode_to_vec());
    let limits = Limits {
        scope_digests: 100,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let reply = rig.get(&command, &rig.token(action)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(&reply.body[..], b"no_scope\n");
    let read: usize = children.iter().map(|d| rig.source.reads_of(d)).sum();
    assert!(read < 40, "{read} of 40 Directories were read");
}

/// Catches: the budget not coming back when a read ends in an error: a blob the store does not
/// hold, a store that fails, in a GET, in a batch and in a streamed read. With a budget that
/// one read takes whole, each failure is followed by a read of another blob that is served.
#[tokio::test]
async fn the_budget_comes_back_when_a_read_fails() {
    // A blob over `stream_over` is streamed: both ways of reading are tried.
    for stream_over in [2000, STREAM_OVER_BYTES] {
        let mut blobs = HashMap::new();
        let contents: Vec<Vec<u8>> = (0..3).map(|i| pattern(i, 3000)).collect();
        let t = build(&mut blobs, "a", &contents, 0);
        let limits = Limits {
            read_budget_bytes: 4000,
            stream_over,
            ..Limits::default()
        };
        let rig = rig(blobs, None, limits);
        let token = rig.token(t.action);
        assert_eq!(rig.get(&t.command, &token).await.status, StatusCode::OK);
        let served = |reply: Reply| reply.status;

        rig.source.hide(t.files[0]);
        assert_eq!(
            rig.get(&t.files[0], &token).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(served(rig.get(&t.files[1], &token).await), StatusCode::OK);
        let reply = rig.batch(&t.files[..1], &token).await;
        assert_eq!(&reply.body[..], &[vec![1u8], vec![0u8; 8]].concat()[..]);
        drop(reply);
        assert_eq!(served(rig.get(&t.files[1], &token).await), StatusCode::OK);

        rig.source.broken.store(true, Ordering::SeqCst);
        assert_eq!(
            rig.get(&t.files[1], &token).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let reply = rig.batch(&t.files[1..2], &token).await;
        assert_eq!(&reply.body[..], &[vec![4u8], vec![0u8; 8]].concat()[..]);
        drop(reply);
        rig.source.broken.store(false, Ordering::SeqCst);
        assert_eq!(served(rig.get(&t.files[2], &token).await), StatusCode::OK);
    }
}

/// Catches: the budget not coming back when a request is dropped in the middle of its read (a
/// client that hangs up, a handler that is cancelled): a GET and a batch dropped while the
/// source holds their read are each followed by a read that needs the whole budget and is
/// served.
#[tokio::test]
async fn the_budget_comes_back_when_a_request_is_dropped_mid_read() {
    for batch in [false, true] {
        let mut blobs = HashMap::new();
        let contents: Vec<Vec<u8>> = (0..2).map(|i| pattern(i, 3000)).collect();
        let t = build(&mut blobs, "a", &contents, 0);
        let limits = Limits {
            read_budget_bytes: 4000,
            ..Limits::default()
        };
        let rig = rig(blobs, Some(t.files[0]), limits);
        let token = rig.token(t.action);
        let wanted = [t.files[0]];
        let mut request: std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + '_>> = if batch
        {
            Box::pin(rig.batch(&wanted, &token))
        } else {
            Box::pin(rig.get(&t.files[0], &token))
        };
        loop {
            assert!(futures::poll!(request.as_mut()).is_pending());
            let open = if batch {
                rig.source
                    .many
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len()
                    == 1
            } else {
                rig.source.reads_of(&t.files[0]) == 1
            };
            if open {
                break;
            }
            tokio::task::yield_now().await;
        }
        drop(request);
        assert_eq!(
            rig.get(&t.files[1], &token).await.status,
            StatusCode::OK,
            "batch {batch}"
        );
    }
}

/// Catches: a walk whose first asker fails leaving the others without a scope: three requests
/// for one Action wait on one walk, the store fails it, and the two that waited take it up in
/// their turn and are served; the one whose walk failed is told the store is unavailable.
#[tokio::test]
async fn a_walk_whose_leader_fails_is_taken_up_by_a_waiter() {
    let mut blobs = HashMap::new();
    let t = build(&mut blobs, "a", &[b"alpha".to_vec()], 0);
    let rig = rig(blobs, Some(t.action), Limits::default());
    let token = rig.token(t.action);
    let asks = futures::future::join_all(
        (0..3).map(|_| async { rig.get(&t.files[0], &token).await.status }),
    );
    let driver = async {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(rig.source.reads_of(&t.action), 1, "one walk is open");
        let (_, gate) = rig.source.gate.as_ref().expect("a gate");
        rig.source.broken.store(true, Ordering::SeqCst);
        gate.add_permits(1);
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        rig.source.broken.store(false, Ordering::SeqCst);
        gate.add_permits(1);
    };
    let (mut statuses, ()) = tokio::join!(asks, driver);
    statuses.sort();
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::SERVICE_UNAVAILABLE
        ]
    );
    assert_eq!(rig.source.reads_of(&t.action), 2);
}

/// Catches: Actions whose walk failed staying in the memory of scopes for ever (a farm whose
/// store fails for a minute would grow it past its bound): with a bound of one, an Action that
/// failed and another that came after it do not stop either from being served once the store is
/// back.
#[tokio::test]
async fn a_failed_walk_does_not_stay_in_the_scope_memory() {
    let mut blobs = HashMap::new();
    let a = build(&mut blobs, "a", &[b"alpha".to_vec()], 0);
    let b = build(&mut blobs, "b", &[b"beta".to_vec()], 0);
    let limits = Limits {
        scopes: 1,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    let (ta, tb) = (rig.token(a.action), rig.token(b.action));
    rig.source.broken.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.get(&a.files[0], &ta).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    rig.source.broken.store(false, Ordering::SeqCst);
    assert_eq!(rig.get(&b.files[0], &tb).await.status, StatusCode::OK);
    assert_eq!(rig.get(&a.files[0], &ta).await.status, StatusCode::OK);
}

/// Catches: the memory of scopes growing past its bound when walks fail or are cancelled (each
/// leaves a cell nobody waits on): with a bound of two, six Actions whose walk fails and six
/// whose walk is dropped mid-read leave no more than two cells held, and an Action that is asked
/// for again once the store is back is served.
#[tokio::test]
async fn failed_and_cancelled_walks_do_not_grow_the_scope_memory() {
    let mut blobs = HashMap::new();
    let built: Vec<Built> = (0..12)
        .map(|i| build(&mut blobs, &format!("t{i}"), &[pattern(i, 50)], 0))
        .collect();
    let limits = Limits {
        scopes: 2,
        ..Limits::default()
    };
    let rig = rig(blobs, None, limits);
    rig.source.broken.store(true, Ordering::SeqCst);
    for b in &built[..6] {
        let reply = rig.get(&b.command, &rig.token(b.action)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            rig.read.scopes_held() <= 2,
            "{} held",
            rig.read.scopes_held()
        );
    }
    rig.source.broken.store(false, Ordering::SeqCst);
    for b in &built[6..] {
        // Dropped after its walk has begun: the future is polled until the source has been asked
        // for the Action, then not polled again.
        let token = rig.token(b.action);
        let mut request = Box::pin(rig.get(&b.command, &token));
        while rig.source.reads_of(&b.action) == 0 {
            assert!(futures::poll!(request.as_mut()).is_pending());
            tokio::task::yield_now().await;
        }
        drop(request);
        assert!(
            rig.read.scopes_held() <= 2,
            "{} held",
            rig.read.scopes_held()
        );
    }
    let first = &built[0];
    let reply = rig.get(&first.command, &rig.token(first.action)).await;
    assert_eq!(reply.status, StatusCode::OK);
}
