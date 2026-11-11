//! The frontend's start publishing the kbf base tree: a bundle, checked against the release's
//! `SHA256SUMS`, is read one object at a time and each object the store lacks is written to
//! `cas/l/<sha256>`, root last.
//!
//! The store is an in-process [`LargeStore`] that records every length check and write, and
//! can fail a key, hold a key of another length, or have a key appear between the check and the
//! write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::action_tree::{BaseFacts, BaseTree, BaseTreeError};
use kbf_front::base_publish::{PublishError, SUMS, publish};
use kbf_front::large_backfill::LargeStore;
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_testkit::dir::TestDir;
use kbf_tree_bundle::{Built, Dir, Facts, Node, Writer, file_name};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const PLATFORM: &str = "linux-x86_64";
const TOOL: &[u8] = b"tool-bytes\n";
const PASSWD: &[u8] = b"root:x:0:0::/root:/bin/nologin\n";

/// What the store has been asked, and what it holds.
#[derive(Default)]
struct State {
    objects: BTreeMap<String, Bytes>,
    lengths: Vec<String>,
    /// The keys written, in order.
    writes: Vec<String>,
    /// The keys a write was attempted at.
    puts: Vec<String>,
    fail_length: Option<String>,
    fail_put: Option<String>,
    /// Keys that another writer takes between the length check and the write.
    raced: BTreeSet<String>,
    /// Keys the store says are of this length, whatever they hold.
    lying: BTreeMap<String, u64>,
}

#[derive(Default)]
struct Bucket {
    state: Mutex<State>,
}

impl Bucket {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn writes(&self) -> Vec<String> {
        self.state().writes.clone()
    }

    fn object(&self, hex: &str) -> Option<Bytes> {
        self.state().objects.get(&format!("cas/l/{hex}")).cloned()
    }

    fn untouched(&self) -> bool {
        let state = self.state();
        state.lengths.is_empty() && state.puts.is_empty() && state.objects.is_empty()
    }
}

impl LargeStore for Bucket {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(async move {
            let mut state = self.state();
            state.lengths.push(key.to_string());
            if state.fail_length.as_deref() == Some(key.as_str()) {
                return Err(ObjectStoreError::Unsupported("length checks of this key"));
            }
            if let Some(length) = state.lying.get(key.as_str()) {
                return Ok(Some(*length));
            }
            Ok(state.objects.get(key.as_str()).map(|b| b.len() as u64))
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            self.state().puts.push(key.to_string());
            let mut got = Vec::new();
            while let Some(piece) = body.next().await {
                got.extend_from_slice(&piece?);
            }
            assert_eq!(got.len() as u64, len, "a body of the length declared");
            let mut state = self.state();
            if state.fail_put.as_deref() == Some(key.as_str()) {
                return Err(ObjectStoreError::Unsupported("writes to this key"));
            }
            if state.raced.contains(key.as_str()) || state.objects.contains_key(key.as_str()) {
                state.objects.insert(key.to_string(), Bytes::from(got));
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            state.objects.insert(key.to_string(), Bytes::from(got));
            state.writes.push(key.to_string());
            Ok(())
        })
    }
}

fn sha(bytes: &[u8]) -> String {
    Digest::new(DigestFunction::Sha256, Sha256::digest(bytes).into(), 0).hash_hex()
}

fn key(hex: &str) -> String {
    format!("cas/l/{hex}")
}

/// A release's bundle and what is in it.
struct Release {
    bytes: Vec<u8>,
    built: Built,
    facts: Facts,
    /// The digest of the manifest of `/.kbf-tools`.
    tools: String,
    /// The digest of the release's `kbf-task`.
    task: String,
    /// Every object, in the bundle's order, by digest.
    objects: Vec<(String, Vec<u8>)>,
}

