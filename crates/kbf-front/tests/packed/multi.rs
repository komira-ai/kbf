//! The fixtures of `untrusted_multi`: a frontend with packs, a backend over a storage node that
//! watches how a large blob reaches it, the key set issued for a lease, and the records, manifests
//! and packs of honest runs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_front::{
    Backend, Ingested, IssuedKeySet, IssuedPackKey, MAX_UNTRUSTED_LARGE_BYTES, PackKeyMint,
    RejectReason, UntrustedLimits, ingest_set,
};
use kbf_meta::{ActionRecord, Role};
use kbf_pack::{
    LargeEntry, OutputDirectory, OutputFile, TaskManifest, Timing, Usage, write_pack_with,
};
use kbf_pack_index::PackIndex as _;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{self, ActionResult, GetActionResultRequest};
use kbf_proto::worker::segments_server::Segments;
use kbf_segments::{RecordHasher, sha256};
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use tonic::Code;

use crate::support::{Rig, Spy, blob, update};
use crate::untrusted_task::{LEASE, SECRET, action_bytes, now, segment_of, tree_of, wire};

/// The most bytes of a large object ingest reads at a time.
pub(crate) const WINDOW: u64 = 8 << 20;
/// A large object: over the record cap, not a whole number of windows.
pub(crate) const LARGE_LEN: usize = (64 << 20) + 12_345;

pub(crate) fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// The large object of the tests and its digest, made once.
pub(crate) fn large() -> &'static (Bytes, Digest) {
    static LARGE: OnceLock<(Bytes, Digest)> = OnceLock::new();
    LARGE.get_or_init(|| {
        let bytes = Bytes::from(patterned(LARGE_LEN));
        let digest = sha256(&bytes);
        (bytes, digest)
    })
}

/// The records of an honest run besides its manifest.
pub(crate) struct Parts {
    /// More than the manifest cap, so a pack that starts with it is not a manifest's pack.
    pub(crate) alpha: Bytes,
    pub(crate) beta: Bytes,
    pub(crate) stdout: Bytes,
    pub(crate) in_dir: Bytes,
    pub(crate) tree: (Digest, Bytes),
}

impl Parts {
    pub(crate) fn new() -> Self {
        let in_dir = blob("a file inside the output directory");
        Self {
            alpha: Bytes::from(patterned((5 << 20) + 17)),
            beta: blob("beta object").1,
            stdout: blob("compiling\n").1,
            tree: tree_of(&[in_dir.0]),
            in_dir: in_dir.1,
        }
    }

    /// The digests of every record the run holds.
    pub(crate) fn digests(&self) -> [Digest; 5] {
        [
            sha256(&self.alpha),
            sha256(&self.beta),
            sha256(&self.stdout),
            self.tree.0,
            sha256(&self.in_dir),
        ]
    }
}

/// The digest of every byte a node streams for `digest`.
pub(crate) async fn streamed_by_node(rig: &Rig, digest: &Digest) -> Digest {
    let mut stream = rig
        .node()
        .read_range(digest, 0, 0)
        .await
        .expect("the node streams it");
    let mut hasher = RecordHasher::new();
    while let Some(chunk) = stream.next().await {
        hasher.update(&chunk.expect("a chunk"));
    }
    hasher.finish().0
}

/// What the storage node was told of a large blob: the pieces, the largest, and how many reads of
/// the large object the object store had answered by the time each piece arrived beyond the pieces
/// taken (a read-ahead is a buffer).
pub(crate) struct Notes {
    pub(crate) store: Arc<Spy>,
    pub(crate) large_key: String,
    pub(crate) pieces: AtomicUsize,
    pub(crate) largest: AtomicUsize,
    pub(crate) ahead: AtomicUsize,
}

impl Notes {
    pub(crate) fn reads(&self) -> usize {
        let ranges = self.store.ranges();
        ranges
            .iter()
            .filter(|(k, _, _)| *k == self.large_key)
            .count()
    }

    pub(crate) fn note(&self, len: usize) {
        let taken = self.pieces.fetch_add(1, Ordering::SeqCst) + 1;
        self.largest.fetch_max(len, Ordering::SeqCst);
        let ahead = self.reads().saturating_sub(taken);
        self.ahead.fetch_max(ahead, Ordering::SeqCst);
    }
}

/// A storage node that notes how a large blob reaches it, and can be told to refuse one.
pub(crate) struct Watching {
    pub(crate) inner: Arc<dyn Storage>,
    pub(crate) notes: Arc<Notes>,
    pub(crate) refuse: AtomicBool,
    /// Reads the whole blob, then fails.
    pub(crate) refuse_after: AtomicBool,
    /// Takes the blob's last byte and stops polling for more, as a client that sends the end of
    /// the write with the last chunk does, then fails.
    pub(crate) stop_at_size: AtomicBool,
}

