//! `CreateTree` against the fake: what a create answers, its idempotence, child trees by the
//! node's digest, and each refusal of the node by name.

mod support;

use kbf_trees_client::fake::{Call, Source};
use kbf_trees_client::v1::{PlacementState, Prefetch};
use kbf_trees_client::{Error, Reason, TreeDigest};
use support::{child, create, create_with, fake_and_client, job, span, whole};
use tonic::Code;

/// Asserts a create answers the node's digest of the tree, a placement the node has, the
/// counts, `created`, and a `READY` placement when the node waits for its fills; and that the
/// request the node read is the one sent, with no digest of kbf's own in it.
#[tokio::test]
async fn a_tree_is_created_and_placed_at_the_jobs_location() {
    let (server, client) = fake_and_client().await;
    let entries = vec![
        whole("work/in.txt", 1, 10),
        span("bin/cc", 2, 30, "packs/one-pass/abc.zip", 4096),
    ];
    let request = create(entries, vec![job("k1-2-3-0")]);
    let created = client.create_tree(request.clone()).await.unwrap();
    assert!(created.is_new());
    assert!(server.has_tree(created.tree()));
    assert_eq!(server.placements(), [created.placement().as_str()]);
    let answer = created.answer();
    assert_eq!((answer.entries, answer.files, answer.bytes), (2, 2, 40));
    assert_eq!(answer.state(), PlacementState::Ready);
    assert_eq!(answer.at, request.at);
    assert_eq!(answer.expires_ms, 1_800_000_000_000 + 4 * 3600 * 1000);
    assert_eq!(
        server.calls(),
        [Call::CreateTree(Box::new(request.clone()))]
    );
    assert_eq!(request.tree, None);
}

/// Asserts a create sent again (as after a lost answer) answers the same placement and the
/// same digest with `created` false and its lease renewed, and makes no second placement.
/// Catches a retry that would leave two placements, or one the caller could not release.
#[tokio::test]
async fn a_repeated_create_answers_the_first_placement() {
    let (server, client) = fake_and_client().await;
    let mut request = create(vec![whole("a", 1, 1)], vec![job("j1")]);
    let first = client.create_tree(request.clone()).await.unwrap();
    request.ttl_secs = Some(60);
    let again = client.create_tree(request).await.unwrap();
    assert!(!again.is_new());
    assert_eq!(again.placement(), first.placement());
    assert_eq!(again.tree(), first.tree());
    assert_eq!(again.answer().expires_ms, 1_800_000_000_000 + 60_000);
    assert_eq!(server.placements().len(), 1);
}

/// Asserts another tree at a location already holding one is `location taken`, naming the
/// location; the first placement stays.
#[tokio::test]
async fn another_tree_at_a_taken_location_is_refused_by_name() {
    let (server, client) = fake_and_client().await;
    client
        .create_tree(create(vec![whole("a", 1, 1)], vec![job("j1")]))
        .await
        .unwrap();
    let error = client
        .create_tree(create(vec![whole("b", 1, 1)], vec![job("j1")]))
        .await
        .unwrap_err();
    let refusal = error.refusal().expect("a refusal");
    assert_eq!(
        (refusal.reason(), refusal.about(), refusal.code()),
        (Reason::LocationTaken, Some("jobs/j1"), Code::AlreadyExists)
    );
    assert_eq!(server.placements().len(), 1);
}

/// Asserts a child entry names a tree by the digest the node returned for it, even from a
/// node whose digest builder differs from another's: the same entries have another digest on
/// it, and a parent naming the returned one is placed. Catches a client that computes or
/// sends a digest of its own, which a node of another builder would refuse.
#[tokio::test]
async fn a_node_that_digests_differently_still_gets_its_tree() {
    let (one, client_one) = fake_and_client().await;
    let (other, client_other) = fake_and_client().await;
    other.set_digest_salt(b"a later builder");
    let base = vec![
        whole("usr/bin/tool", 3, 100),
        whole("usr/lib/libc.so", 4, 200),
    ];
    let on_one = client_one
        .create_tree(create(base.clone(), vec![]))
        .await
        .unwrap();
    let on_other = client_other
        .create_tree(create(base, vec![]))
        .await
        .unwrap();
    assert_ne!(on_one.tree(), on_other.tree());
    let parent = |tree| {
        create(
            vec![child("base", tree), whole("work/x", 5, 1)],
            vec![job("j")],
        )
    };
    let placed = client_other
        .create_tree(parent(on_other.tree()))
        .await
        .unwrap();
    assert!(placed.is_new());
    assert_eq!(
        placed.answer().bytes,
        1,
        "the child's bytes are already on the node"
    );
    let unknown = client_other
        .create_tree(parent(on_one.tree()))
        .await
        .unwrap_err();
    assert!(unknown.is(Reason::UnknownChild), "{unknown:?}");
    assert!(one.has_tree(on_one.tree()));
}

/// Asserts a child the node no longer has is `unknown child tree`, naming the entry, a
/// definite refusal; and that once the child is created again (inline), the same parent is
/// placed.
#[tokio::test]
async fn an_unknown_child_is_refused_until_it_is_sent_again() {
    let (server, client) = fake_and_client().await;
    let base = create(vec![whole("tool", 3, 100)], vec![]);
    let child_tree = client.create_tree(base.clone()).await.unwrap().tree();
    assert!(server.forget_tree(child_tree));
    let parent = create(vec![child("base", child_tree)], vec![job("j")]);
    let error = client.create_tree(parent.clone()).await.unwrap_err();
    let refusal = error.refusal().expect("a refusal");
    assert_eq!(
        (refusal.reason(), refusal.about(), refusal.code()),
        (Reason::UnknownChild, Some("base"), Code::FailedPrecondition)
    );
    assert!(!error.may_have_applied());
    assert_eq!(client.create_tree(base).await.unwrap().tree(), child_tree);
    assert!(client.create_tree(parent).await.unwrap().is_new());
}

