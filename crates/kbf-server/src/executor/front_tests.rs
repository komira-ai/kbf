use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::{
    Backend, Cache, CacheStorage, MemoryMetaLog, PackKeyMint, PackSettings, Packs, SegmentsConfig,
    VerifiedBlob,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_pack::{
    OutputDirectory, PackKeys, Slot, TaskManifest, Timing, Usage, WriterKind, write_pack,
};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{self, Directory, FileNode, Platform, Tree};
use kbf_state::ManualClock;
use kbf_types::DigestFunction;

use super::*;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const OTHER_IMAGE: &str = "docker://registry.test/other@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const NOW: u64 = 1_790_000_040;

thread_local! {
    static CLOCK: Cell<u64> = const { Cell::new(NOW) };
}

fn clock() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(CLOCK.with(Cell::get))
}

fn lease() -> LeaseId {
    LeaseId::new(7, 3)
}

fn wire(digest: &kbf_types::Digest) -> Digest {
    kbf_front::digest_to_proto(digest)
}

/// Answers what it is told and remembers what it was asked.
struct Prepared {
    answer: Mutex<Result<PreparedRun, PrepareError>>,
    asked: Mutex<Vec<(String, kbf_types::Digest, SystemTime, Duration)>>,
}

impl Prepared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Ok(run("cas/b/7-3/aa-0", NOW + 7_200))),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn answers(&self, answer: Result<PreparedRun, PrepareError>) {
        *self.answer.lock().unwrap() = answer;
    }
}

#[tonic::async_trait]
impl RunPrepare for Prepared {
    async fn prepare_run(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedRun, PrepareError> {
        self.asked
            .lock()
            .unwrap()
            .push((lease.to_owned(), *action, now, wall));
        self.answer.lock().unwrap().clone()
    }
}

fn run(key: &str, not_after: u64) -> PreparedRun {
    PreparedRun {
        key: key.to_owned(),
        put_url: Secret::new(format!("http://store.test/{key}?sig=put")),
        grant_url: "https://grant.test:9400".to_owned(),
        grant_token: Secret::new("grant-token"),
        not_after,
        input_bytes: Some(12_345),
    }
}

/// An object store whose reads can be made to fail.
struct Flaky {
    inner: MemoryStore,
    fail_reads: AtomicBool,
}

impl ObjectStore for Flaky {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Unsupported("reads, for this test"));
        }
        self.inner.get_range(key, range).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Unsupported("reads, for this test"));
        }
        self.inner.len(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        page: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, page).await
    }
}

/// The seams over the frontend's own types: packs over an in-memory object store and index, a
/// CAS over a node's cache, and the real key mint.
struct Rig {
    front: Front,
    prepared: Arc<Prepared>,
    store: Arc<Flaky>,
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
    mint: PackKeyMint,
}

impl Rig {
    fn new() -> Self {
        CLOCK.with(|c| c.set(NOW));
        let store = Arc::new(Flaky {
            inner: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            fail_reads: AtomicBool::new(false),
        });
        let index: Arc<dyn PackIndex> =
            Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let packs =
            Packs::new(Arc::clone(&store), index, keys, PackSettings::default()).expect("packs");
        let cache = Arc::new(Cache::memory());
        let node = Arc::new(CacheStorage::new(
            Arc::clone(&cache),
            SegmentsConfig::DEFAULT,
        ));
        let backend = Backend::remote_over(node, Some(packs.clone()));
        let prepared = Prepared::new();
        let front = Front::with_clock(
            FrontParts {
                packs,
                backend,
                mint: PackKeyMint::new(SECRET).expect("a mint"),
                prepare: prepared.clone(),
                base_image: ImageRef::parse(IMAGE).expect("an image"),
                kbf_image: ImageRef::parse(&format!("docker://{KBF_IMAGE}")).expect("an image"),
            },
            clock,
        );
        Self {
            front,
            prepared,
            store,
            cache,
            mint: PackKeyMint::new(SECRET).expect("a mint"),
        }
    }

    /// Stores `message` in the CAS and returns its digest.
    async fn store(&self, message: &impl prost::Message) -> kbf_types::Digest {
        let blob = VerifiedBlob::hashed(Bytes::from(message.encode_to_vec()));
        let digest = blob.digest();
        self.cache.store_blobs(vec![blob]).await.expect("stored");
        digest
    }

