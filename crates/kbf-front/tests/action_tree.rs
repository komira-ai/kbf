//! The action tree's manifest writer (`kbf_front::action_tree`): the manifests it writes, read
//! back with tierstore's own reader at kbf's pin; where it finds blobs and how it writes the
//! ones that are nowhere; the memo; and every refusal, by name.
//!
//! The input closure is the real one: Directory, file, Action and Command blobs are stored in a
//! storage node (a map of blobs) and collected by `collect_closure_keeping`. The pack index, the
//! presence of `cas/l/` objects and the store's create-only writes are a fake ([`Farm`]) that
//! records every ask and write and fails or hangs on request; a blob to be written is read from
//! the real node. Time is a test clock; a lookup that hangs ends at the writer's deadline on
//! tokio's paused clock, and no test reads the wall clock or waits.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream;
use kbf_front::action_tree::{ActionTree, BaseFacts, BaseTree, NoTree, TreeWriter};
use kbf_front::job_read::{BlobSource, Span};
use kbf_front::large_backfill::LargeStore;
use kbf_front::prepare_run::{ClosureLimits, KeptClosure, collect_closure_keeping};
use kbf_front::{Backend, Cache, CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_proto::reapi::ActionResult;
use kbf_proto::reapi::{self, Directory, DirectoryNode, FileNode, SymlinkNode};
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tierstore::{TreeDigest, TreeEntry, flatten};

#[path = "action_tree/golden.rs"]
mod golden;
#[path = "action_tree/kept.rs"]
mod kept;
#[path = "action_tree/limits.rs"]
mod limits;
#[path = "action_tree/memo.rs"]
mod memo;
#[path = "action_tree/store.rs"]
mod store;

/// The time the test clock starts at.
const START_SECS: u64 = 1_790_000_000;

/// How the fake answers one kind of call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    Fail,
    Hang,
}

/// What the fake is told to do wrong.
#[derive(Default)]
struct Faults {
    /// The pack index's answers.
    index: Fault,
    /// The index answers for all but the last digest of each ask.
    index_short: bool,
    /// The presence checks of `cas/l/`.
    head: Fault,
    /// A blob whose read fails before any byte.
    read: Option<Digest>,
    /// A blob whose read fails after its first chunk.
    read_broken: Option<Digest>,
    /// A key whose write the store refuses.
    put: Option<String>,
}

#[derive(Default)]
struct State {
    /// The pack index: where each packed blob is.
    spans: BTreeMap<Digest, Span>,
    /// The objects of the store, by key, with their bytes.
    objects: BTreeMap<String, Bytes>,
    /// Objects the store holds whose bytes no test reads: their lengths.
    sized: BTreeMap<String, u64>,
    /// Keys a presence check does not see but a create-only write finds taken.
    late: BTreeSet<String>,
    /// Bytes served for a blob instead of its own.
    wrong: BTreeMap<Digest, Bytes>,
    faults: Faults,
    /// Every ask of the index, the digests it named.
    asks: Vec<Vec<Digest>>,
    /// Every presence check, by blob.
    heads: Vec<Digest>,
    /// Every blob read to be written.
    reads: Vec<Digest>,
    /// Every write, by key, in the order made.
    puts: Vec<String>,
    /// The bytes a write whose body ended in an error had received, by key.
    received: BTreeMap<String, u64>,
}

/// A storage node's blobs in a map, so that a tree of a hundred thousand files is stored in
/// moments; what the writer and the closure never call goes to a real in-memory node.
struct Blobs {
    blobs: Mutex<BTreeMap<Digest, Bytes>>,
    inner: Arc<dyn Storage>,
}