/// Asserts a request carrying a digest of its own is placed when it is the node's and
/// refused `tree digest mismatch` when it is not.
#[tokio::test]
async fn a_digest_in_the_request_must_be_the_nodes() {
    let (_server, client) = fake_and_client().await;
    let entries = vec![whole("a", 1, 1)];
    let node_digest = client
        .create_tree(create(entries.clone(), vec![]))
        .await
        .unwrap()
        .tree();
    let mut claimed = create(entries.clone(), vec![job("j")]);
    claimed.tree = Some(node_digest.as_bytes().to_vec());
    assert!(client.create_tree(claimed.clone()).await.is_ok());
    claimed.tree = Some(TreeDigest::from_array([9; 32]).as_bytes().to_vec());
    claimed.at = vec![job("j2")];
    let error = client.create_tree(claimed).await.unwrap_err();
    assert!(error.is(Reason::DigestMismatch), "{error:?}");
}

/// Asserts each marked source is refused by its name: a missing object names its key (the
/// span's key, or `cas/l/<sha256>` for a whole object), a short or lying one names the entry;
/// and a cleared mark is no mark.
#[tokio::test]
async fn a_bad_source_is_refused_naming_the_key_or_the_entry() {
    let (server, client) = fake_and_client().await;
    let whole_key = format!("cas/l/{}", "01".repeat(32));
    let cases = [
        (
            "packs/p.zip",
            Source::Missing,
            Reason::SourceMissing,
            "packs/p.zip",
        ),
        ("packs/p.zip", Source::Short, Reason::Short, "bin/cc"),
        ("packs/p.zip", Source::Lying, Reason::Mismatch, "bin/cc"),
        (
            whole_key.as_str(),
            Source::Missing,
            Reason::SourceMissing,
            whole_key.as_str(),
        ),
    ];
    let entries = vec![span("bin/cc", 2, 30, "packs/p.zip", 0), whole("in", 1, 1)];
    for (key, mark, reason, about) in cases {
        server.mark_source(key, Some(mark));
        let error = client
            .create_tree(create(entries.clone(), vec![job("j")]))
            .await
            .unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!((refusal.reason(), refusal.about()), (reason, Some(about)));
        assert_eq!(refusal.code(), reason.code());
        server.mark_source(key, None);
    }
    assert!(
        client
            .create_tree(create(entries, vec![job("j")]))
            .await
            .is_ok()
    );
    assert!(server.placements().len() == 1);
}

/// Asserts a tree over the node's pin limit is `tree too large for the cache` when the
/// create waits for its fills (asked, or left to the node's default), and placed `FILLING`
/// with `background` and `LAZY` with `none`.
#[tokio::test]
async fn a_tree_too_large_for_the_cache_is_refused_under_wait_and_placed_in_background() {
    let (server, client) = fake_and_client().await;
    server.set_pin_limit(10);
    let entries = vec![whole("big", 1, 11)];
    for prefetch in [Prefetch::Wait, Prefetch::Unspecified] {
        let error = client
            .create_tree(create_with(entries.clone(), vec![job("j")], prefetch))
            .await
            .unwrap_err();
        assert!(error.is(Reason::TooLargeForCache), "{error:?}");
        assert_eq!(
            error.refusal().map(|r| r.code()),
            Some(Code::ResourceExhausted)
        );
    }
    let background = create_with(entries.clone(), vec![job("j")], Prefetch::Background);
    let filling = client.create_tree(background).await.unwrap();
    assert_eq!(filling.answer().state(), PlacementState::Filling);
    assert_eq!(filling.answer().filled, 0);
    let none = create_with(entries, vec![job("k")], Prefetch::None);
    let lazy = client.create_tree(none).await.unwrap();
    assert_eq!(lazy.answer().state(), PlacementState::Lazy);
}

/// Asserts a request the node will not read is refused by name under `INVALID_ARGUMENT`,
/// naming the entry, and one with an entry of no kind is a definite status that names no
/// refusal; none is a lost answer.
#[tokio::test]
async fn a_malformed_request_is_refused_by_name() {
    let (server, client) = fake_and_client().await;
    let mut bad_digest = whole("a", 1, 1);
    if let Some(kbf_trees_client::v1::entry::Kind::File(file)) = &mut bad_digest.kind {
        file.sha256 = vec![1; 31];
    }
    let mut bad_mode = whole("m", 1, 1);
    if let Some(kbf_trees_client::v1::entry::Kind::File(file)) = &mut bad_mode.kind {
        file.mode = Some(0o17777);
    }
    let cases = [
        (
            vec![whole("a", 1, 1), whole("a", 2, 1)],
            Reason::DuplicatePath,
            "a",
        ),
        (vec![bad_digest], Reason::MalformedDigest, "a"),
        (vec![bad_mode], Reason::BadMode, "m"),
        (
            vec![whole("x/../y", 1, 1)],
            Reason::UnsupportedEntryName,
            "x/../y",
        ),
    ];
    for (entries, reason, about) in cases {
        let error = client
            .create_tree(create(entries, vec![]))
            .await
            .unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!((refusal.reason(), refusal.about()), (reason, Some(about)));
        assert_eq!(refusal.code(), Code::InvalidArgument);
    }
    let kindless = kbf_trees_client::v1::Entry {
        path: "x".to_owned(),
        kind: None,
    };
    let error = client
        .create_tree(create(vec![kindless], vec![]))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Status { code: Code::InvalidArgument, message } if message == "entry x has no kind"),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert!(server.placements().is_empty());
}