/// The bundle of a base tree of `/usr/bin/tool`, `/etc/passwd`, an empty `/tmp`, the top-level
/// directories `top` (empty) and `/.kbf-tools/kbf-task`, which holds `task`. Two releases on
/// one pin differ in `task` alone.
fn release(commit: &str, platform: &str, task: &[u8], top: &[&str]) -> Release {
    let mut bin = Dir::new();
    bin.insert("tool", Node::file(TOOL, true));
    let mut usr = Dir::new();
    usr.insert("bin", Node::Dir(bin));
    let mut etc = Dir::new();
    etc.insert("passwd", Node::file(PASSWD, false));
    let mut tools = Dir::new();
    tools.insert("kbf-task", Node::file(task, true));
    let tools_manifest = tools.build().expect("tools").root;
    let mut root = Dir::new();
    root.insert("usr", Node::Dir(usr));
    root.insert("etc", Node::Dir(etc));
    root.insert("tmp", Node::Dir(Dir::new()));
    root.insert(".kbf-tools", Node::Dir(tools));
    for name in top {
        root.insert(*name, Node::Dir(Dir::new()));
    }
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
    for bytes in [TOOL, PASSWD, task] {
        let hex = writer.object(bytes).expect("written");
        objects.push((hex, bytes.to_vec()));
    }
    for (hex, bytes) in &built.manifests {
        writer.object(bytes).expect("written");
        objects.push((hex.clone(), bytes.clone()));
    }
    Release {
        bytes: writer.finish().expect("finished"),
        built,
        facts,
        tools: tools_manifest,
        task: sha(task),
        objects,
    }
}

fn standard(task: &[u8]) -> Release {
    release(COMMIT, PLATFORM, task, &[])
}

/// A release directory: the bundle, and a sums file that lists it (with a line of another
/// platform's bundle, as a real one has).
fn install(dir: &Path, release: &Release) {
    install_as(
        dir,
        &file_name(COMMIT, PLATFORM),
        &release.bytes,
        &release.bytes,
    );
}

/// The file `name` holding `bytes`, and a sums file that lists `name` with the digest of
/// `listed`.
fn install_as(dir: &Path, name: &str, bytes: &[u8], listed: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect("the bundle");
    let other = file_name(COMMIT, "linux-arm64");
    let sums = format!("{}  {other}\n{}  {name}\n", sha(b"arm"), sha(listed));
    std::fs::write(dir.join(SUMS), sums).expect("the sums");
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

async fn run(
    dir: &Path,
    store: &Arc<Bucket>,
) -> Result<kbf_front::base_publish::Published, PublishError> {
    publish(dir, COMMIT, PLATFORM, store.clone()).await
}

/// Asserts that a first publish writes every object of the bundle to `cas/l/<sha256>` with its
/// bytes, holds none, and hands back the base tree of the root manifest with the bundle's
/// facts, under the root's digest. Catches: an object left out, one written under another key
/// or with other bytes, and a base tree not made from the root the facts name.
#[tokio::test]
async fn a_first_publish_writes_every_object() {
    let dir = scratch("base-publish-first");
    let release = standard(b"kbf-task of release one");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());

    let published = run(&dir, &store).await.expect("published");

    assert_eq!(published.written, release.objects.len() as u64);
    assert_eq!(published.held, 0);
    for (hex, bytes) in &release.objects {
        assert_eq!(
            store.object(hex).as_deref(),
            Some(bytes.as_slice()),
            "{hex}"
        );
    }
    assert_eq!(published.facts, release.facts);
    assert_eq!(published.root.hash_hex(), release.built.root);
    let root = &release.built.manifests.last().expect("a root").1;
    assert_eq!(published.root.size_bytes, root.len() as u64);
    let facts = BaseFacts {
        entries: release.built.entries,
        depth: release.built.depth,
    };
    assert_eq!(published.base, BaseTree::new(root, facts).expect("a base"));
}

/// Asserts that a second release on the same pin (the same base files, another `kbf-task`)
/// writes exactly its `kbf-task`, its `.kbf-tools` manifest and its root, and holds the other
/// objects: one length check each, no write. Catches: a publish that rewrites the base image's
/// objects on every release, and one that holds an object whose key differs.
#[tokio::test]
async fn a_second_release_on_the_same_pin_writes_three_objects() {
    let dir = scratch("base-publish-second");
    let store = Arc::new(Bucket::default());
    let one = standard(b"kbf-task of release one");
    install(&dir, &one);
    run(&dir, &store).await.expect("the first release");
    let writes_before = store.writes().len();
    let two = standard(b"kbf-task of release two, which differs");
    install(&dir, &two);

    let published = run(&dir, &store).await.expect("the second release");

    assert_eq!(published.written, 3);
    assert_eq!(published.held, two.objects.len() as u64 - 3);
    let written: BTreeSet<_> = store
        .writes()
        .split_off(writes_before)
        .into_iter()
        .collect();
    let want: BTreeSet<_> = [&two.task, &two.tools, &two.built.root]
        .into_iter()
        .map(|hex| key(hex))
        .collect();
    assert_eq!(written, want);
}

