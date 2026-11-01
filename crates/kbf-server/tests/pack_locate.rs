//! The `kbf.worker.v1.PackIndex` service over a pack index (`kbf_server::pack_locate`), called
//! without a listener: what it answers for blobs the index has and has not, the bound on a call,
//! the digests it refuses, an index that fails, and that a call is admitted before it is
//! answered; and `CommitPack`, which records a pack a daemon stored after checking it against an
//! in-memory object store. The service on a started frontend's worker listener, under mutual TLS
//! and the role table, is `tests/frontend/pack_locate.rs`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{PackSettings, Packs};
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo,
    Recorded,
};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::PackIndex as _;
use kbf_proto::worker::{CommitPackRequest, PackLocateRequest, PackLocation, PackedBlob};
use kbf_server::Peers;
use kbf_server::pack_locate::{PackCommits, PackLocateService, commit_served, service};
use kbf_server::proxy_auth::{Identity, Trust};
use kbf_state::ManualClock;
use kbf_storage::service::PeerRole;
use tonic::{Code, Request};

const PACK: &str = "cas/d/node1/boot1/0000000000000001";

/// The digest of a blob of `size` bytes whose hash is `byte` repeated, in both spellings.
fn digest(byte: u8, size: u64) -> (BlobDigest, Digest) {
    let blob = BlobDigest {
        hash: [byte; 32],
        size,
    };
    let wire = Digest {
        hash: hex::encode(blob.hash),
        size_bytes: i64::try_from(size).expect("a small size"),
    };
    (blob, wire)
}

/// An index that holds one pack of two blobs: `digest(1, 10)` at offset 0 and `digest(2, 20)` at
/// offset 10.
fn index() -> Arc<dyn PackIndex> {
    let index = MemoryPackIndex::new(Arc::new(ManualClock::default()));
    let pack = NewPack {
        key: PACK.to_owned(),
        writer: "node1".to_owned(),
        size: 100,
        blobs: vec![
            PackBlob {
                digest: digest(1, 10).0,
                offset: 0,
            },
            PackBlob {
                digest: digest(2, 20).0,
                offset: 10,
            },
        ],
    };
    index.record_pack(&pack).expect("recorded");
    Arc::new(index)
}

/// A service over `index` that admits no one: it is not asked who calls in `answer`.
fn serving(index: Arc<dyn PackIndex>) -> PackLocateService {
    PackLocateService::new(index, Peers::Unauthenticated)
}

fn located(offset: u64, length: u64) -> PackLocation {
    PackLocation {
        located: true,
        pack: PACK.to_owned(),
        offset,
        length,
    }
}

/// An index whose every call fails.
struct Broken;

impl PackIndex for Broken {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }
}

/// An index whose `locate` panics.
struct Panics;

impl PackIndex for Panics {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        panic!("the index is broken")
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }
}

/// Asserts that each digest is answered in the order asked: a blob of the index with its pack, its
/// offset and its length, one the index does not have as not located (with the other fields
/// empty), and a digest named twice twice.
#[tokio::test]
async fn each_blob_is_answered_where_it_is_in_the_order_asked() {
    let asked = [
        digest(2, 20).1,
        digest(9, 5).1,
        digest(1, 10).1,
        digest(2, 20).1,
    ];
    let answer = serving(index()).answer(&asked).await.expect("answered");
    let none = PackLocation::default();
    assert_eq!(
        answer.locations,
        [located(10, 20), none, located(0, 10), located(10, 20)]
    );
    assert!(!answer.locations[1].located);
    assert!(answer.locations[1].pack.is_empty());
}

/// Asserts that a blob of the same hash and another size is a different blob: the index names a
/// blob by both.
#[tokio::test]
async fn a_blob_is_named_by_its_hash_and_its_size() {
    let asked = [digest(1, 11).1, digest(1, 10).1];
    let answer = serving(index()).answer(&asked).await.expect("answered");
    assert_eq!(answer.locations, [PackLocation::default(), located(0, 10)]);
}

/// Asserts that a call that names no digest is answered with none.
#[tokio::test]
async fn a_call_with_no_digest_is_answered_with_none() {
    let answer = serving(index()).answer(&[]).await.expect("answered");
    assert!(answer.locations.is_empty());
}