impl Storage for Watching {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(&'a self, d: &'a Digest, offset: u64, limit: u64) -> Answer<'a, ReadStream> {
        self.inner.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        if self.refuse.load(Ordering::SeqCst) {
            let down = std::io::Error::other("the node is down");
            return Box::pin(async move { Err(StorageError::Unavailable(Arc::new(down))) });
        }
        if self.stop_at_size.load(Ordering::SeqCst) {
            return Box::pin(async move {
                let mut bytes = bytes;
                let mut got = 0;
                while got < size {
                    let Some(Ok(piece)) = bytes.next().await else {
                        break;
                    };
                    got += piece.len() as u64;
                }
                drop(bytes);
                let down = std::io::Error::other("the stream was left after its last byte");
                Err(StorageError::Unavailable(Arc::new(down)))
            });
        }
        if self.refuse_after.load(Ordering::SeqCst) {
            return Box::pin(async move {
                let mut bytes = bytes;
                while bytes.next().await.is_some() {}
                let down = std::io::Error::other("the node went down at the end");
                Err(StorageError::Unavailable(Arc::new(down)))
            });
        }
        let notes = Arc::clone(&self.notes);
        let noted = bytes
            .inspect(move |piece| {
                if let Ok(piece) = piece {
                    notes.note(piece.len());
                }
            })
            .boxed();
        self.inner.put_streamed(size, noted, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(&'a self, a: &'a Digest) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(r)
    }

    fn commit_action_record(&self, role: Role, a: Digest, r: ActionRecord) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, a, r)
    }
}

/// The frontend with packs, a backend over a storage node that watches, the Action of the lease and
/// the key set issued for it: three pack keys and two large keys unless said.
pub(crate) struct Multi {
    pub(crate) rig: Rig,
    pub(crate) backend: Backend,
    pub(crate) mint: PackKeyMint,
    pub(crate) action: Digest,
    pub(crate) keys: IssuedKeySet,
    pub(crate) watching: Arc<Watching>,
    pub(crate) p: Parts,
}

impl Multi {
    pub(crate) async fn new() -> Self {
        Self::sized(3, 2).await
    }

    pub(crate) async fn sized(packs: u8, large_keys: u8) -> Self {
        let rig = Rig::new().await;
        let bytes = action_bytes(false);
        let action = sha256(&bytes);
        assert_eq!(update(&rig.front, &[(action, bytes)]).await, [0]);
        let mint = PackKeyMint::new(SECRET).expect("a mint");
        let minted = mint
            .mint_set(LEASE, &action, now(), packs, large_keys)
            .expect("keys");
        let keys = IssuedKeySet::from_minted(minted, usize::from(packs));
        let large_key = keys.large().first().map(|k| k.key().to_owned());
        let notes = Arc::new(Notes {
            store: Arc::clone(&rig.store),
            large_key: large_key.unwrap_or_default(),
            pieces: AtomicUsize::new(0),
            largest: AtomicUsize::new(0),
            ahead: AtomicUsize::new(0),
        });
        let watching = Arc::new(Watching {
            inner: Arc::clone(rig.node()),
            notes,
            refuse: AtomicBool::new(false),
            refuse_after: AtomicBool::new(false),
            stop_at_size: AtomicBool::new(false),
        });
        let node = Arc::clone(&watching) as Arc<dyn Storage>;
        let backend = Backend::remote_over(node, Some(rig.packs.clone()));
        Self {
            rig,
            backend,
            mint,
            action,
            keys,
            watching,
            p: Parts::new(),
        }
    }

    /// The manifest of an honest run of `packs` packs whose large objects are `huge` (slot and
    /// digest), each named by an output file of its own.
    pub(crate) fn manifest(&self, packs: u8, huge: &[(u8, Digest)]) -> TaskManifest {
        let file = |path: String, digest: Digest| OutputFile {
            path,
            digest,
            is_executable: false,
        };
        let mut output_files = vec![
            file("out/alpha".to_owned(), sha256(&self.p.alpha)),
            file("out/beta".to_owned(), sha256(&self.p.beta)),
        ];
        for (slot, digest) in huge {
            output_files.push(file(format!("out/big{slot}"), *digest));
        }
        TaskManifest {
            lease: LEASE.to_owned(),
            action: self.action,
            exit_code: 0,
            stdout: Some(sha256(&self.p.stdout)),
            stderr: None,
            output_files,
            output_directories: vec![OutputDirectory {
                path: "out/d".to_owned(),
                tree: self.p.tree.0,
            }],
            output_symlinks: Vec::new(),
            timing: Timing::default(),
            usage: Usage::default(),
            packs,
            large: huge
                .iter()
                .map(|&(slot, digest)| LargeEntry { slot, digest })
                .collect(),
            inputs: None,
        }
    }

