//! The executor's seams [`JsAgentFlags::front`] makes, by the output mode.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use kbf_front::{Cache, CacheStorage, PackKeyMint, PackSettings, SegmentsConfig};
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::Digest;
use kbf_state::ManualClock;
use kbf_types::LeaseId;

use super::*;
use crate::executor::{
    Ingest as _, MintError, PrepareError, PreparedRun, PutDirRoots, RunPrepare, Secret,
};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// A preparation of presigned runs only, which counts the runs it is asked for.
#[derive(Default)]
struct Presigned {
    asked: Mutex<usize>,
}

#[tonic::async_trait]
impl RunPrepare for Presigned {
    async fn prepare_run(
        &self,
        _: &str,
        _: &kbf_types::Digest,
        _: SystemTime,
        _: Duration,
    ) -> Result<PreparedRun, PrepareError> {
        *self.asked.lock().expect("a count") += 1;
        Ok(PreparedRun {
            key: "cas/b/7-3/aa-0".to_owned(),
            put_url: Secret::new("https://store.test/put"),
            grant_url: "https://grant.test:9400".to_owned(),
            grant_token: Secret::new("token"),
            not_after: u64::MAX,
        })
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

fn flags(out: TaskOut) -> JsAgentFlags {
    let mut config = Config::new("front-1", 1, 2_000, 16 << 30);
    config.task_out = out;
    let image = format!("docker://registry.test/app@sha256:{}", "ab".repeat(32));
    JsAgentFlags {
        config,
        job_url: "https://jobs.test:9443".to_owned(),
        token_file: "/nowhere/token".into(),
        ca_file: None,
        base_image: ImageRef::parse(&image).expect("an image"),
        kbf_image: ImageRef::parse(&image).expect("an image"),
        base_tree: "/opt/kbf/base-tree".into(),
        published: Arc::default(),
    }
}

/// The key a front of `flags` mints for lease 7.3, and how many presigned runs it asked for.
async fn minted(flags: &JsAgentFlags) -> (Result<String, MintError>, usize) {
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
    let prepare = Arc::new(Presigned::default());
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
        .mint(LeaseId::new(7, 3), &action, Duration::from_secs(60))
        .await;
    let asked = *prepare.asked.lock().expect("a count");
    (key.map(|key| key.as_str().to_owned()), asked)
}

/// Asserts that a front of presigned writes prepares a presigned run, and that a front of put
/// directories writes over the base its flags publish: none before it is set, and then it asks
/// its preparation for a put directory (which a preparation of presigned runs only refuses),
/// never for a presigned run.
#[tokio::test]
async fn the_front_mints_by_the_output_mode() {
    let (key, asked) = minted(&flags(TaskOut::Presigned)).await;
    assert_eq!((key, asked), (Ok("cas/b/7-3/aa-0".to_owned()), 1));

    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let put_dir = flags(TaskOut::PutDir(roots));
    let (key, asked) = minted(&put_dir).await;
    assert!(
        matches!(&key, Err(MintError::Unavailable(why)) if why.contains("base tree")),
        "{key:?}"
    );
    assert_eq!(asked, 0);
    put_dir.published.set(base()).expect("set once");
    let (key, asked) = minted(&put_dir).await;
    assert_eq!(
        key,
        Err(MintError::Refused(
            "this preparation makes no put directory".to_owned()
        ))
    );
    assert_eq!(asked, 0);
}