/// Asserts the bound on a call: the index's own batch is answered, and one digest more is
/// INVALID_ARGUMENT, saying how many were named and the bound.
#[tokio::test]
async fn a_call_names_at_most_the_indexs_batch() {
    let service = serving(index());
    let (_, wire) = digest(7, 1);
    let batch = vec![wire; MAX_BATCH];
    let answer = service.answer(&batch).await.expect("a full batch");
    assert_eq!(answer.locations.len(), MAX_BATCH);

    let (_, wire) = digest(7, 1);
    let over = vec![wire; MAX_BATCH + 1];
    let refused = service.answer(&over).await.expect_err("one too many");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "1001 digests in one call, at most 1000");
}

/// Asserts that a digest that is not a SHA-256 one is INVALID_ARGUMENT, naming it, whichever way
/// it is wrong: a hash that is not hex, one of another length, a negative size.
#[tokio::test]
async fn a_digest_that_is_not_sha256_is_invalid_argument() {
    let service = serving(index());
    let wrong = |hash: &str, size_bytes: i64| Digest {
        hash: hash.to_owned(),
        size_bytes,
    };
    let not_hex = "zz".repeat(32);
    let short = "ab".repeat(31);
    for bad in [
        wrong(&not_hex, 1),
        wrong(&short, 1),
        wrong("", 1),
        wrong(&"ab".repeat(32), -1),
    ] {
        let named = format!("{}/{} is not a SHA-256 digest", bad.hash, bad.size_bytes);
        let (_, good) = digest(1, 10);
        let refused = service.answer(&[good, bad]).await.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument, "{named}");
        assert_eq!(refused.message(), named);
    }
}

/// Asserts that an index that fails is UNAVAILABLE with a fixed text, which tells a daemon to read
/// as it did before and shows it nothing of the index's own error.
#[tokio::test]
async fn an_index_that_fails_is_unavailable_with_a_fixed_text() {
    let service = serving(Arc::new(Broken));
    let refused = service
        .answer(&[digest(1, 10).1])
        .await
        .expect_err("the index fails");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(refused.message(), "the pack index could not be read");
}

/// Asserts that an index whose call panics is INTERNAL with a fixed text, and that the call
/// returns instead of panicking in the handler.
#[tokio::test]
async fn an_index_that_panics_is_internal_with_a_fixed_text() {
    let service = serving(Arc::new(Panics));
    let refused = service
        .answer(&[digest(1, 10).1])
        .await
        .expect_err("the index panics");
    assert_eq!(refused.code(), Code::Internal);
    assert_eq!(refused.message(), "the pack index could not be read");
}

/// Asserts that a call is admitted before it is answered: with no mutual TLS, or with it and no
/// certificate, a call for a blob the index has is refused UNAUTHENTICATED and nothing is
/// answered, as the listener's blob calls are.
#[tokio::test]
async fn a_call_is_admitted_before_it_is_answered() {
    let certified = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    };
    for peers in [Peers::Unauthenticated, certified] {
        let service = PackLocateService::new(index(), peers);
        let request = PackLocateRequest {
            digests: vec![digest(1, 10).1],
        };
        let refused = service
            .locate(Request::new(request))
            .await
            .expect_err("not admitted");
        assert_eq!(refused.code(), Code::Unauthenticated);
    }
}

/// Asserts that the listener registers the service only when it was given an index: with none,
/// no service is built (a daemon's call is then UNIMPLEMENTED), and with one it is, whether or
/// not the frontend has the pack store to commit packs through.
#[test]
fn the_service_is_built_only_over_an_index() {
    let peers = Peers::Unauthenticated;
    assert!(service(None, &peers, None).is_none());
    assert!(service(Some(&index()), &peers, None).is_some());
    let commits = Some(commits_over(Arc::new(Broken), 2));
    assert!(service(Some(&index()), &peers, commits).is_some());
}

