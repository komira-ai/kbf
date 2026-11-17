//! The executor's seams [`JsAgentFlags::front`] makes.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::executor::RunId;
use kbf_front::{Cache, CacheStorage, PackKeyMint, PackSettings, SegmentsConfig};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::Digest;
use kbf_state::ManualClock;

use super::*;
use crate::executor::{
    Ingest as _, InputTree, MintError, MintedDir, PrepareError, PreparedPutDir, PutDirRoots,
    RunPrepare,
};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// A preparation that refuses every run, and counts the put directories it is asked for.
#[derive(Default)]
struct Refusing {
    asked: Mutex<usize>,
}

const REFUSED: &str = "this test preparation refuses every run";

#[tonic::async_trait]
impl RunPrepare for Refusing {
    async fn prepare_put_dir(
        &self,
        _: &str,
        _: &kbf_types::Digest,
        _: SystemTime,
        _: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        *self.asked.lock().expect("a count") += 1;
        Err(PrepareError::Refused(REFUSED.to_owned()))
    }

    async fn mint_put_dir(
        &self,
        _: &str,
        _: &kbf_types::Digest,
        _: SystemTime,
        _: Duration,
    ) -> Result<MintedDir, PrepareError> {
        Err(PrepareError::Refused(REFUSED.to_owned()))
    }
}

/// A base tree of one symlink.
fn base() -> kbf_front::action_tree::BaseTree {
    let root = br#"{"v":1,"children":[{"name":"bin","kind":"symlink","target":"usr/bin"}]}"#;
    let facts = kbf_front::action_tree::BaseFacts {
        entries: 1,
        depth: 1,
    };
    kbf_front::action_tree::BaseTree::new(root, facts).expect("a base")
}

fn flags() -> JsAgentFlags {
    let mut config = Config::new("front-1");
    config.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
    let image = format!("docker://registry.test/app@sha256:{}", "ab".repeat(32));
    JsAgentFlags {
        config,
        job_url: "https://jobs.test:9443".to_owned(),
        token_file: "/nowhere/token".into(),
        ca_file: None,
        base_image: ImageRef::parse(&image).expect("an image"),
        kbf_image: ImageRef::parse(&image).expect("an image"),
        base_tree: "/opt/kbf/base-tree".into(),
        base_tree_url: String::new(),
        base_tree_keep: 1,
        base_tree_timeout: std::time::Duration::from_secs(60),
        published: Arc::default(),
        put_retry: std::time::Duration::from_secs(60),
    }
}

/// The packs and the backend over them that a front is made of.
fn packs_and_backend() -> (Packs, Backend) {
    let objects = Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    }));
    let index: Arc<dyn PackIndex> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let packs = Packs::new(objects, index, keys, PackSettings::default()).expect("packs");
    let node = Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ));
    let backend = Backend::remote_over(node, Some(packs.clone()));
    (packs, backend)
}

/// The key a front of `flags` mints for lease 7.3, and how many put directories it asked for.
async fn minted(flags: &JsAgentFlags) -> (Result<String, MintError>, usize) {
    let (packs, backend) = packs_and_backend();
    let prepare = Arc::new(Refusing::default());
    let source = RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: prepare.clone(),
    };
    let front = flags.front(packs, backend, source);
    let action = Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    };
    let key = front
        .mint(
            RunId::new(kbf_types::OperationId(7), 3),
            &action,
            Duration::from_secs(60),
            InputTree::Manifests,
        )
        .await;
    let asked = *prepare.asked.lock().expect("a count");
    (key.map(|key| key.as_str().to_owned()), asked)
}

/// Asserts that a front writes over the base its flags publish: none before it is set, so the
/// mint is unavailable and the preparation is not asked, and then it asks its preparation for a
/// put directory and passes on what the preparation says.
#[tokio::test]
async fn the_front_mints_over_the_base_its_flags_publish() {
    let flags = flags();
    let (key, asked) = minted(&flags).await;
    assert!(
        matches!(&key, Err(MintError::Unavailable(why)) if why.contains("base tree")),
        "{key:?}"
    );
    assert_eq!(asked, 0);
    flags.published.set(base()).expect("set once");
    let (key, asked) = minted(&flags).await;
    assert_eq!(key, Err(MintError::Refused(REFUSED.to_owned())));
    assert_eq!(asked, 1);
}

/// What `flags.build` makes over `store`, with a token file and a plain job url of its own.
fn built(
    mut flags: JsAgentFlags,
    store: Option<Arc<dyn kbf_state::Store>>,
) -> Result<crate::executor::Executor, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::test_dir::scratch_dir("executor-store");
    let token = dir.path().join("token");
    std::fs::write(&token, "a-token-of-printable-bytes-0123456789\n").expect("written");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("mode");
    flags.token_file = token;
    flags.job_url = "http://127.0.0.1:7".to_owned();
    let (packs, backend) = packs_and_backend();
    let source = RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: Arc::new(Refusing::default()),
    };
    let front = flags.front(packs, backend, source);
    flags.build(
        Arc::new(front),
        Arc::new(Metrics::default()),
        store,
        kbf_front::Recorder::off(),
        RETENTION,
    )
}

/// The finished retention the tests build with.
const RETENTION: Duration = Duration::from_secs(42);

/// Catches `build` refusing a state store or the lack of one: with no store and over a store it
/// builds; settings that break a rule of the executor are refused.
#[tokio::test]
async fn build_makes_the_executor_with_or_without_a_state_store() {
    let store: Arc<dyn kbf_state::Store> = Arc::new(kbf_state::fake::Fake::with_flaw(
        Arc::new(ManualClock::default()),
        kbf_state::fake::Flaw::None,
    ));
    assert!(built(flags(), None).is_ok());
    assert!(built(flags(), Some(store)).is_ok());
    let mut zero = flags();
    zero.config.live = Duration::ZERO;
    let why = built(zero, None).expect_err("refused").to_string();
    assert!(why.contains("liveness time"), "{why}");
}