    /// An Action that names a stored Command; `action` shapes the Action and `command` the
    /// Command.
    async fn action(
        &self,
        action: impl FnOnce(&mut Action),
        command: impl FnOnce(&mut Command),
    ) -> Digest {
        let mut cmd = Command::default();
        command(&mut cmd);
        let command = self.store(&cmd).await;
        let mut act = Action {
            command_digest: Some(wire(&command)),
            ..Action::default()
        };
        action(&mut act);
        wire(&self.store(&act).await)
    }
}

fn platform(image: &str) -> Platform {
    Platform {
        properties: vec![Property {
            name: "container-image".to_owned(),
            value: image.to_owned(),
        }],
    }
}

fn seconds(n: i64) -> prost_types::Duration {
    prost_types::Duration {
        seconds: n,
        nanos: 0,
    }
}

fn action_digest() -> Digest {
    wire(&kbf_types::Digest::new(DigestFunction::Sha256, [9; 32], 5))
}

/// Catches a run whose key, write and grant are not the ones the frontend prepared for its
/// lease: the preparation is asked once with the farm lease's build id, the action, the time and
/// the run's wall, and each of the three is handed out once.
#[tokio::test]
async fn a_run_is_prepared_once_and_its_key_write_and_grant_are_handed_out_once() {
    let rig = Rig::new();
    let wall = Duration::from_secs(900);
    let key = rig
        .front
        .mint(lease(), &action_digest(), wall)
        .await
        .expect("a key");
    assert_eq!(key.as_str(), "cas/b/7-3/aa-0");
    let asked = rig.prepared.asked.lock().unwrap().clone();
    let action = kbf_types::Digest::new(DigestFunction::Sha256, [9; 32], 5);
    assert_eq!(asked, [("7-3".to_owned(), action, clock(), wall)]);

    let url = rig
        .front
        .put_url("node1", &key, wall, 1 << 20)
        .await
        .expect("the write");
    assert_eq!(url.expose(), "http://store.test/cas/b/7-3/aa-0?sig=put");
    let again = rig.front.put_url("node1", &key, wall, 1 << 20).await;
    assert!(matches!(again, Err(PresignError::Refused(_))));

    let grant = rig
        .front
        .read_grant(lease(), &action_digest(), wall)
        .await
        .expect("the grant");
    assert_eq!(grant.url, "https://grant.test:9400");
    assert_eq!(grant.token.expose(), "grant-token");
    assert_eq!(
        grant.input_bytes,
        Some(12_345),
        "the closure size is the grant's"
    );
    let again = rig.front.read_grant(lease(), &action_digest(), wall).await;
    assert!(matches!(again, Err(InputsError::Unavailable(_))));
    assert!(rig.front.parked().is_empty(), "nothing is kept once used");
}

/// Catches a write url handed to a run it was not prepared for: the url is found by the key it
/// signs, and a key nobody prepared gets none.
#[tokio::test]
async fn a_write_is_found_by_its_key_and_a_stranger_gets_none() {
    let rig = Rig::new();
    let wall = Duration::from_secs(60);
    rig.front
        .mint(lease(), &action_digest(), wall)
        .await
        .expect("a key");
    let other = PackKey::new("cas/b/7-4/bb-0");
    let refused = rig.front.put_url("node1", &other, wall, 1).await;
    assert!(matches!(refused, Err(PresignError::Refused(_))));
    let grant = rig
        .front
        .read_grant(LeaseId::new(7, 4), &action_digest(), wall)
        .await;
    assert!(matches!(grant, Err(InputsError::Unavailable(_))));
    assert_eq!(rig.front.parked().len(), 1, "the prepared run is kept");
}

/// Catches a write and a grant that stop working before the run's wall time does: the run
/// would start and be refused its inputs or its upload, and the farm would run it again.
#[tokio::test]
async fn a_write_that_ends_before_the_run_does_is_not_a_key() {
    let rig = Rig::new();
    rig.prepared.answers(Ok(run("cas/b/7-3/aa-0", NOW + 899)));
    let refused = rig
        .front
        .mint(lease(), &action_digest(), Duration::from_secs(900))
        .await;
    assert!(refused.is_err());
    assert!(rig.front.parked().is_empty());
    rig.prepared.answers(Ok(run("cas/b/7-3/aa-0", NOW + 900)));
    let kept = rig
        .front
        .mint(lease(), &action_digest(), Duration::from_secs(900))
        .await;
    assert!(kept.is_ok(), "a write good for exactly the wall is enough");
}