/// Asserts when a frontend tells its daemons it takes `CommitPack`: with the index and the pack
/// store both, and in no other case.
#[test]
fn a_frontend_takes_commits_only_with_the_index_and_the_pack_store() {
    let index = index();
    let commits = commits_over(Arc::new(Broken), 2);

    assert!(commit_served(Some(&index), Some(&commits)));
    assert!(!commit_served(Some(&index), None));
    assert!(!commit_served(None, Some(&commits)));
    assert!(!commit_served(None, None));
}

/// An object store that refuses to replace an object, in memory.
fn store() -> Arc<MemoryStore> {
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    Arc::new(MemoryStore::new(create_only))
}

/// The packs of a frontend over an empty object store and `index`, as `PackCommits` that check
/// `sample` blobs of a pack.
fn commits_over(index: Arc<dyn PackIndex>, sample: usize) -> PackCommits {
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let packs = Packs::new(store(), index, keys, PackSettings::default());
    PackCommits::new(packs.expect("packs"), sample)
}

/// The name of the pack the commit tests store.
const COMMIT_KEY: &str = "cas/d/node-a/boot1/0000000000000000";

/// The bytes of a pack: two blobs, `first blob` and `second blob`, back to back, and filler where
/// a footer would be.
const COMMIT_BODY: &[u8] = b"first blobsecond blobfooter";

/// The commit of `COMMIT_KEY` that names its two blobs, made by `writer`.
fn commit_of(key: &str, writer: &str) -> CommitPackRequest {
    let packed = |text: &[u8], offset: u64| PackedBlob {
        digest: Some(kbf_front::digest_to_proto(&kbf_segments::sha256(text))),
        offset,
    };
    CommitPackRequest {
        pack: key.to_owned(),
        size: COMMIT_BODY.len() as u64,
        blobs: vec![packed(b"first blob", 0), packed(b"second blob", 10)],
        writer: writer.to_owned(),
    }
}