/// Asserts that publishing a release already published writes nothing and sends no write
/// request. Catches: a restart that rewrites the tree.
#[tokio::test]
async fn publishing_the_same_release_again_writes_nothing() {
    let dir = scratch("base-publish-again");
    let store = Arc::new(Bucket::default());
    let release = standard(b"kbf-task");
    install(&dir, &release);
    run(&dir, &store).await.expect("first");
    let puts = store.state().puts.len();

    let again = run(&dir, &store).await.expect("again");

    assert_eq!(
        (again.written, again.held),
        (0, release.objects.len() as u64)
    );
    assert_eq!(store.state().puts.len(), puts);
}

/// Asserts that an object whose key is taken between the length check and the write (the store
/// refuses the create-only write) is success, counted as held, and the publish goes on. Catches:
/// a start that fails because another frontend published the same tree at the same moment.
#[tokio::test]
async fn a_key_that_exists_at_put_time_is_success() {
    let dir = scratch("base-publish-raced");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    store.state().raced.insert(key(&sha(TOOL)));

    let published = run(&dir, &store).await.expect("published");

    assert_eq!(published.held, 1);
    assert_eq!(published.written, release.objects.len() as u64 - 1);
    assert!(store.object(&sha(TOOL)).is_some());
}

/// Asserts that a failing length check refuses the publish, naming the key and the call, with
/// objects more than the channel holds still to come (the reader stops instead of waiting).
/// Catches: a failure reported as a held object, and a reader thread that waits for ever on a
/// writer that has gone.
#[tokio::test]
async fn a_failing_length_check_fails_by_name_with_the_key() {
    let dir = scratch("base-publish-length-fails");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    store.state().fail_length = Some(key(&sha(PASSWD)));

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Store {
        key: failed, call, ..
    } = &error
    else {
        panic!("not a store failure: {error}");
    };
    assert_eq!(
        (failed.as_str(), *call),
        (key(&sha(PASSWD)).as_str(), "length check")
    );
    assert!(error.to_string().contains(&key(&sha(PASSWD))), "{error}");
    assert!(
        store.object(&release.built.root).is_none(),
        "the root is not written"
    );
}

/// Asserts that a failing write refuses the publish, naming the key and the call, and writes no
/// object after it. Catches: a write failure that is skipped, and objects written past it.
#[tokio::test]
async fn a_failing_write_fails_by_name_with_the_key() {
    let dir = scratch("base-publish-put-fails");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    store.state().fail_put = Some(key(&release.tools));

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Store {
        key: failed, call, ..
    } = &error
    else {
        panic!("not a store failure: {error}");
    };
    assert_eq!(
        (failed.as_str(), *call),
        (key(&release.tools).as_str(), "write")
    );
    assert!(error.to_string().contains(&key(&release.tools)), "{error}");
    assert!(!store.writes().contains(&key(&release.built.root)));
}

/// Asserts that an object of another length held at its key is refused by name, with both
/// lengths, and not replaced. Catches: a publish that trusts a key whose object is not the
/// bytes it names, or overwrites it.
#[tokio::test]
async fn an_object_of_the_wrong_length_at_its_key_fails() {
    let dir = scratch("base-publish-wrong-length");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    store
        .state()
        .lying
        .insert(key(&sha(TOOL)), TOOL.len() as u64 + 1);

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Length {
        key: failed,
        held,
        want,
    } = &error
    else {
        panic!("not a length refusal: {error}");
    };
    assert_eq!(failed, &key(&sha(TOOL)));
    assert_eq!((*held, *want), (TOOL.len() as u64 + 1, TOOL.len() as u64));
    assert!(!store.state().puts.contains(failed), "not replaced");
}

