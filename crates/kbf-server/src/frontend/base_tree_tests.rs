//! The start's publish of the kbf base tree, over an in-memory pack store: what it writes, the
//! `job-tree/` row it records and how each failure is named. The helper [`bundle_dir`] writes a
//! release's bundles for the tests that serve a frontend (`executor_tests`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::{PackSettings, Packs};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore, ObjectKey, ObjectStore as _, PageSize};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_tree_bundle::{Dir, Facts, Node, Writer, file_name};
use sha2::{Digest as _, Sha256};

use super::Start;
use crate::job_tree;
use crate::job_tree::tests::Refusing;
use crate::serve::ServeError;
use crate::test_dir::scratch_dir;
use crate::test_log::capture;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// What a bundle holds: its root's digest and each object's, in hex, in the bundle's order.
pub(in crate::frontend) struct Bundled {
    pub(in crate::frontend) root: String,
    pub(in crate::frontend) objects: Vec<String>,
}

/// The bundle of `commit` for `platform`, a tree of `/etc/passwd`, `/usr/bin/tool` and
/// `/.kbf-tools/kbf-task`: its bytes and what is in it.
fn bundle(commit: &str, platform: &str) -> (Vec<u8>, Bundled) {
    let files: [&[u8]; 3] = [b"root:x:0:0::/root:/bin/nologin\n", b"tool\n", b"kbf-task"];
    let mut etc = Dir::new();
    etc.insert("passwd", Node::file(files[0], false));
    let mut bin = Dir::new();
    bin.insert("tool", Node::file(files[1], true));
    let mut usr = Dir::new();
    usr.insert("bin", Node::Dir(bin));
    let mut tools = Dir::new();
    tools.insert("kbf-task", Node::file(files[2], true));
    let mut root = Dir::new();
    root.insert("etc", Node::Dir(etc));
    root.insert("usr", Node::Dir(usr));
    root.insert(".kbf-tools", Node::Dir(tools));
    let built = root.build().expect("a tree");
    let facts = Facts {
        commit: commit.to_owned(),
        platform: platform.to_owned(),
        root: built.root.clone(),
        entries: built.entries,
        depth: built.depth as u64,
        normalised: Vec::new(),
    };
    let mut writer = Writer::new(Vec::new(), &facts).expect("a bundle");
    let mut objects = Vec::new();
    for file in files {
        objects.push(writer.object(file).expect("written"));
    }
    for (_, manifest) in &built.manifests {
        objects.push(writer.object(manifest).expect("written"));
    }
    let bundled = Bundled {
        root: built.root,
        objects,
    };
    (writer.finish().expect("finished"), bundled)
}

/// Writes into `dir` the bundles of the release `commit` for each of `platforms`, and the
/// release's `SHA256SUMS` that lists them: what a frontend's `--executor-base-tree` names. Gives
/// what the tree holds (the same for every platform).
pub(in crate::frontend) fn bundle_dir(dir: &Path, commit: &str, platforms: &[&str]) -> Bundled {
    let mut sums = String::new();
    for platform in platforms {
        let (bytes, _) = bundle(commit, platform);
        let name = file_name(commit, platform);
        std::fs::write(dir.join(&name), &bytes).expect("a bundle");
        sums.push_str(&format!("{}  {name}\n", sha(&bytes)));
    }
    std::fs::write(dir.join("SHA256SUMS"), sums).expect("the sums");
    bundle(commit, "linux-x86_64").1
}

fn packs() -> (Packs, Arc<MemoryStore>) {
    let objects = Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    }));
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let packs = Packs::new(Arc::clone(&objects), index, keys, PackSettings::default());
    (packs.expect("packs"), objects)
}

/// How many objects the pack store holds.
async fn held(objects: &MemoryStore) -> usize {
    let prefix = KeyPrefix::new("").expect("a prefix");
    let page = objects.list(&prefix, None, PageSize::MAX).await;
    page.expect("a listing").objects.len()
}

fn state() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_700_000_000_456)
}

fn start(dir: &Path, packs: Packs, state: Option<Arc<dyn Store>>) -> Start {
    Start {
        dir: dir.to_owned(),
        platform: "linux-x86_64".to_owned(),
        packs,
        state,
    }
}

fn key_of(hex: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/l/{hex}")).expect("a key")
}