/// Catches a preparation that fails being taken for a key, and its reason being lost.
#[tokio::test]
async fn a_run_that_cannot_be_prepared_is_not_a_key_and_says_why() {
    let rig = Rig::new();
    for why in [
        PrepareError::Refused("an input is not held".to_owned()),
        PrepareError::Unavailable("the store is down".to_owned()),
    ] {
        rig.prepared.answers(Err(why.clone()));
        let refused = rig
            .front
            .mint(lease(), &action_digest(), Duration::from_secs(60))
            .await;
        let expected = match &why {
            PrepareError::Refused(why) => MintError::Refused(why.clone()),
            PrepareError::Unavailable(why) => MintError::Unavailable(why.clone()),
        };
        assert_eq!(refused.err(), Some(expected));
    }
    let not_a_digest = reapi::Digest {
        hash: "xyz".to_owned(),
        size_bytes: 1,
    };
    let refused = rig
        .front
        .mint(lease(), &not_a_digest, Duration::from_secs(60))
        .await;
    assert!(refused.is_err(), "an action digest that is not one");
    assert_eq!(
        rig.prepared.asked.lock().unwrap().len(),
        2,
        "and it is not prepared"
    );
}

/// Catches runs that were prepared and never started (a `Start` that ran out of its window)
/// being kept for ever: what a later mint finds past its expiry is dropped.
#[tokio::test]
async fn what_a_start_that_ran_out_of_its_window_left_is_dropped_at_the_next_mint() {
    let rig = Rig::new();
    rig.prepared.answers(Ok(run("cas/b/7-3/aa-0", NOW + 100)));
    rig.front
        .mint(lease(), &action_digest(), Duration::from_secs(10))
        .await
        .expect("a key");
    assert_eq!(rig.front.parked().len(), 1);
    CLOCK.with(|c| c.set(NOW + 101));
    rig.prepared
        .answers(Ok(run("cas/b/7-9/bb-0", NOW + 100_000)));
    rig.front
        .mint(
            LeaseId::new(7, 9),
            &action_digest(),
            Duration::from_secs(10),
        )
        .await
        .expect("another key");
    let kept: Vec<_> = rig.front.parked().keys().copied().collect();
    assert_eq!(kept, [LeaseId::new(7, 9)]);
}

/// Catches a farm lease whose run is prepared twice (the Start came again) keeping two sets of
/// secrets: the second preparation replaces the first.
#[tokio::test]
async fn preparing_a_lease_again_replaces_what_was_parked() {
    let rig = Rig::new();
    let wall = Duration::from_secs(60);
    rig.front
        .mint(lease(), &action_digest(), wall)
        .await
        .expect("a key");
    rig.prepared.answers(Ok(run("cas/b/7-3/cc-0", NOW + 7_200)));
    let key = rig
        .front
        .mint(lease(), &action_digest(), wall)
        .await
        .expect("a key");
    assert_eq!(rig.front.parked().len(), 1);
    assert!(rig.front.put_url("node1", &key, wall, 1).await.is_ok());
}

/// Catches an action's run being launched from anything but the kbf image, and a timeout that is
/// not the action's: an action that names the served base runs from the kbf image, by digest.
#[tokio::test]
async fn an_action_runs_from_the_kbf_image_by_digest_and_has_its_timeout() {
    let rig = Rig::new();
    let digest = rig
        .action(
            |action| {
                action.platform = Some(platform(IMAGE));
                action.timeout = Some(seconds(90));
            },
            |_| {},
        )
        .await;
    let facts = rig.front.action(&digest).await.expect("facts");
    assert_eq!(
        facts,
        ActionFacts {
            image: KBF_IMAGE.to_owned(),
            timeout: Some(Duration::from_secs(90)),
        }
    );
}

/// Catches a client older than REAPI 2.2, whose Command (not its Action) holds the platform, and
/// an Action that names no timeout or a zero one, which the executor gives its own.
#[tokio::test]
#[allow(deprecated)]
async fn the_commands_platform_is_read_and_no_timeout_is_none() {
    let rig = Rig::new();
    let old = rig
        .action(|_| {}, |command| command.platform = Some(platform(IMAGE)))
        .await;
    assert_eq!(rig.front.action(&old).await.expect("facts").timeout, None);
    let zero = rig
        .action(
            |action| action.timeout = Some(seconds(0)),
            |command| command.platform = Some(platform(IMAGE)),
        )
        .await;
    assert_eq!(rig.front.action(&zero).await.expect("facts").timeout, None);
}

