//! One pack key ingested twice, by two frontends over the same stores: what a dead or stalled
//! frontend and the adopter that takes its job leave behind (`docs/design/n-frontends.md`, 4.4).

use kbf_pack::OutputFile;
use kbf_pack_index::{Location, PackInfo};

use super::*;

/// Which of the two frontends ingests first.
#[derive(Clone, Copy, Debug)]
enum Order {
    AThenB,
    BThenA,
    Together,
}

/// How the run's pack was issued.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// A put directory (`--task-out-mode put-dir`, the mode shared ownership needs).
    PutDir,
    /// One presigned key.
    Presigned,
}

/// The stores two frontends share: the packs' object store, the pack index, and the CAS and
/// action cache of the node.
struct Plane {
    store: Arc<MemoryStore>,
    index: Arc<MemoryPackIndex>,
    cache: Arc<Cache<MemoryMetaLog, MemoryStore>>,
}

/// What the stores hold of one run: where its output blob is, its pack's row, and the action's
/// cache entry.
#[derive(Debug, PartialEq)]
struct Held {
    located: Option<Location>,
    pack: Option<PackInfo>,
    entry: Option<(Digest, reapi::ActionResult)>,
}

const OUTPUT: &[u8] = b"the bytes the action wrote to out";

impl Plane {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            })),
            index: Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default()))),
            cache: Arc::new(Cache::memory()),
        }
    }

    /// A frontend named `name` over these stores: its own packs and backend, the same index,
    /// object store, CAS and mint secret, as a second process of the group would hold.
    fn frontend(&self, name: &str, mode: Mode) -> Front {
        let index: Arc<dyn PackIndex> = self.index.clone();
        let keys = PackKeys::new(WriterKind::Frontend, name, "boot1").expect("keys");
        let at = UNIX_EPOCH + Duration::from_secs(NOW);
        let packs = Packs::new(
            Arc::clone(&self.store),
            index,
            keys,
            PackSettings::default(),
        )
        .expect("packs")
        .with_clock(Arc::new(move || at));
        let node = Arc::new(CacheStorage::new(
            Arc::clone(&self.cache),
            SegmentsConfig::DEFAULT,
        ));
        let backend = Backend::remote_over(node, Some(packs.clone()));
        let put_dir = match mode {
            Mode::PutDir => Some(PutDirParts::new(
                &packs,
                &backend,
                Arc::new(OnceLock::new()),
            )),
            Mode::Presigned => None,
        };
        Front::with_clock(
            FrontParts {
                packs,
                backend,
                mint: PackKeyMint::new(SECRET).expect("a mint"),
                prepare: Prepared::new(),
                base_image: ImageRef::parse(IMAGE).expect("an image"),
                kbf_image: ImageRef::parse(&format!("docker://{KBF_IMAGE}")).expect("an image"),
                put_dir,
            },
            clock,
        )
    }

    async fn store(&self, message: &impl prost::Message) -> kbf_types::Digest {
        let blob = VerifiedBlob::hashed(Bytes::from(message.encode_to_vec()));
        let digest = blob.digest();
        self.cache.store_blobs(vec![blob]).await.expect("stored");
        digest
    }

    /// An Action whose Command declares the output `out`.
    async fn action(&self) -> Digest {
        let command = Command {
            output_paths: vec!["out".to_owned()],
            ..Command::default()
        };
        let command = self.store(&command).await;
        let action = Action {
            command_digest: Some(wire(&command)),
            ..Action::default()
        };
        wire(&self.store(&action).await)
    }

    /// The key the run commits under for `action`, after writing its pack: a pack whose
    /// manifest names the output `out`, and whose one record is that file's bytes.
    async fn run(&self, mode: Mode, action: &Digest) -> PackKey {
        let digest = cas_digest(action).expect("a digest");
        let mint = PackKeyMint::new(SECRET).expect("a mint");
        let (key, object) = match mode {
            Mode::PutDir => {
                let dir = mint.mint_put_dir("7-3", &digest, clock()).expect("a dir");
                let dir = dir.dir().to_owned();
                let object = format!("{dir}/pack");
                (PackKey::new(dir), object)
            }
            Mode::Presigned => {
                let key = mint
                    .mint("7-3", &digest, clock(), Slot::Pack(0))
                    .expect("a key");
                (PackKey::new(key.key()), key.key().to_owned())
            }
        };
        let mut manifest = manifest_of("7-3", action);
        manifest.output_files = vec![OutputFile {
            path: "out".to_owned(),
            digest: VerifiedBlob::hashed(Bytes::from_static(OUTPUT)).digest(),
            is_executable: false,
        }];
        let pack = write_pack(&manifest, &[OUTPUT]).expect("a pack");
        let object = ObjectKey::new(object).expect("an object key");
        let put = self.store.put_new(&object, Bytes::from(pack), None);
        put.await.expect("written");
        key
    }

    /// What the stores hold of the run of `action` whose output is [`OUTPUT`].
    async fn held(&self, action: &Digest) -> Held {
        let blob = kbf_pack::blob_digest(OUTPUT);
        let located = self.index.locate(&[blob]).expect("located").remove(0);
        let pack = located.as_ref().map(|at| {
            self.index
                .pack(&at.pack_key)
                .expect("asked")
                .expect("a row")
        });
        let action = cas_digest(action).expect("a digest");
        let entry = self.cache.action_result(&action).await.expect("asked");
        Held {
            located,
            pack,
            entry: entry.map(|(digest, result)| (wire(&digest), result)),
        }
    }
}