impl Storage for Blobs {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let missing = digests
            .iter()
            .filter(|d| !blobs.contains_key(d))
            .copied()
            .collect();
        Box::pin(async move { Ok(missing) })
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        let blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        let found = blobs
            .get(digest)
            .cloned()
            .ok_or(StorageError::NotFound(*digest));
        Box::pin(async move { found })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.inner.read_range(digest, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        let digest = kbf_segments::sha256(&bytes);
        let mut blobs = self.blobs.lock().unwrap_or_else(PoisonError::into_inner);
        blobs.insert(digest, bytes);
        Box::pin(async move { Ok(digest) })
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(action)
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// The pack index, `cas/l/` and the store's writes, over a storage node.
struct Farm {
    backend: Backend,
    storage: Arc<dyn Storage>,
    state: Mutex<State>,
}

/// The key of `digest`'s whole object.
fn whole_key(hex: &str) -> String {
    format!("cas/l/{hex}")
}

impl Farm {
    fn new() -> Arc<Self> {
        let storage: Arc<dyn Storage> = Arc::new(Blobs {
            blobs: Mutex::new(BTreeMap::new()),
            inner: Arc::new(CacheStorage::new(
                Arc::new(Cache::memory()),
                SegmentsConfig::DEFAULT,
            )),
        });
        Arc::new(Self {
            backend: Backend::remote(Arc::clone(&storage)),
            storage,
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores `bytes` in the CAS (the storage node), and nowhere a tree can name.
    async fn cas(&self, bytes: impl Into<Bytes>) -> Digest {
        self.storage.put(bytes.into()).await.expect("stored")
    }

    /// Stores `bytes` in the CAS and records them in a pack at `key`, `offset`.
    async fn packed(&self, bytes: impl Into<Bytes>, key: &str, offset: u64) -> Digest {
        let digest = self.cas(bytes).await;
        self.span(digest, key, offset);
        digest
    }

    /// Records `digest` in the pack index.
    fn span(&self, digest: Digest, key: &str, offset: u64) {
        let span = Span {
            key: key.to_owned(),
            offset,
            length: digest.size_bytes,
        };
        self.state().spans.insert(digest, span);
    }

    /// Stores `bytes` in the CAS and as their whole object in `cas/l/`.
    async fn whole(&self, bytes: impl Into<Bytes>) -> Digest {
        let bytes = bytes.into();
        let digest = self.cas(bytes.clone()).await;
        let key = whole_key(&digest.hash_hex());
        self.state().objects.insert(key, bytes);
        digest
    }

    /// The object at `key`, if any.
    fn object(&self, key: &str) -> Option<Bytes> {
        self.state().objects.get(key).cloned()
    }

    /// The text of the manifest of digest `hex`.
    fn manifest(&self, hex: &str) -> String {
        let bytes = self
            .object(&whole_key(hex))
            .expect("the manifest is stored");
        String::from_utf8(bytes.to_vec()).expect("a manifest is text")
    }

    /// The writes made so far, by key.
    fn puts(&self) -> Vec<String> {
        self.state().puts.clone()
    }

    /// The writes made of `hex`'s key so far.
    fn puts_of(&self, hex: &str) -> usize {
        let key = whole_key(hex);
        self.state().puts.iter().filter(|put| **put == key).count()
    }

    /// Every digest the index was asked for so far.
    fn asked(&self) -> BTreeSet<Digest> {
        self.state().asks.iter().flatten().copied().collect()
    }

    /// Forgets what was asked and written so far.
    fn clear_records(&self) {
        let mut state = self.state();
        state.asks.clear();
        state.heads.clear();
        state.reads.clear();
        state.puts.clear();
    }

    /// The tree `tree` flattened by tierstore's reader from the stored manifests.
    fn flatten(&self, tree: &ActionTree) -> Result<Vec<TreeEntry>, tierstore::TreeError> {
        let objects = self.state().objects.clone();
        flatten(tree.digest(), |digest: &TreeDigest| {
            objects
                .get(&whole_key(&digest.to_string()))
                .map(|bytes| bytes.to_vec())
                .ok_or("no such manifest")
        })
    }
}

/// What a fault makes a call answer, once it has been recorded.
async fn fault(fault: Fault) -> Result<(), StorageError> {
    match fault {
        Fault::None => Ok(()),
        Fault::Fail => Err(StorageError::Unavailable(Arc::new(std::io::Error::other(
            "the index is down",
        )))),
        Fault::Hang => futures::future::pending().await,
    }
}

impl BlobSource for Farm {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(self.backend.read_blob(digest))
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move {
            let (refused, broken, wrong) = {
                let mut state = self.state();
                state.reads.push(*digest);
                let faults = &state.faults;
                (
                    faults.read == Some(*digest),
                    faults.read_broken == Some(*digest),
                    state.wrong.get(digest).cloned(),
                )
            };
            if refused {
                return Err(StorageError::NotFound(*digest));
            }
            let bytes = match wrong {
                Some(bytes) => bytes,
                None => self.backend.read_blob(digest).await?,
            };
            // Three bytes a chunk, so a blob is many chunks.
            let mut chunks: Vec<Result<Bytes, StorageError>> = (0..bytes.len())
                .step_by(3)
                .map(|at| Ok(bytes.slice(at..(at + 3).min(bytes.len()))))
                .collect();
            if broken {
                chunks.truncate(1);
                chunks.push(Err(StorageError::Internal("the read broke".to_owned())));
            }
            Ok(stream::iter(chunks).boxed())
        })
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        Box::pin(async move {
            let (index, short, mut spans) = {
                let mut state = self.state();
                state.asks.push(digests.to_vec());
                let spans: Vec<Option<Span>> = digests
                    .iter()
                    .map(|d| state.spans.get(d).cloned())
                    .collect();
                (state.faults.index, state.faults.index_short, spans)
            };
            fault(index).await?;
            if short {
                spans.pop();
            }
            Ok(spans)
        })
    }

    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        Box::pin(async move {
            let (head, length) = {
                let mut state = self.state();
                state.heads.push(*digest);
                let key = whole_key(&digest.hash_hex());
                let length = state.objects.get(&key).map(|b| b.len() as u64);
                (
                    state.faults.head,
                    length.or_else(|| state.sized.get(&key).copied()),
                )
            };
            fault(head).await?;
            Ok(length)
        })
    }
}

impl LargeStore for Farm {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        let length = self.object(key.as_str()).map(|bytes| bytes.len() as u64);
        Box::pin(async move { Ok(length) })
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let name = key.as_str().to_owned();
            let refused = {
                let mut state = self.state();
                state.puts.push(name.clone());
                state.faults.put.as_deref() == Some(name.as_str())
            };
            if refused {
                return Err(ObjectStoreError::Unsupported("writes to this key"));
            }
            let mut got = Vec::new();
            while let Some(piece) = body.next().await {
                match piece {
                    Ok(piece) => got.extend_from_slice(&piece),
                    Err(error) => {
                        self.state().received.insert(name, got.len() as u64);
                        return Err(error);
                    }
                }
            }
            let mut state = self.state();
            if state.objects.contains_key(&name) || state.late.contains(&name) {
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            assert_eq!(got.len() as u64, len, "a body of the length declared");
            state.objects.insert(name, Bytes::from(got));
            Ok(())
        })
    }
}