/// Catches actions the executor must not run being passed on: an image named by a tag and a
/// timeout below zero are the caller's to fix (an action that names no image is run: see
/// `an_action_that_names_no_image_is_run_from_the_kbf_image`).
#[tokio::test]
async fn an_image_named_by_tag_or_a_negative_timeout_is_invalid() {
    let rig = Rig::new();
    let tagged = rig
        .action(
            |action| action.platform = Some(platform("docker://registry.test/app:latest")),
            |_| {},
        )
        .await;
    let negative = rig
        .action(
            |action| {
                action.platform = Some(platform(IMAGE));
                action.timeout = Some(seconds(-1));
            },
            |_| {},
        )
        .await;
    for digest in [tagged, negative] {
        let refused = rig.front.action(&digest).await;
        assert!(
            matches!(refused, Err(InputsError::Invalid(_))),
            "{refused:?}"
        );
    }
}

/// Catches a refusal of the real preparation being sorted by its text or its kind and not by whose
/// it is to fix: only a store that could not be read is the farm's to retry; a blob that is
/// missing, a tree over budget and a lease no key can carry are the caller's, and neither says
/// more than the preparation did.
#[test]
fn the_real_preparations_refusals_are_sorted_by_whose_they_are() {
    use kbf_front::prepare_run::ClosureError;
    let digest = kbf_types::Digest::new(DigestFunction::Sha256, [9; 32], 5);
    let unavailable = RunPrepError::Closure(ClosureError::Unavailable {
        digest,
        why: "the store is down".to_owned(),
    });
    let text = unavailable.to_string();
    assert_eq!(
        prepare_error(unavailable),
        PrepareError::Unavailable(text.clone())
    );
    assert!(text.contains("the store is down"), "{text}");
    for refused in [
        RunPrepError::Closure(ClosureError::Missing {
            what: "a file",
            digest,
        }),
        RunPrepError::Closure(ClosureError::TooManyDigests { limit: 3 }),
        RunPrepError::Clock,
    ] {
        let text = refused.to_string();
        assert_eq!(prepare_error(refused), PrepareError::Refused(text));
    }
}

/// Catches the base environment being served for an image it is not: an action whose
/// `container-image` is another digest than the served base is the caller's to fix, and the
/// refusal names both images and the kbf image the run would have used; a tag is refused too,
/// and so is a value that is not an image. An action that names the base, or none, is run.
#[tokio::test]
async fn an_action_naming_another_image_than_the_served_base_is_refused() {
    let rig = Rig::new();
    let other = rig
        .action(
            |action| action.platform = Some(platform(OTHER_IMAGE)),
            |_| {},
        )
        .await;
    let refused = rig.front.action(&other).await;
    let Err(InputsError::Invalid(why)) = refused else {
        panic!("not refused: {refused:?}");
    };
    for named in [
        OTHER_IMAGE.trim_start_matches("docker://"),
        IMAGE.trim_start_matches("docker://"),
        KBF_IMAGE,
    ] {
        assert!(why.contains(named), "{why} does not name {named}");
    }
    for value in ["docker://registry.test/app:latest", "not an image"] {
        let bad = rig
            .action(|action| action.platform = Some(platform(value)), |_| {})
            .await;
        let refused = rig.front.action(&bad).await;
        assert!(
            matches!(refused, Err(InputsError::Invalid(_))),
            "{refused:?}"
        );
    }
    // The same digest under another repository name is the same image.
    let renamed = format!(
        "docker://mirror.test/app@{}",
        ImageRef::parse(IMAGE).expect("an image").digest()
    );
    let alias = rig
        .action(|action| action.platform = Some(platform(&renamed)), |_| {})
        .await;
    assert!(rig.front.action(&alias).await.is_ok());
}

/// Catches an action that names no image being refused or run elsewhere: it gets the base, so it
/// runs from the kbf image like one that names the base.
#[tokio::test]
async fn an_action_that_names_no_image_is_run_from_the_kbf_image() {
    let rig = Rig::new();
    let bare = rig.action(|_| {}, |_| {}).await;
    let facts = rig.front.action(&bare).await.expect("runs");
    assert_eq!(facts.image, KBF_IMAGE);
}