    /// Pack 0 of `manifest` holding `records` after it.
    pub(crate) fn pack_zero(manifest: &TaskManifest, records: &[&[u8]]) -> Vec<u8> {
        write_pack_with(manifest, records, MAX_UNTRUSTED_LARGE_BYTES).expect("a pack")
    }

    /// The manifest and the two packs of an honest run of two packs: the tree, `beta` and the
    /// standard output in pack 0; `alpha` (first, and over the manifest cap) and the file of the
    /// tree in pack 1.
    pub(crate) fn two_packs(&self) -> (TaskManifest, Vec<u8>, Vec<u8>) {
        let (p, manifest) = (&self.p, self.manifest(2, &[]));
        let zero = Self::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
        let one = segment_of(&[&p.alpha, &p.in_dir]);
        (manifest, zero, one)
    }

    /// The manifest and pack 0 of an honest run of one pack and the large object in slot 0.
    pub(crate) fn large_run(&self) -> (TaskManifest, Vec<u8>) {
        let p = &self.p;
        let manifest = self.manifest(1, &[(0, large().1)]);
        let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
        (manifest.clone(), Self::pack_zero(&manifest, &records))
    }

    pub(crate) fn pack_key(&self, at: usize) -> &IssuedPackKey {
        &self.keys.packs()[at]
    }

    pub(crate) fn large_key(&self, at: usize) -> &IssuedPackKey {
        &self.keys.large()[at]
    }

    pub(crate) async fn put(&self, key: &IssuedPackKey, bytes: &[u8]) {
        self.rig.store.seed(key.key(), bytes).await;
    }

    pub(crate) async fn run_with(&self, keys: &IssuedKeySet, limits: &UntrustedLimits) -> Ingested {
        let (packs, backend) = (&self.rig.packs, &self.backend);
        let mint = &self.mint;
        ingest_set(packs, backend, mint, LEASE, keys, &self.action, limits).await
    }

    pub(crate) async fn run(&self) -> Ingested {
        self.run_with(&self.keys, &UntrustedLimits::default()).await
    }

    /// What the frontend's GetActionResult answers for the action.
    pub(crate) async fn cached(&self) -> Result<ActionResult, tonic::Status> {
        let request = GetActionResultRequest {
            action_digest: Some(wire(&self.action)),
            ..GetActionResultRequest::default()
        };
        let mut client = ActionCacheClient::new(self.rig.front.clone());
        Ok(client.get_action_result(request).await?.into_inner())
    }

    /// Asserts that the run was refused for `reason`: counted under it and no other, nothing
    /// cached.
    pub(crate) async fn rejected(&self, got: Ingested, reason: RejectReason) {
        assert_eq!(got, Ingested::Reject(reason));
        let metrics = self.rig.packs.metrics();
        for each in RejectReason::ALL {
            let want = u64::from(each == reason);
            assert_eq!(metrics.ingest_rejects(each), want, "{each:?}");
        }
        assert_eq!(metrics.ingests(), 0);
        let cached = self.cached().await.expect_err("nothing cached");
        assert_eq!(cached.code(), Code::NotFound);
    }

    /// Asserts that the index has no row for any pack of the set and none of the run's records.
    pub(crate) fn nothing_indexed(&self) {
        assert!(self.rig.index.has_none(&self.p.digests()));
        for key in self.keys.packs() {
            assert!(self.rig.index.pack(key.key()).expect("asked").is_none());
        }
    }
}

/// The result an honest manifest describes: sorted by path, no metadata.
pub(crate) fn result_of(manifest: &TaskManifest) -> ActionResult {
    let mut files: Vec<reapi::OutputFile> = manifest
        .output_files
        .iter()
        .map(|file| reapi::OutputFile {
            path: file.path.clone(),
            digest: Some(wire(&file.digest)),
            is_executable: file.is_executable,
            ..reapi::OutputFile::default()
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let directory = |dir: &OutputDirectory| reapi::OutputDirectory {
        path: dir.path.clone(),
        tree_digest: Some(wire(&dir.tree)),
        ..reapi::OutputDirectory::default()
    };
    ActionResult {
        output_files: files,
        output_directories: manifest.output_directories.iter().map(directory).collect(),
        stdout_digest: manifest.stdout.as_ref().map(wire),
        exit_code: manifest.exit_code,
        ..ActionResult::default()
    }
}