/// A file node.
fn file(name: &str, digest: &Digest, exec: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(kbf_front::digest_to_proto(digest)),
        is_executable: exec,
        ..FileNode::default()
    }
}

/// A directory node.
fn subdir(name: &str, digest: &Digest) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(kbf_front::digest_to_proto(digest)),
    }
}

/// A symlink node.
fn link(name: &str, target: &str) -> SymlinkNode {
    SymlinkNode {
        name: name.to_owned(),
        target: target.to_owned(),
        ..SymlinkNode::default()
    }
}

/// A Directory of these nodes, in the order given.
fn directory(files: Vec<FileNode>, dirs: Vec<DirectoryNode>, links: Vec<SymlinkNode>) -> Directory {
    Directory {
        files,
        directories: dirs,
        symlinks: links,
        ..Directory::default()
    }
}

/// The kbf base tree of the tests, published as its manifests: `usr` (a file), `.kbf-tools`
/// (`kbf-task`), the symlink `bin` and the file `README`, whose bytes are a span of a pack.
struct Base {
    tree: BaseTree,
    /// The root manifest's bytes.
    root: Vec<u8>,
    /// The digests of the base's manifests `usr` and `.kbf-tools`, and their lengths.
    usr: String,
    usr_len: usize,
    tools: String,
    tools_len: usize,
}

/// A SHA-256 of a base file's contents (the base's file blobs are never read).
const BASE_FILE: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

impl Base {
    /// The base, its manifests stored in `farm`.
    fn publish(farm: &Farm) -> Self {
        let usr = format!(
            r#"{{"v":1,"children":[{{"name":"cc","kind":"file","sha256":"{BASE_FILE}","size":5,"exec":true}}]}}"#
        );
        let tools = format!(
            r#"{{"v":1,"children":[{{"name":"kbf-task","kind":"file","sha256":"{BASE_FILE}","size":5,"exec":true}}]}}"#
        );
        let usr_hex = TreeDigest::of(usr.as_bytes()).to_string();
        let tools_hex = TreeDigest::of(tools.as_bytes()).to_string();
        let root = format!(
            concat!(
                r#"{{"v":1,"children":["#,
                r#"{{"name":".kbf-tools","kind":"dir","sha256":"{tools}","size":{tools_len}}},"#,
                r#"{{"name":"README","kind":"file","sha256":"{file}","size":5,"exec":false,"source":{{"key":"cas/f/base/p0","offset":3}}}},"#,
                r#"{{"name":"bin","kind":"symlink","target":"usr/bin"}},"#,
                r#"{{"name":"usr","kind":"dir","sha256":"{usr}","size":{usr_len}}}"#,
                r#"]}}"#
            ),
            tools = tools_hex,
            tools_len = tools.len(),
            file = BASE_FILE,
            usr = usr_hex,
            usr_len = usr.len(),
        );
        let root_hex = TreeDigest::of(root.as_bytes()).to_string();
        let mut state = farm.state();
        for (hex, text) in [(&usr_hex, &usr), (&tools_hex, &tools), (&root_hex, &root)] {
            state
                .objects
                .insert(whole_key(hex), Bytes::from(text.clone().into_bytes()));
        }
        drop(state);
        let tree = BaseTree::new(root.as_bytes(), Self::FACTS).expect("a base tree");
        Self {
            tree,
            root: root.into_bytes(),
            usr: usr_hex,
            usr_len: usr.len(),
            tools: tools_hex,
            tools_len: tools.len(),
        }
    }