/// Catches blobs that cannot be had being taken for a bad action: a CAS that does not hold the
/// Action, or the Command, is unavailable (the farm accepted the action, so its blobs were
/// there), and bytes that are not an Action or name a bad Command are the caller's.
#[tokio::test]
async fn an_action_the_cas_cannot_give_is_unavailable_and_one_that_is_malformed_is_invalid() {
    let rig = Rig::new();
    let unheld = rig.front.action(&action_digest()).await;
    assert!(
        matches!(unheld, Err(InputsError::Unavailable(_))),
        "{unheld:?}"
    );

    let junk = VerifiedBlob::hashed(Bytes::from_static(b"\xff\xff not an action"));
    let digest = junk.digest();
    rig.cache.store_blobs(vec![junk]).await.expect("stored");
    let junk = rig.front.action(&wire(&digest)).await;
    assert!(matches!(junk, Err(InputsError::Invalid(_))), "{junk:?}");

    let no_command = rig.store(&Action::default()).await;
    let none = rig.front.action(&wire(&no_command)).await;
    assert!(matches!(none, Err(InputsError::Invalid(_))), "{none:?}");

    let bad = Action {
        command_digest: Some(Digest {
            hash: "short".to_owned(),
            size_bytes: 1,
        }),
        ..Action::default()
    };
    let bad = rig.store(&bad).await;
    let refused = rig.front.action(&wire(&bad)).await;
    assert!(
        matches!(refused, Err(InputsError::Invalid(_))),
        "{refused:?}"
    );

    let absent = Action {
        command_digest: Some(action_digest()),
        ..Action::default()
    };
    let absent = rig.store(&absent).await;
    let unheld = rig.front.action(&wire(&absent)).await;
    assert!(
        matches!(unheld, Err(InputsError::Unavailable(_))),
        "{unheld:?}"
    );

    let negative = Digest {
        hash: "a".repeat(64),
        size_bytes: -1,
    };
    let refused = rig.front.action(&negative).await;
    assert!(
        matches!(refused, Err(InputsError::Invalid(_))),
        "{refused:?}"
    );
}

/// The key the real mint issues for the build, the action and the time.
fn issued(rig: &Rig, build: &str, action: &Digest) -> PackKey {
    let action = super::cas_digest(action).expect("a digest");
    let key = rig
        .mint
        .mint(build, &action, clock(), Slot::Pack(0))
        .expect("a key");
    PackKey::new(key.key())
}

fn limits() -> UntrustedLimits {
    UntrustedLimits {
        max_pack_bytes: 1 << 20,
        max_blob_bytes: 1 << 20,
    }
}

/// Catches an ingest that is not the frontend's: a key the real mint issued for this lease and
/// action, whose pack is not in the store, is refused as missing; a key of another lease and
/// text that is no key are not the issued one.
#[tokio::test]
async fn a_pack_that_is_not_there_or_a_key_that_is_not_the_issued_one_is_refused() {
    let rig = Rig::new();
    let action = rig.action(|_| {}, |_| {}).await;
    let key = issued(&rig, "7-3", &action);
    let missing = rig.front.ingest(lease(), &key, &action, &limits()).await;
    assert!(
        matches!(missing, Ingested::Reject(RejectReason::Missing)),
        "{missing:?}"
    );

    let theirs = issued(&rig, "7-4", &action);
    let stranger = rig.front.ingest(lease(), &theirs, &action, &limits()).await;
    assert!(
        matches!(stranger, Ingested::Reject(RejectReason::WrongKey)),
        "{stranger:?}"
    );

    for text in [
        "cas/b/7-3/made-up-0",
        "cas/b/7-3",
        "cas/x/7-3/aa-0",
        "cas/b//aa-0",
    ] {
        let key = PackKey::new(text);
        let refused = rig.front.ingest(lease(), &key, &action, &limits()).await;
        assert!(
            matches!(refused, Ingested::Reject(RejectReason::WrongKey)),
            "{text}: {refused:?}"
        );
    }
}