/// Asserts that objects are written in the bundle's order: every file, then each manifest after
/// the manifests it names, the root's last. Catches: a root written before what it names, which
/// a reader of a half-published tree would follow into a missing object.
#[tokio::test]
async fn the_writes_have_the_root_last() {
    let dir = scratch("base-publish-order");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());

    run(&dir, &store).await.expect("published");

    let want: Vec<String> = release.objects.iter().map(|(hex, _)| key(hex)).collect();
    assert_eq!(store.writes(), want);
    assert_eq!(store.writes().last(), Some(&key(&release.built.root)));
}

/// Asserts that a bundle the sums file does not list is refused by name before anything is read
/// or sent to the store. Catches: a bundle taken on the name alone.
#[tokio::test]
async fn a_bundle_not_listed_in_the_sums_is_refused() {
    let dir = scratch("base-publish-unlisted");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let other = format!("{}  kbf-base-tree-other.tree.tar\n", sha(b"x"));
    std::fs::write(dir.join(SUMS), other).expect("the sums");
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::NotListed { name, .. } = &error else {
        panic!("not an unlisted bundle: {error}");
    };
    assert_eq!(name, &file_name(COMMIT, PLATFORM));
    assert!(store.untouched());
}

/// Asserts that a missing sums file is refused naming it, and a sums file that lists the bundle
/// twice is refused naming the bundle. Catches: a start that publishes with nothing to check the
/// bundle against.
#[tokio::test]
async fn no_sums_file_and_a_double_listing_are_refused() {
    let dir = scratch("base-publish-sums");
    let release = standard(b"kbf-task");
    std::fs::write(dir.join(file_name(COMMIT, PLATFORM)), &release.bytes).expect("the bundle");
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("no sums");
    let PublishError::NoSums { path } = &error else {
        panic!("not a missing sums file: {error}");
    };
    assert_eq!(path, &dir.join(SUMS));

    let line = format!("{}  {}\n", sha(&release.bytes), file_name(COMMIT, PLATFORM));
    std::fs::write(dir.join(SUMS), format!("{line}{line}")).expect("the sums");
    let error = run(&dir, &store).await.expect_err("twice");
    assert!(matches!(error, PublishError::ListedTwice { .. }), "{error}");
    assert!(store.untouched());
}

/// Asserts that a line of the sums file that is not `<sha256>  <name>` and a newline is
/// refused with its number: no final newline, a digest in upper case or too short, one space,
/// a name with a slash, an empty name. Catches: a lenient parse that skips the line that would
/// have listed another digest.
#[tokio::test]
async fn a_malformed_sums_line_is_refused_with_its_number() {
    let dir = scratch("base-publish-sums-lines");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let good = format!("{}  {}\n", sha(b"x"), file_name(COMMIT, "linux-arm64"));
    let store = Arc::new(Bucket::default());
    let digest = sha(b"y");
    for bad in [
        format!("{digest}  name"),
        format!("{}  name\n", digest.to_uppercase()),
        format!("{}  name\n", &digest[1..]),
        format!("{digest} name\n"),
        format!("{digest}  a/name\n"),
        format!("{digest}  \n"),
    ] {
        std::fs::write(dir.join(SUMS), format!("{good}{bad}")).expect("the sums");

        let error = run(&dir, &store).await.expect_err("refused");

        let PublishError::SumsLine { line, .. } = error else {
            panic!("not a malformed line ({bad:?}): {error}");
        };
        assert_eq!(line, 2, "{bad:?}");
    }
    assert!(store.untouched());
}

/// Asserts that a listed bundle that is not in the directory is refused naming its path.
/// Catches: a platform whose bundle the release did not ship starting anyway.
#[tokio::test]
async fn a_missing_bundle_is_refused_by_name() {
    let dir = scratch("base-publish-missing");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    std::fs::remove_file(dir.join(file_name(COMMIT, PLATFORM))).expect("removed");
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::NoBundle { path } = &error else {
        panic!("not a missing bundle: {error}");
    };
    assert_eq!(path, &dir.join(file_name(COMMIT, PLATFORM)));
    assert!(store.untouched());
}

