//! The seams of a frontend whose tasks commit into put directories (`--task-out-mode put-dir`):
//! the directory and the action tree a run gets, and the ingest of what it committed.

use std::sync::OnceLock;

use kbf_front::action_tree::{BaseFacts, BaseTree};
use kbf_front::prepare_run::{ClosureLimits, collect_closure_keeping};

use super::*;

/// A preparation that makes put directories with the real mint and the real closure over the
/// rig's CAS, or refuses as told.
struct DirPrep {
    backend: OnceLock<Backend>,
    refuse: Option<PrepareError>,
    /// How long past now the directory's keys are good.
    good_for: u64,
}

impl DirPrep {
    fn new(refuse: Option<PrepareError>, good_for: u64) -> Arc<Self> {
        Arc::new(Self {
            backend: OnceLock::new(),
            refuse,
            good_for,
        })
    }
}

#[tonic::async_trait]
impl RunPrepare for DirPrep {
    async fn prepare_run(
        &self,
        _: &str,
        _: &kbf_types::Digest,
        _: SystemTime,
        _: Duration,
    ) -> Result<PreparedRun, PrepareError> {
        Err(PrepareError::Refused("only put directories".to_owned()))
    }

    async fn prepare_put_dir(
        &self,
        lease: &str,
        action: &kbf_types::Digest,
        now: SystemTime,
        _: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        if let Some(refused) = &self.refuse {
            return Err(refused.clone());
        }
        let backend = self.backend.get().expect("the rig set the backend");
        let kept = collect_closure_keeping(backend, action, ClosureLimits::DEFAULT).await;
        let kept = kept.map_err(|e| PrepareError::Unavailable(e.to_string()))?;
        let mint = PackKeyMint::new(SECRET).expect("a mint");
        let dir = mint.mint_put_dir(lease, action, now).expect("a dir");
        Ok(PreparedPutDir {
            dir: dir.dir().to_owned(),
            kept,
            not_after: NOW + self.good_for,
        })
    }
}

/// A base tree of one symlink.
fn base() -> BaseTree {
    let root = br#"{"v":1,"children":[{"name":"bin","kind":"symlink","target":"usr/bin"}]}"#;
    BaseTree::new(
        root,
        BaseFacts {
            entries: 1,
            depth: 1,
        },
    )
    .expect("a base")
}

/// A rig in put-directory mode over `prepare`; the base tree published when `published`.
fn rig_with(prepare: Arc<DirPrep>, published: bool) -> Rig {
    let slot = Arc::new(OnceLock::new());
    if published {
        slot.set(base()).expect("set once");
    }
    let dirs = Arc::clone(&prepare);
    Rig::over(Prepared::new(), prepare, move |packs, backend| {
        dirs.backend.set(backend.clone()).expect("set once");
        Some(PutDirParts::new(packs, backend, slot))
    })
}

fn rig() -> Rig {
    rig_with(DirPrep::new(None, 7_200), true)
}

/// An Action whose input root holds one file named `name` and whose Command declares `out`.
async fn action_with_input(rig: &Rig, name: &str) -> Digest {
    let file = rig.store(&reapi::Command::default()).await;
    let root = rig
        .store(&Directory {
            files: vec![FileNode {
                name: name.to_owned(),
                digest: Some(wire(&file)),
                ..FileNode::default()
            }],
            ..Directory::default()
        })
        .await;
    rig.action(
        |a| a.input_root_digest = Some(wire(&root)),
        |c| c.output_paths = vec!["out".to_owned()],
    )
    .await
}

const WALL: Duration = Duration::from_secs(3_600);

/// Asserts that a run gets its put directory (verified by the mint for its action), that the
/// action tree is written to the store and its digest handed out once, and that no write and
/// no grant was prepared for it.
#[tokio::test]
async fn a_run_gets_its_dir_and_its_tree_once_and_no_write_or_grant() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    let key = rig.front.mint(lease(), &action, WALL).await.expect("a dir");
    let nonce = key
        .as_str()
        .strip_prefix("cas/b/7-3/")
        .expect("the lease's");
    assert_eq!(nonce.len(), 40, "{key}");
    let dir = IssuedPutDir::new("7-3", nonce).expect("a dir");
    let digest = cas_digest(&action).expect("a digest");
    assert_eq!(rig.mint.verify_put_dir(&dir, &digest), Ok(clock()));

    let tree = rig.front.tree(lease()).await.expect("a tree");
    assert_eq!(tree.len(), 64, "{tree}");
    let manifest = ObjectKey::new(format!("cas/l/{tree}")).expect("a key");
    assert!(rig.store.len(&manifest).await.expect("asked").is_some());
    assert!(matches!(
        rig.front.tree(lease()).await,
        Err(InputsError::Unavailable(_))
    ));
    assert!(matches!(
        rig.front.put_url(&key, WALL, 1).await,
        Err(PresignError::Refused(_))
    ));
    assert!(matches!(
        rig.front.read_grant(lease(), &action, WALL).await,
        Err(InputsError::Unavailable(_))
    ));
}