/// Catches an object store that cannot be read being taken for a pack that is bad: the pack
/// may be there, so the answer is to ask again and not to refuse it.
#[tokio::test]
async fn a_store_that_cannot_be_read_is_unavailable_and_not_a_refusal() {
    let rig = Rig::new();
    let action = rig.action(|_| {}, |_| {}).await;
    let key = issued(&rig, "7-3", &action);
    rig.store.fail_reads.store(true, Ordering::SeqCst);
    let down = rig.front.ingest(lease(), &key, &action, &limits()).await;
    assert!(matches!(down, Ingested::Unavailable(_)), "{down:?}");

    let bad = reapi::Digest {
        hash: "x".to_owned(),
        size_bytes: 1,
    };
    let down = rig.front.ingest(lease(), &key, &bad, &limits()).await;
    assert!(matches!(down, Ingested::Unavailable(_)), "{down:?}");
}

/// Catches an ingest through the seam that does not give the executor the result the frontend
/// rebuilt: a pack the task wrote under the issued key is read, checked and turned into the
/// action's result, with its exit code, and the frontend caches it when the action exited 0 and
/// not when it failed.
#[tokio::test]
async fn a_pack_under_the_issued_key_becomes_the_actions_result() {
    let rig = Rig::new();
    for (exit_code, cached) in [(0, true), (3, false)] {
        let build = format!("7-{}", 10 + exit_code);
        let lease = LeaseId::new(7, 10 + u64::try_from(exit_code).expect("small"));
        let action = rig
            .action(
                |a| a.timeout = Some(seconds(i64::from(exit_code) + 1)),
                |_| {},
            )
            .await;
        let key = issued(&rig, &build, &action);
        let manifest = TaskManifest {
            lease: build,
            action: super::cas_digest(&action).expect("a digest"),
            exit_code,
            stdout: None,
            stderr: None,
            output_files: Vec::new(),
            output_directories: Vec::new(),
            output_symlinks: Vec::new(),
            timing: Timing::default(),
            usage: Usage::default(),
            packs: 1,
            large: vec![],
            inputs: None,
        };
        let pack = write_pack(&manifest, &[]).expect("a pack");
        let object = ObjectKey::new(key.as_str().to_owned()).expect("an object key");
        rig.store
            .put_new(&object, Bytes::from(pack), None)
            .await
            .expect("written");
        let done = rig.front.ingest(lease, &key, &action, &limits()).await;
        let Ingested::Ran {
            result,
            cached: kept,
        } = done
        else {
            panic!("not ingested: {done:?}");
        };
        assert_eq!(result.exit_code, exit_code);
        assert_eq!(kept, cached, "exit code {exit_code}");
    }
}

/// Catches a reason of the frontend's that the executor reads as another.
#[test]
fn every_reason_of_the_frontend_is_the_executors_own() {
    use kbf_front::RejectReason as Front;
    let pairs = [
        (Front::Missing, RejectReason::Missing),
        (Front::WrongKey, RejectReason::WrongKey),
        (Front::TooLarge, RejectReason::TooLarge),
        (Front::Malformed, RejectReason::Malformed),
        (Front::HashMismatch, RejectReason::HashMismatch),
        (Front::BadManifest, RejectReason::BadManifest),
        (Front::Incomplete, RejectReason::Incomplete),
        (Front::Expired, RejectReason::Expired),
        (Front::Claimed, RejectReason::Claimed),
    ];
    assert_eq!(pairs.len(), Front::ALL.len());
    for (front, ours) in pairs {
        assert_eq!(super::reason(front), ours);
        let spelled = format!("{ours:?}").to_lowercase();
        assert_eq!(front.label().replace('_', ""), spelled);
    }
}

/// A manifest of an action that exited 0 with nothing to show, for the build `build`.
fn manifest_of(build: &str, action: &Digest) -> TaskManifest {
    TaskManifest {
        lease: build.to_owned(),
        action: super::cas_digest(action).expect("a digest"),
        exit_code: 0,
        stdout: None,
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing::default(),
        usage: Usage::default(),
        packs: 1,
        large: vec![],
        inputs: None,
    }
}

/// Writes `pack` to the object store under `key`, as the task's `PUT` does.
async fn put_pack(rig: &Rig, key: &PackKey, pack: Vec<u8>) {
    let object = ObjectKey::new(key.as_str().to_owned()).expect("an object key");
    rig.store
        .put_new(&object, Bytes::from(pack), None)
        .await
        .expect("written");
}