/// The result and caching of an ingest that ran, or a panic naming what it was.
fn ran(done: Ingested) -> (reapi::ActionResult, bool) {
    match done {
        Ingested::Ran { result, cached } => (*result, cached),
        other => panic!("not ingested: {other:?}"),
    }
}

/// What the stores hold after one ingest of the run, for comparison with two.
async fn once(mode: Mode) -> Held {
    let plane = Plane::new();
    let front = plane.frontend("fe1", mode);
    let action = plane.action().await;
    let key = plane.run(mode, &action).await;
    ran(front.ingest(lease(), &key, &action, &limits()).await);
    plane.held(&action).await
}

/// One ingest of the run by `front`.
async fn ingest_of(
    front: &Front,
    key: &PackKey,
    action: &Digest,
    caps: &UntrustedLimits,
) -> (reapi::ActionResult, bool) {
    ran(front.ingest(lease(), key, action, caps).await)
}

/// Asserts, for one mode and order, that both ingests return `Ran` with equal results, that the
/// action cache holds one entry for the action and it is the result, and that the index holds
/// the rows one ingest leaves (and, run in sequence, that the second changes none).
async fn leaves_one_result(mode: Mode, order: Order) {
    let plane = Plane::new();
    let (a, b) = (plane.frontend("fe1", mode), plane.frontend("fe2", mode));
    let action = plane.action().await;
    let key = plane.run(mode, &action).await;
    let caps = limits();
    let ingest = |front| ingest_of(front, &key, &action, &caps);
    let which = format!("{mode:?} {order:?}");
    let (first, second, between) = match order {
        Order::AThenB | Order::BThenA => {
            let (early, late) = if matches!(order, Order::AThenB) {
                (&a, &b)
            } else {
                (&b, &a)
            };
            let first = ingest(early).await;
            let between = Some(plane.held(&action).await);
            (first, ingest(late).await, between)
        }
        Order::Together => {
            let (first, second) = futures::join!(ingest(&a), ingest(&b));
            (first, second, None)
        }
    };
    assert_eq!(first, second, "{which}: the two results differ");
    assert!(first.1, "{which}: not cached");

    let after = plane.held(&action).await;
    let (_, entry) = after.entry.as_ref().expect("an action-cache entry");
    assert_eq!(entry, &first.0, "{which}: the entry is not the result");
    let at = after.located.as_ref().expect("the output is indexed");
    assert!(at.pack_key.starts_with("cas/b/7-3/"), "{which}: {at:?}");
    assert!(after.pack.is_some(), "{which}: the pack has no row");
    assert_eq!(
        after,
        once(mode).await,
        "{which}: more than one ingest holds"
    );
    if let Some(between) = between {
        assert_eq!(between, after, "{which}: the second ingest changed a row");
    }
}

/// Catches an ingest of a put directory that is not repeatable when the frontend that began it
/// is stalled and the adopter ingests it again: frontend A then B leaves the index rows of one
/// ingest (the output's location and its pack's row unchanged by the second), one action-cache
/// entry equal to the result, and both return `Ran` with equal results.
#[tokio::test]
async fn a_put_dir_ingested_by_a_then_b_leaves_one_result() {
    leaves_one_result(Mode::PutDir, Order::AThenB).await;
}

/// Catches the same repeat in the other order: B first (the adopter of a job whose first
/// frontend is slow), then A, as the zombie window of the fence allows.
#[tokio::test]
async fn a_put_dir_ingested_by_b_then_a_leaves_one_result() {
    leaves_one_result(Mode::PutDir, Order::BThenA).await;
}

/// Catches two ingests of one put directory that overlap (the stalled frontend wakes while the
/// adopter is mid-ingest): both return `Ran` with equal results and the stores hold what one
/// ingest leaves.
#[tokio::test]
async fn a_put_dir_ingested_by_both_at_once_leaves_one_result() {
    leaves_one_result(Mode::PutDir, Order::Together).await;
}

/// Catches the presigned key (single ownership's mode) not being repeatable, in order A, B.
#[tokio::test]
async fn a_presigned_key_ingested_by_a_then_b_leaves_one_result() {
    leaves_one_result(Mode::Presigned, Order::AThenB).await;
}

/// Catches the presigned key not being repeatable, in order B, A.
#[tokio::test]
async fn a_presigned_key_ingested_by_b_then_a_leaves_one_result() {
    leaves_one_result(Mode::Presigned, Order::BThenA).await;
}

/// Catches two overlapping ingests of one presigned key leaving a second set of rows.
#[tokio::test]
async fn a_presigned_key_ingested_by_both_at_once_leaves_one_result() {
    leaves_one_result(Mode::Presigned, Order::Together).await;
}

/// Catches two ingests running on separate threads (real interleaving of the object store,
/// index and cache calls, not one task's join) leaving anything but one result: both return
/// `Ran` with equal results, and the rows are those of one ingest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_threads_ingesting_one_put_dir_leave_one_result() {
    let plane = Plane::new();
    let action = plane.action().await;
    let key = plane.run(Mode::PutDir, &action).await;
    let mut tasks = Vec::new();
    for name in ["fe1", "fe2"] {
        let front = plane.frontend(name, Mode::PutDir);
        let (key, action) = (key.clone(), action.clone());
        tasks.push(tokio::spawn(async move {
            ran(front.ingest(lease(), &key, &action, &limits()).await)
        }));
    }
    let first = tasks.remove(0).await.expect("joined");
    let second = tasks.remove(0).await.expect("joined");
    assert_eq!(first, second);
    assert!(first.1);
    assert_eq!(plane.held(&action).await, once(Mode::PutDir).await);
}