/// Asserts that before the base tree is published no tree can be written: the farm's to retry.
#[tokio::test]
async fn before_the_base_tree_is_published_a_run_is_unavailable() {
    let rig = rig_with(DirPrep::new(None, 7_200), false);
    let action = action_with_input(&rig, "input").await;
    let refused = rig.front.mint(lease(), &action, WALL).await;
    assert!(
        matches!(&refused, Err(MintError::Unavailable(why)) if why.contains("base tree")),
        "{refused:?}"
    );
    assert!(rig.front.tree(lease()).await.is_err());
}

/// Asserts whose a tree that cannot be written is: an input root holding `.kbf-out` is the
/// caller's (refused), a store that cannot say where the blobs are is the farm's (unavailable).
#[tokio::test]
async fn a_tree_that_cannot_be_written_is_refused_or_unavailable() {
    let rig = rig();
    let reserved = action_with_input(&rig, ".kbf-out").await;
    let refused = rig.front.mint(lease(), &reserved, WALL).await;
    assert!(
        matches!(&refused, Err(MintError::Refused(why)) if why.contains(".kbf-out")),
        "{refused:?}"
    );

    let action = action_with_input(&rig, "input").await;
    rig.store.fail_reads.store(true, Ordering::SeqCst);
    let down = rig.front.mint(lease(), &action, WALL).await;
    assert!(matches!(down, Err(MintError::Unavailable(_))), "{down:?}");
}

/// Asserts that the preparation's refusals keep whose they are, that a directory whose keys
/// end before the run's wall is refused, and that a preparation that makes no put directory
/// refuses every run.
#[tokio::test]
async fn a_preparation_that_cannot_give_a_dir_refuses_the_run() {
    for (refuse, unavailable) in [
        (PrepareError::Refused("over budget".to_owned()), false),
        (PrepareError::Unavailable("the store".to_owned()), true),
    ] {
        let rig = rig_with(DirPrep::new(Some(refuse), 7_200), true);
        let action = action_with_input(&rig, "input").await;
        let got = rig.front.mint(lease(), &action, WALL).await;
        assert_eq!(
            matches!(got, Err(MintError::Unavailable(_))),
            unavailable,
            "{got:?}"
        );
        assert!(got.is_err());
    }
    let short = rig_with(DirPrep::new(None, WALL.as_secs() - 1), true);
    let action = action_with_input(&short, "input").await;
    let got = short.front.mint(lease(), &action, WALL).await;
    assert!(matches!(got, Err(MintError::Refused(_))), "{got:?}");

    let presigned_only = Rig::over(Prepared::new(), Prepared::new(), |packs, backend| {
        let published = Arc::new(OnceLock::new());
        published.set(base()).expect("set once");
        Some(PutDirParts::new(packs, backend, published))
    });
    let action = action_with_input(&presigned_only, "input").await;
    let got = presigned_only.front.mint(lease(), &action, WALL).await;
    assert_eq!(
        got,
        Err(MintError::Refused(
            "this preparation makes no put directory".to_owned()
        ))
    );
}

/// Asserts that a pack committed as `pack` in the run's directory is ingested into the action's
/// result, that a key that is no put directory is refused as the wrong key (whatever the
/// action's digest), and that a bad digest of the action with the right key is unavailable.
#[tokio::test]
async fn a_pack_committed_in_the_dir_becomes_the_result() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    let key = rig.front.mint(lease(), &action, WALL).await.expect("a dir");
    let pack = write_pack(&manifest_of("7-3", &action), &[]).expect("a pack");
    put_pack(&rig, &PackKey::new(format!("{key}/pack")), pack).await;

    let done = rig.front.ingest(lease(), &key, &action, &limits()).await;
    assert!(
        matches!(done, Ingested::Ran { cached: true, .. }),
        "{done:?}"
    );

    // The key is judged before the action: a bad digest does not hide a wrong key.
    let bad_action = Digest {
        hash: "not hex".to_owned(),
        size_bytes: -1,
    };
    let got = rig
        .front
        .ingest(lease(), &PackKey::new("cas/b/7-3"), &bad_action, &limits())
        .await;
    assert!(
        matches!(got, Ingested::Reject(RejectReason::WrongKey)),
        "{got:?}"
    );
    let got = rig
        .front
        .ingest(lease(), &key, &bad_action, &limits())
        .await;
    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");

    for bad in ["cas/b/7-3", "elsewhere/7-3/abc"] {
        let got = rig
            .front
            .ingest(lease(), &PackKey::new(bad), &action, &limits())
            .await;
        assert!(
            matches!(got, Ingested::Reject(RejectReason::WrongKey)),
            "{bad}: {got:?}"
        );
    }
}