/// Asserts that a bundle whose bytes are not the listed digest is refused, with both digests
/// and nothing read from it: the listed one is a valid bundle's, the file another valid bundle.
/// Catches: a bundle swapped after the release was checked, read because it parses.
#[tokio::test]
async fn a_bundle_that_is_not_the_listed_digest_is_refused() {
    let dir = scratch("base-publish-digest");
    let listed = standard(b"kbf-task");
    let other = standard(b"another kbf-task");
    install_as(
        &dir,
        &file_name(COMMIT, PLATFORM),
        &other.bytes,
        &listed.bytes,
    );
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Digest {
        listed: said,
        actual,
        ..
    } = &error
    else {
        panic!("not a digest refusal: {error}");
    };
    assert_eq!(said, &sha(&listed.bytes));
    assert_eq!(actual, &sha(&other.bytes));
    assert!(store.untouched());
}

/// Asserts that a bundle whose facts are of another commit, or of another platform, than the
/// one asked is refused naming both, before any object is written. Catches: the bundle of
/// another release installed under this release's name.
#[tokio::test]
async fn facts_of_another_commit_or_platform_are_refused() {
    let dir = scratch("base-publish-facts");
    let store = Arc::new(Bucket::default());
    let name = file_name(COMMIT, PLATFORM);

    let other_commit = release(&"f".repeat(40), PLATFORM, b"kbf-task", &[]);
    install_as(&dir, &name, &other_commit.bytes, &other_commit.bytes);
    let error = run(&dir, &store).await.expect_err("another commit");
    let PublishError::Commit { got, want, .. } = &error else {
        panic!("not a commit refusal: {error}");
    };
    assert_eq!(
        (got.as_str(), want.as_str()),
        ("f".repeat(40).as_str(), COMMIT)
    );

    let other_platform = release(COMMIT, "linux-arm64", b"kbf-task", &[]);
    install_as(&dir, &name, &other_platform.bytes, &other_platform.bytes);
    let error = run(&dir, &store).await.expect_err("another platform");
    let PublishError::Platform { got, want, .. } = &error else {
        panic!("not a platform refusal: {error}");
    };
    assert_eq!((got.as_str(), want.as_str()), ("linux-arm64", PLATFORM));
    assert!(store.untouched());
}

/// Asserts that a build that does not know its commit publishes nothing and says so, even with
/// a valid bundle in the directory. Catches: a bundle named `kbf-base-tree--linux-...` looked
/// for, or a stale one taken.
#[tokio::test]
async fn an_empty_commit_is_refused() {
    let dir = scratch("base-publish-no-commit");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());

    let error = publish(&dir, "", PLATFORM, store.clone())
        .await
        .expect_err("refused");

    assert!(matches!(error, PublishError::NoCommit), "{error}");
    assert_eq!(
        error.to_string(),
        "this build does not know its commit, so it has no base tree"
    );
    assert!(store.untouched());
}

/// Asserts that a root that holds a name an action tree keeps (`work`) is refused naming it,
/// and the root manifest is not written: no tree the executor could not use is ever named in the
/// store. Catches: the check left to the first action.
#[tokio::test]
async fn a_root_holding_a_reserved_name_is_refused_and_not_written() {
    let dir = scratch("base-publish-reserved");
    let release = release(COMMIT, PLATFORM, b"kbf-task", &["work"]);
    install(&dir, &release);
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Base { source, .. } = &error else {
        panic!("not a base tree refusal: {error}");
    };
    assert_eq!(source, &BaseTreeError::Reserved("work".to_owned()));
    assert!(store.object(&release.built.root).is_none());
    assert!(!store.state().puts.contains(&key(&release.built.root)));
}

/// Asserts that a bundle whose facts name a root it does not hold is refused naming the root,
/// after its objects. Catches: a start that serves with no base tree.
#[tokio::test]
async fn a_bundle_without_its_root_is_refused() {
    let dir = scratch("base-publish-no-root");
    let release = standard(b"kbf-task");
    let mut facts = release.facts.clone();
    facts.root = "ab".repeat(32);
    let mut writer = Writer::new(Vec::new(), &facts).expect("a bundle");
    writer.object(TOOL).expect("written");
    let bytes = writer.finish().expect("finished");
    install_as(&dir, &file_name(COMMIT, PLATFORM), &bytes, &bytes);
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::NoRoot { root, .. } = &error else {
        panic!("not a missing root: {error}");
    };
    assert_eq!(root, &"ab".repeat(32));
}