    /// The base's entries (its root's four children and one below each of its two
    /// directories) and its depth.
    const FACTS: BaseFacts = BaseFacts {
        entries: 6,
        depth: 2,
    };

    /// This base with other facts.
    fn with_facts(&self, facts: BaseFacts) -> BaseTree {
        BaseTree::new(&self.root, facts).expect("a base tree")
    }
}

/// A writer over a [`Farm`], a test clock and the test base.
struct Rig {
    farm: Arc<Farm>,
    clock: Arc<Mutex<SystemTime>>,
    writer: TreeWriter,
    base: Base,
    /// The Command every Action of the rig names.
    command: Digest,
}

impl Rig {
    async fn new() -> Self {
        Self::with_memo(None).await
    }

    /// A rig whose memo holds at most `entries`, or the writer's own bound.
    async fn with_memo(entries: Option<usize>) -> Self {
        let farm = Farm::new();
        let clock = Arc::new(Mutex::new(UNIX_EPOCH + Duration::from_secs(START_SECS)));
        let now = Arc::clone(&clock);
        let source = Arc::clone(&farm) as Arc<dyn BlobSource>;
        let store = Arc::clone(&farm) as Arc<dyn LargeStore>;
        let clock_fn: kbf_front::Clock =
            Arc::new(move || *now.lock().unwrap_or_else(PoisonError::into_inner));
        let writer = match entries {
            Some(entries) => TreeWriter::with_memo_entries(source, store, clock_fn, entries),
            None => TreeWriter::new(source, store, clock_fn),
        };
        let base = Base::publish(&farm);
        let command = reapi::Command {
            arguments: vec!["true".to_owned()],
            ..reapi::Command::default()
        };
        let command = farm.packed(command.encode_to_vec(), "cas/f/w/p9", 0).await;
        Self {
            farm,
            clock,
            writer,
            base,
            command,
        }
    }

    /// Moves the test clock on by `by`.
    fn advance(&self, by: Duration) {
        *self.clock.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }

    /// Stores the Directory `message` in the CAS; the empty Directory is the empty blob, which
    /// every store holds.
    async fn store(&self, message: &Directory) -> Digest {
        let bytes = message.encode_to_vec();
        if bytes.is_empty() {
            return kbf_segments::sha256(&[]);
        }
        self.farm.cas(bytes).await
    }

    /// The Action that runs the rig's Command over `root`, stored and packed.
    async fn action(&self, root: &Digest) -> Digest {
        let action = reapi::Action {
            command_digest: Some(kbf_front::digest_to_proto(&self.command)),
            input_root_digest: Some(kbf_front::digest_to_proto(root)),
            ..reapi::Action::default()
        };
        self.farm
            .packed(action.encode_to_vec(), "cas/f/w/p9", 100)
            .await
    }

    /// The closure of an Action over `root`, its Directories kept.
    async fn kept(&self, root: &Digest) -> KeptClosure {
        let action = self.action(root).await;
        collect_closure_keeping(&self.farm.backend, &action, ClosureLimits::DEFAULT)
            .await
            .expect("a closure")
    }

    /// The action tree of an Action over `root`, over the rig's base.
    async fn write(&self, root: &Digest) -> Result<ActionTree, NoTree> {
        self.write_over(root, &self.base.tree).await
    }

    /// The action tree of an Action over `root`, over `base`.
    async fn write_over(&self, root: &Digest, base: &BaseTree) -> Result<ActionTree, NoTree> {
        let kept = self.kept(root).await;
        self.writer.write(&kept, base).await
    }
}