/// Catches the executor's own caps not reaching the ingest (the ingest's defaults, 128 MiB and
/// 64 MiB, would apply instead): a pack over the executor's `max_pack_bytes`, and one whose blob
/// is over its `max_blob_bytes`, are refused as too large, and the same packs are taken under
/// roomier caps, so the cap and nothing else is the reason.
#[tokio::test]
async fn a_pack_over_the_executors_caps_is_too_large() {
    let rig = Rig::new();
    let action = rig.action(|_| {}, |_| {}).await;

    let small = write_pack(&manifest_of("7-3", &action), &[]).expect("a pack");
    let key = issued(&rig, "7-3", &action);
    put_pack(&rig, &key, small.clone()).await;
    let capped = UntrustedLimits {
        max_pack_bytes: small.len() as u64 - 1,
        max_blob_bytes: 1 << 20,
    };
    let refused = rig.front.ingest(lease(), &key, &action, &capped).await;
    assert!(
        matches!(refused, Ingested::Reject(RejectReason::TooLarge)),
        "{refused:?}"
    );
    let taken = rig.front.ingest(lease(), &key, &action, &limits()).await;
    assert!(matches!(taken, Ingested::Ran { .. }), "{taken:?}");

    let big = vec![b'b'; 4_000];
    let mut manifest = manifest_of("7-4", &action);
    manifest.stdout = Some(VerifiedBlob::hashed(Bytes::from(big.clone())).digest());
    let heavy = write_pack(&manifest, &[&big]).expect("a pack");
    let key = issued(&rig, "7-4", &action);
    put_pack(&rig, &key, heavy).await;
    let capped = UntrustedLimits {
        max_pack_bytes: 1 << 20,
        max_blob_bytes: 3_999,
    };
    let other = LeaseId::new(7, 4);
    let refused = rig.front.ingest(other, &key, &action, &capped).await;
    assert!(
        matches!(refused, Ingested::Reject(RejectReason::TooLarge)),
        "{refused:?}"
    );
    let taken = rig.front.ingest(other, &key, &action, &limits()).await;
    assert!(matches!(taken, Ingested::Ran { .. }), "{taken:?}");
}

/// Catches the Command's platform winning over the Action's: when both name an image the
/// Action's is the one (the frontend's own execution reads them in that order), so a client that
/// sets both is run where the farm would run it.
#[tokio::test]
#[allow(deprecated)]
async fn the_actions_platform_wins_over_the_commands() {
    let rig = Rig::new();
    let digest = rig
        .action(
            |action| action.platform = Some(platform(IMAGE)),
            |command| command.platform = Some(platform(OTHER_IMAGE)),
        )
        .await;
    let facts = rig.front.action(&digest).await.expect("facts");
    assert_eq!(facts.image, KBF_IMAGE);
    // Were the Command's platform the one read, the other image would be refused.
}

/// Catches the ingest handed a backend that cannot read the packs (a tree that only the pack
/// holds is then unavailable, and every task with an output directory is retried for ever): the
/// pack of a task whose output directory's tree and file are in the pack alone is ingested into
/// a result, and cached.
#[tokio::test]
async fn a_tree_that_only_the_pack_holds_is_read_through_the_same_packs() {
    let rig = Rig::new();
    let action = rig.action(|_| {}, |_| {}).await;
    let file = b"a file inside the output directory".to_vec();
    let file_digest = VerifiedBlob::hashed(Bytes::from(file.clone())).digest();
    let tree = Tree {
        root: Some(Directory {
            files: vec![FileNode {
                name: "f".to_owned(),
                digest: Some(wire(&file_digest)),
                ..FileNode::default()
            }],
            ..Directory::default()
        }),
        ..Tree::default()
    };
    let tree = prost::Message::encode_to_vec(&tree);
    let mut manifest = manifest_of("7-3", &action);
    manifest.output_directories = vec![OutputDirectory {
        path: "out".to_owned(),
        tree: VerifiedBlob::hashed(Bytes::from(tree.clone())).digest(),
    }];
    let pack = write_pack(&manifest, &[&tree, &file]).expect("a pack");
    let key = issued(&rig, "7-3", &action);
    put_pack(&rig, &key, pack).await;
    let done = rig.front.ingest(lease(), &key, &action, &limits()).await;
    let Ingested::Ran { result, cached } = done else {
        panic!("not ingested: {done:?}");
    };
    assert_eq!(result.output_directories.len(), 1);
    assert!(cached, "the result was not cached");
}