/// Asserts that the start writes the tree's objects to the pack store, records
/// `job-tree/<root>` with the commit and the clock's time, and says so in one line with the
/// root and the counts; and that a second start over the same row writes no object and leaves the
/// row as the first left it. Catches: a publish that forgets the row, a row rewritten by a later
/// start, and a start that logs nothing an operator can read.
#[tokio::test]
async fn the_start_publishes_the_tree_and_records_it_once() {
    let dir = scratch_dir("base-tree-publish");
    let root = bundle_dir(&dir, COMMIT, &["linux-x86_64"]).root;
    let (packs, objects) = packs();
    let store = state();
    let lines = capture();

    let first = start(&dir, packs.clone(), Some(store.clone()))
        .publish_as(COMMIT, now)
        .await
        .expect("published");

    assert_eq!(first.root.hash_hex(), root);
    assert_eq!(first.held, 0);
    assert!(first.written > 0);
    assert!(
        objects
            .len(&key_of(&root))
            .await
            .expect("a length")
            .is_some()
    );
    let row = store
        .get(&job_tree::key(&root))
        .expect("read")
        .expect("a row");
    assert_eq!(
        row.value,
        format!(r#"{{"release":"{COMMIT}","first_published_ms":1700000000456}}"#).into_bytes()
    );
    let text = lines.text();
    assert!(text.contains("published the kbf base tree"), "{text}");
    assert!(text.contains(&root), "{text}");
    assert!(
        text.contains(&format!("written={}", first.written)),
        "{text}"
    );
    assert!(text.contains("held=0"), "{text}");

    let later = start(&dir, packs, Some(store.clone()))
        .publish_as(COMMIT, || now() + Duration::from_secs(60))
        .await
        .expect("again");

    assert_eq!((later.written, later.held), (0, first.written));
    assert_eq!(store.get(&job_tree::key(&root)).expect("read"), Some(row));
    let text = lines.text();
    assert!(text.contains("recorded by an earlier publish"), "{text}");
}

/// Asserts that a frontend without a state store publishes the tree and writes no row (there is
/// nowhere to write it). Catches: a start that needs `--state-store` to publish.
#[tokio::test]
async fn without_a_state_store_the_tree_is_published_and_no_row_is_written() {
    let dir = scratch_dir("base-tree-no-state");
    let root = bundle_dir(&dir, COMMIT, &["linux-x86_64"]).root;
    let (packs, objects) = packs();

    let published = start(&dir, packs, None)
        .publish_as(COMMIT, now)
        .await
        .expect("published");

    assert_eq!(published.root.hash_hex(), root);
    assert!(
        objects
            .len(&key_of(&root))
            .await
            .expect("a length")
            .is_some()
    );
}

/// Asserts that a failing publish (no bundle for the platform) is a start error that names
/// `--executor-base-tree` and the bundle's path, and records no row. Catches: a start that goes
/// on without a base tree, and one that records a tree it did not publish.
#[tokio::test]
async fn a_publish_that_fails_names_the_flag_and_records_nothing() {
    let dir = scratch_dir("base-tree-missing");
    bundle_dir(&dir, COMMIT, &["linux-arm64"]);
    let (packs, _) = packs();
    let store = state();

    let error = start(&dir, packs, Some(store.clone()))
        .publish_as(COMMIT, now)
        .await
        .expect_err("refused");

    let ServeError::BaseTree(why) = &error else {
        panic!("not a base tree error: {error}");
    };
    assert!(
        error.to_string().starts_with("--executor-base-tree: "),
        "{error}"
    );
    assert!(why.contains(&file_name(COMMIT, "linux-x86_64")), "{why}");
    assert!(
        store
            .scan_prefix(job_tree::PREFIX)
            .expect("a scan")
            .is_empty()
    );
}

/// Asserts that a build that does not know its commit (`BUILD_COMMIT_FULL` empty) fails the
/// start naming the flag and why. Catches: a start that looks for `kbf-base-tree--linux-...`.
#[tokio::test]
async fn a_build_without_a_commit_fails_the_start() {
    let dir = scratch_dir("base-tree-no-commit");
    bundle_dir(&dir, COMMIT, &["linux-x86_64"]);
    let (packs, objects) = packs();

    let error = start(&dir, packs, None)
        .publish_as("", now)
        .await
        .expect_err("refused");

    assert_eq!(
        error.to_string(),
        "--executor-base-tree: this build does not know its commit, so it has no base tree"
    );
    assert_eq!(held(&objects).await, 0);
}

/// Asserts that a state store that fails the record fails the start naming the flag and the
/// row's key, after the objects were published. Catches: a record that is lost without a word.
#[tokio::test]
async fn a_state_store_that_fails_the_record_fails_the_start() {
    let dir = scratch_dir("base-tree-record-fails");
    let root = bundle_dir(&dir, COMMIT, &["linux-x86_64"]).root;
    let (packs, _) = packs();
    let refusing: Arc<dyn Store> = Arc::new(Refusing(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )));

    let error = start(&dir, packs, Some(refusing))
        .publish_as(COMMIT, now)
        .await
        .expect_err("refused");

    assert_eq!(
        error.to_string(),
        format!(
            "--executor-base-tree: {}: state store failure: the disk is full",
            job_tree::key(&root)
        )
    );
}