/// Asserts that a listed file that is not a bundle (an empty tar; a tar whose first entry is
/// not the facts; bytes that are no tar) is refused with the bundle's own reason and its path,
/// and a bundle that is a directory is refused as unreadable. Catches: a reader's failure
/// reported as a missing root, or swallowed.
#[tokio::test]
async fn a_file_that_is_not_a_bundle_is_refused_with_the_reason() {
    let dir = scratch("base-publish-not-a-bundle");
    let name = file_name(COMMIT, PLATFORM);
    let store = Arc::new(Bucket::default());
    let release = standard(b"kbf-task");

    let empty = vec![0; 1024];
    install_as(&dir, &name, &empty, &empty);
    let error = run(&dir, &store).await.expect_err("empty");
    assert!(
        matches!(
            &error,
            PublishError::Bundle {
                source: kbf_tree_bundle::BundleError::Empty,
                ..
            }
        ),
        "{error}"
    );

    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(1);
    tar.append_data(&mut header, "objects/stray", &b"x"[..])
        .expect("appended");
    let stray = tar.into_inner().expect("tar");
    install_as(&dir, &name, &stray, &stray);
    let error = run(&dir, &store).await.expect_err("stray");
    assert!(
        matches!(
            &error,
            PublishError::Bundle {
                source: kbf_tree_bundle::BundleError::FactsNotFirst(_),
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .starts_with(&dir.join(&name).display().to_string())
    );

    std::fs::remove_file(dir.join(&name)).expect("removed");
    std::fs::create_dir(dir.join(&name)).expect("a directory");
    let sums = format!("{}  {name}\n", sha(&release.bytes));
    std::fs::write(dir.join(SUMS), sums).expect("the sums");
    let error = run(&dir, &store).await.expect_err("a directory");
    assert!(matches!(error, PublishError::Io { .. }), "{error}");
    assert!(store.untouched());
}

/// Asserts that a sums file that cannot be read (a directory) is refused naming its path.
/// Catches: a read error taken for a missing file, or for an empty listing.
#[tokio::test]
async fn an_unreadable_sums_file_is_refused_by_name() {
    let dir = scratch("base-publish-sums-dir");
    std::fs::create_dir(dir.join(SUMS)).expect("a directory");
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    let PublishError::Io { path, .. } = &error else {
        panic!("not an unreadable file: {error}");
    };
    assert_eq!(path, &dir.join(SUMS));
}

/// Asserts that a bundle that is bad part way (an object whose bytes are not its name's) is
/// refused with the bundle's reason, and not the writer's, after the objects before it were
/// written. Catches: a corrupt object published under another's key.
#[tokio::test]
async fn an_object_that_is_not_its_name_is_refused() {
    let dir = scratch("base-publish-bad-object");
    let release = standard(b"kbf-task");
    let name = file_name(COMMIT, PLATFORM);
    let mut tar = tar::Builder::new(Vec::new());
    let facts = serde_json::to_vec(&release.facts).expect("facts");
    let mut header = tar::Header::new_ustar();
    header.set_size(facts.len() as u64);
    tar.append_data(&mut header, "facts.json", facts.as_slice())
        .expect("appended");
    let mut header = tar::Header::new_ustar();
    header.set_size(3);
    tar.append_data(&mut header, format!("objects/{}", sha(b"abc")), &b"abd"[..])
        .expect("appended");
    let bytes = tar.into_inner().expect("tar");
    install_as(&dir, &name, &bytes, &bytes);
    let store = Arc::new(Bucket::default());

    let error = run(&dir, &store).await.expect_err("refused");

    assert!(
        matches!(
            &error,
            PublishError::Bundle {
                source: kbf_tree_bundle::BundleError::Digest { .. },
                ..
            }
        ),
        "{error}"
    );
    assert!(store.untouched());
}