/// The index form of the digest of `text`.
fn blob_of(text: &[u8]) -> BlobDigest {
    let digest = kbf_segments::sha256(text);
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// Peers that admit a proxied identity without a certificate.
fn certified() -> Peers {
    Peers::Certified {
        deny_list: None,
        denied_rows: None,
    }
}

/// `message` as a call that a proxy vouched for as the daemon `node`.
fn from_daemon<T>(message: T, node: &str) -> Request<T> {
    let mut request = Request::new(message);
    let identity = Identity {
        node_id: Some(node.to_owned()),
        role: Some(PeerRole::Daemon),
        ips: Vec::new(),
    };
    request.extensions_mut().insert(Trust::Proxied(identity));
    request
}

/// A frontend that commits packs: its service, its index, and its packs (for their counters),
/// over an object store that holds `objects`.
struct Committing {
    service: PackLocateService,
    index: Arc<MemoryPackIndex>,
    packs: Packs,
}

async fn committing(objects: &[(&str, &[u8])], sample: usize) -> Committing {
    let held = store();
    for (key, body) in objects {
        let key = ObjectKey::new(*key).expect("a key");
        let body = Bytes::copy_from_slice(body);
        held.put_new(&key, body, None).await.expect("stored");
    }
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let shared: Arc<dyn PackIndex> = index.clone();
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let settings = PackSettings::default();
    let made = Packs::new(held, Arc::clone(&shared), keys, settings);
    let packs = made.expect("packs");
    let commits = PackCommits::new(packs.clone(), sample);
    let service = PackLocateService::new(shared, certified());
    let service = service.with_commits(Some(commits));
    Committing {
        service,
        index,
        packs,
    }
}

/// What the frontend answers the daemon `node` for `request`.
async fn committed(
    farm: &Committing,
    request: CommitPackRequest,
    node: &str,
) -> Result<(bool, u32), tonic::Status> {
    let answered = farm.service.commit_pack(from_daemon(request, node)).await;
    let answered = answered?.into_inner();
    Ok((answered.created, answered.indexed))
}

/// Asserts that a daemon's commit of its stored pack is recorded: the answer says the pack was
/// created and indexed two blobs, the index then locates each blob at its offset in the pack, and
/// the same commit sent again is answered `created: false` with the same count.
#[tokio::test]
async fn a_daemon_commits_its_stored_pack_and_a_retry_changes_nothing() {
    let farm = committing(&[(COMMIT_KEY, COMMIT_BODY)], 2).await;

    let first = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;
    assert_eq!(first.expect("committed"), (true, 2));

    let asked = [blob_of(b"first blob"), blob_of(b"second blob")];
    let located = farm.index.locate(&asked).expect("located");
    let places: Vec<_> = located.iter().flatten().collect();
    assert_eq!(places.len(), 2);
    assert!(places.iter().all(|place| place.pack_key == COMMIT_KEY));
    assert_eq!(places[0].offset, 0);
    assert_eq!(places[1].offset, 10);
    let retry = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;
    assert_eq!(retry.expect("retried"), (false, 2));
}

/// Asserts that a sampled blob whose bytes are not its digest is refused INVALID_ARGUMENT, that
/// nothing is recorded, and that the refusal is counted.
#[tokio::test]
async fn a_blob_that_is_not_its_digest_is_refused_and_nothing_is_recorded() {
    let lying: &[u8] = b"firsT blobsecond blobfooter";
    let farm = committing(&[(COMMIT_KEY, lying)], 2).await;

    let refused = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;

    let refused = refused.expect_err("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    let said = refused.message();
    assert!(said.contains("holds other bytes"), "{said}");
    let asked = [blob_of(b"first blob"), blob_of(b"second blob")];
    let located = farm.index.locate(&asked).expect("located");
    assert!(located.iter().all(Option::is_none));
    assert_eq!(farm.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts that a sample of 0 records a pack whose blob bytes are wrong, where a sample of 2
/// refuses it: the sample the service is given is the number of blobs a commit checks.
#[tokio::test]
async fn a_sample_of_zero_records_a_pack_whose_blob_bytes_are_wrong() {
    let lying: &[u8] = b"firsT blobsecond blobfooter";
    let farm = committing(&[(COMMIT_KEY, lying)], 0).await;

    let recorded = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;

    assert_eq!(recorded.expect("recorded unchecked"), (true, 2));
    assert_eq!(farm.packs.metrics().commit_rejects.get(), 0);
}

/// Asserts that a pack that is not in the object store is refused FAILED_PRECONDITION and
/// nothing is recorded.
#[tokio::test]
async fn a_pack_that_is_not_stored_is_refused() {
    let farm = committing(&[], 2).await;

    let refused = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;

    let refused = refused.expect_err("refused");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    let asked = [blob_of(b"first blob")];
    let located = farm.index.locate(&asked).expect("located");
    assert!(located[0].is_none());
}

/// Asserts that a pack named outside `cas/d/<writer>/` is refused INVALID_ARGUMENT though it is
/// stored: the key of another daemon, and the key of a frontend's pack.
#[tokio::test]
async fn a_pack_named_outside_the_writers_prefix_is_refused() {
    let other = "cas/d/node-b/boot1/0000000000000000";
    let frontend = "cas/f/node-a/boot1/0000000000000000";
    let farm = committing(&[(other, COMMIT_BODY), (frontend, COMMIT_BODY)], 2).await;

    for key in [other, frontend] {
        let refused = committed(&farm, commit_of(key, "node-a"), "node-a").await;

        let refused = refused.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument, "{key}");
    }
    assert_eq!(farm.packs.metrics().commit_rejects.get(), 2);
}

/// Asserts that a writer that is not the node the caller's certificate names is refused
/// PERMISSION_DENIED, though the pack is stored and well named: a daemon commits only its own
/// packs. Nothing is recorded.
#[tokio::test]
async fn a_writer_that_is_not_the_caller_is_refused() {
    let farm = committing(&[(COMMIT_KEY, COMMIT_BODY)], 2).await;

    let refused = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-b").await;

    let refused = refused.expect_err("refused");
    assert_eq!(refused.code(), Code::PermissionDenied);
    let asked = [blob_of(b"first blob")];
    let located = farm.index.locate(&asked).expect("located");
    assert!(located[0].is_none());
    assert_eq!(farm.packs.metrics().commit_rejects.get(), 1);
}

/// Asserts that a blob with no digest, and one with a digest that is not SHA-256, refuse the
/// commit INVALID_ARGUMENT and are counted.
#[tokio::test]
async fn a_blob_without_a_usable_digest_refuses_the_commit() {
    let farm = committing(&[(COMMIT_KEY, COMMIT_BODY)], 2).await;
    let mut no_digest = commit_of(COMMIT_KEY, "node-a");
    no_digest.blobs[1].digest = None;
    let mut not_sha = commit_of(COMMIT_KEY, "node-a");
    not_sha.blobs[1].digest = Some(Digest {
        hash: "ab".repeat(31),
        size_bytes: 11,
    });

    for request in [no_digest, not_sha] {
        let refused = committed(&farm, request, "node-a").await;

        let refused = refused.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument);
    }
    assert_eq!(farm.packs.metrics().commit_rejects.get(), 2);
}

/// Asserts that a pack recorded before with other contents is refused ALREADY_EXISTS, and that
/// the first record stands.
#[tokio::test]
async fn a_pack_recorded_with_other_contents_is_refused() {
    let farm = committing(&[(COMMIT_KEY, COMMIT_BODY)], 2).await;
    let first = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;
    assert_eq!(first.expect("committed"), (true, 2));
    let mut other = commit_of(COMMIT_KEY, "node-a");
    other.blobs.truncate(1);

    let refused = committed(&farm, other, "node-a").await;

    let refused = refused.expect_err("refused");
    assert_eq!(refused.code(), Code::AlreadyExists);
    let retry = committed(&farm, commit_of(COMMIT_KEY, "node-a"), "node-a").await;
    assert_eq!(retry.expect("retried"), (false, 2));
}

/// Asserts that an index that fails is UNAVAILABLE with a fixed text for a commit too, which
/// shows a daemon nothing of the index's own error and tells it to send the commit again.
#[tokio::test]
async fn an_index_that_fails_makes_a_commit_unavailable_with_a_fixed_text() {
    let broken: Arc<dyn PackIndex> = Arc::new(Broken);
    let held = store();
    let key = ObjectKey::new(COMMIT_KEY).expect("a key");
    let body = Bytes::from_static(COMMIT_BODY);
    held.put_new(&key, body, None).await.expect("stored");
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let settings = PackSettings::default();
    let packs = Packs::new(held, Arc::clone(&broken), keys, settings);
    let commits = PackCommits::new(packs.expect("packs"), 2);
    let service = PackLocateService::new(broken, certified());
    let service = service.with_commits(Some(commits));

    let request = from_daemon(commit_of(COMMIT_KEY, "node-a"), "node-a");
    let refused = service.commit_pack(request).await;

    let refused = refused.expect_err("the index fails");
    assert_eq!(refused.code(), Code::Unavailable);
    let text = "the pack store or the pack index could not answer";
    assert_eq!(refused.message(), text);
}

/// Asserts that a frontend without the pack store answers a daemon's commit UNIMPLEMENTED, built
/// without commits or with none, after admitting the call.
#[tokio::test]
async fn a_frontend_without_the_pack_store_leaves_commit_unimplemented() {
    let plain = PackLocateService::new(index(), certified());
    let none = PackLocateService::new(index(), certified());
    let none = none.with_commits(None);
    for service in [plain, none] {
        let request = from_daemon(commit_of(COMMIT_KEY, "node-a"), "node-a");

        let refused = service.commit_pack(request).await;

        let refused = refused.expect_err("not served");
        assert_eq!(refused.code(), Code::Unimplemented);
    }
}

/// Asserts that a commit is admitted before it is looked at: with no mutual TLS, or with it and
/// no certificate, it is refused UNAUTHENTICATED, whatever pack it names.
#[tokio::test]
async fn a_commit_is_admitted_before_it_is_answered() {
    for peers in [Peers::Unauthenticated, certified()] {
        let commits = commits_over(Arc::new(Broken), 2);
        let service = PackLocateService::new(index(), peers);
        let service = service.with_commits(Some(commits));
        let request = Request::new(commit_of(COMMIT_KEY, "node-a"));

        let refused = service.commit_pack(request).await;

        let refused = refused.expect_err("not admitted");
        assert_eq!(refused.code(), Code::Unauthenticated);
    }
}
