//! `ingest_set`: a run of several packs and large objects, read from the object store under the
//! keys of an `IssuedKeySet`. Pack 0 holds the manifest; packs 1.. hold more records; a large
//! object is a raw object under an `L<i>` key that is streamed through SHA-256 into the CAS, a
//! window at a time, and never held whole. The action-cache entry is written last.
//!
//! The fixtures are those of `untrusted_task` (the store logs every range it is asked for, the
//! index is in memory, the action cache is a real storage node read back through the frontend).
//! Every test says the behaviour it asserts.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_front::{
    Backend, Ingested, IssuedKeySet, IssuedPackKey, MAX_UNTRUSTED_LARGE_BYTES, PackKeyMint,
    RejectReason, UntrustedLimits, ingest, ingest_set,
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
use crate::untrusted_task::{
    LEASE, SECRET, action_bytes, entry_of, forged, now, segment_of, tree_of, wire,
};

/// The most bytes of a large object ingest reads at a time.
const WINDOW: u64 = 8 << 20;
/// A large object: over the record cap, not a whole number of windows.
const LARGE_LEN: usize = (64 << 20) + 12_345;

fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// The large object of the tests and its digest, made once.
fn large() -> &'static (Bytes, Digest) {
    static LARGE: OnceLock<(Bytes, Digest)> = OnceLock::new();
    LARGE.get_or_init(|| {
        let bytes = Bytes::from(patterned(LARGE_LEN));
        let digest = sha256(&bytes);
        (bytes, digest)
    })
}

/// The records of an honest run besides its manifest.
struct Parts {
    /// More than the manifest cap, so a pack that starts with it is not a manifest's pack.
    alpha: Bytes,
    beta: Bytes,
    stdout: Bytes,
    in_dir: Bytes,
    tree: (Digest, Bytes),
}

impl Parts {
    fn new() -> Self {
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
    fn digests(&self) -> [Digest; 5] {
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
async fn streamed_by_node(rig: &Rig, digest: &Digest) -> Digest {
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
struct Notes {
    store: Arc<Spy>,
    large_key: String,
    pieces: AtomicUsize,
    largest: AtomicUsize,
    ahead: AtomicUsize,
}

impl Notes {
    fn reads(&self) -> usize {
        let ranges = self.store.ranges();
        ranges
            .iter()
            .filter(|(k, _, _)| *k == self.large_key)
            .count()
    }

    fn note(&self, len: usize) {
        let taken = self.pieces.fetch_add(1, Ordering::SeqCst) + 1;
        self.largest.fetch_max(len, Ordering::SeqCst);
        let ahead = self.reads().saturating_sub(taken);
        self.ahead.fetch_max(ahead, Ordering::SeqCst);
    }
}

/// A storage node that notes how a large blob reaches it, and can be told to refuse one.
struct Watching {
    inner: Arc<dyn Storage>,
    notes: Arc<Notes>,
    refuse: AtomicBool,
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
struct Multi {
    rig: Rig,
    backend: Backend,
    mint: PackKeyMint,
    action: Digest,
    keys: IssuedKeySet,
    watching: Arc<Watching>,
    p: Parts,
}

impl Multi {
    async fn new() -> Self {
        Self::sized(3, 2).await
    }

    async fn sized(packs: u8, large_keys: u8) -> Self {
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
    fn manifest(&self, packs: u8, huge: &[(u8, Digest)]) -> TaskManifest {
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
    fn pack_zero(manifest: &TaskManifest, records: &[&[u8]]) -> Vec<u8> {
        write_pack_with(manifest, records, MAX_UNTRUSTED_LARGE_BYTES).expect("a pack")
    }

    /// The manifest and the two packs of an honest run of two packs: the tree, `beta` and the
    /// standard output in pack 0; `alpha` (first, and over the manifest cap) and the file of the
    /// tree in pack 1.
    fn two_packs(&self) -> (TaskManifest, Vec<u8>, Vec<u8>) {
        let (p, manifest) = (&self.p, self.manifest(2, &[]));
        let zero = Self::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
        let one = segment_of(&[&p.alpha, &p.in_dir]);
        (manifest, zero, one)
    }

    /// The manifest and pack 0 of an honest run of one pack and the large object in slot 0.
    fn large_run(&self) -> (TaskManifest, Vec<u8>) {
        let p = &self.p;
        let manifest = self.manifest(1, &[(0, large().1)]);
        let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
        (manifest.clone(), Self::pack_zero(&manifest, &records))
    }

    fn pack_key(&self, at: usize) -> &IssuedPackKey {
        &self.keys.packs()[at]
    }

    fn large_key(&self, at: usize) -> &IssuedPackKey {
        &self.keys.large()[at]
    }

    async fn put(&self, key: &IssuedPackKey, bytes: &[u8]) {
        self.rig.store.seed(key.key(), bytes).await;
    }

    async fn run_with(&self, keys: &IssuedKeySet, limits: &UntrustedLimits) -> Ingested {
        let (packs, backend) = (&self.rig.packs, &self.backend);
        let mint = &self.mint;
        ingest_set(packs, backend, mint, LEASE, keys, &self.action, limits).await
    }

    async fn run(&self) -> Ingested {
        self.run_with(&self.keys, &UntrustedLimits::default()).await
    }

    /// What the frontend's GetActionResult answers for the action.
    async fn cached(&self) -> Result<ActionResult, tonic::Status> {
        let request = GetActionResultRequest {
            action_digest: Some(wire(&self.action)),
            ..GetActionResultRequest::default()
        };
        let mut client = ActionCacheClient::new(self.rig.front.clone());
        Ok(client.get_action_result(request).await?.into_inner())
    }

    /// Asserts that the run was refused for `reason`: counted under it and no other, nothing
    /// cached.
    async fn rejected(&self, got: Ingested, reason: RejectReason) {
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
    fn nothing_indexed(&self) {
        assert!(self.rig.index.has_none(&self.p.digests()));
        for key in self.keys.packs() {
            assert!(self.rig.index.pack(key.key()).expect("asked").is_none());
        }
    }
}

/// The result an honest manifest describes: sorted by path, no metadata.
fn result_of(manifest: &TaskManifest) -> ActionResult {
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

/// Asserts that a run of two packs is ingested into exactly the result its manifest describes and
/// cached: the tree is read from the pack that holds it and the file it names is in the other, every
/// record of both packs and both packs are in the index, an unused key is not, and the counts say
/// two packs. Pack 1 starts with a record over the manifest cap, which only a pack without a
/// manifest may do.
#[tokio::test]
async fn a_two_pack_run_is_ingested_and_cached() {
    let farm = Multi::new().await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &one).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert_eq!(farm.cached().await.expect("a hit"), result_of(&manifest));
    assert!(farm.rig.index.has_all(&farm.p.digests()));
    for key in &farm.keys.packs()[..2] {
        assert!(farm.rig.index.pack(key.key()).expect("asked").is_some());
    }
    let unused = farm.pack_key(2).key();
    assert!(farm.rig.index.pack(unused).expect("asked").is_none());
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingests(), 1);
    assert_eq!(metrics.ingest_packs(), 2);
    assert_eq!(metrics.ingest_large_bytes(), 0);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    assert!(
        scrape.contains("kbf_pack_ingest_packs_total 2\n"),
        "{scrape}"
    );
    assert!(
        scrape.contains("kbf_pack_ingest_large_bytes_total 0\n"),
        "{scrape}"
    );
}

/// Asserts that a run of one pack ingested through the set of one key is what `ingest` makes of
/// it: the same result and the cache entry, one pack counted. Catches a set that changes what a run
/// of one pack is.
#[tokio::test]
async fn a_run_of_one_pack_through_a_set_of_one_key_is_what_ingest_makes_of_it() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    let zero = Multi::pack_zero(&manifest, &records);
    farm.put(farm.pack_key(0), &zero).await;
    let set = IssuedKeySet::single(farm.pack_key(0).clone());

    let through_set = farm.run_with(&set, &UntrustedLimits::default()).await;
    let (packs, backend, mint) = (&farm.rig.packs, &farm.backend, &farm.mint);
    let limits = UntrustedLimits::default();
    let key = farm.pack_key(0);
    let plain = ingest(packs, backend, mint, LEASE, key, &farm.action, &limits).await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(through_set, want);
    assert_eq!(plain, want);
    assert_eq!(farm.rig.packs.metrics().ingest_packs(), 2);
}

/// Asserts that a pack 1 the store does not hold is `Incomplete`, not `Missing` (pack 0 is there),
/// and that nothing is indexed, not even pack 0, and nothing is cached.
#[tokio::test]
async fn a_missing_second_pack_is_incomplete() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that a manifest that names more packs than the set holds is a `BadManifest` and that
/// the store was asked about pack 0 only: a pack the frontend did not issue a key for is never
/// read.
#[tokio::test]
async fn a_pack_beyond_the_issued_count_is_a_bad_manifest() {
    let farm = Multi::sized(2, 2).await;
    let p = &farm.p;
    let manifest = farm.manifest(3, &[]);
    let zero = Multi::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    let first = farm.pack_key(0).key();
    assert!(
        farm.rig
            .store
            .ranges()
            .iter()
            .all(|(key, _, _)| key == first)
    );
}

/// Asserts that a large entry in a slot the set holds no key for is a `BadManifest`, and that no
/// large object was asked about.
#[tokio::test]
async fn a_large_slot_beyond_the_issued_count_is_a_bad_manifest() {
    let farm = Multi::sized(3, 1).await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[(1, large().1)]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(farm.pack_key(0), &Multi::pack_zero(&manifest, &records))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    farm.nothing_indexed();
}

/// Asserts that a large object under its key is streamed into the CAS and the action is cached:
/// the result is as the manifest describes, the CAS holds the blob under its digest (the node
/// streams back bytes that hash to it, FindMissing says it is held), the frontend serves the
/// action-cache entry (which it does only when every blob the result names is held), and the counts
/// say one pack and the large bytes.
#[tokio::test]
async fn a_large_object_is_streamed_into_the_cas_and_the_run_is_cached() {
    let farm = Multi::new().await;
    let (manifest, zero) = farm.large_run();
    let digest = large().1;
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert!(
        farm.backend
            .find_missing(&[digest])
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(streamed_by_node(&farm.rig, &digest).await, digest);
    assert_eq!(farm.cached().await.expect("a hit"), result_of(&manifest));
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingest_packs(), 1);
    assert_eq!(metrics.ingest_large_bytes(), LARGE_LEN as u64);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    let line = format!("kbf_pack_ingest_large_bytes_total {LARGE_LEN}\n");
    assert!(scrape.contains(&line), "{scrape}");
}

/// Asserts that a large object is never held whole: the object store is read a window at a time
/// in order, one read per piece the node takes and none ahead of it, no window over 8 MiB and no
/// piece the node is given over it, though the object is over 64 MiB.
#[tokio::test]
async fn a_large_object_is_read_and_stored_a_window_at_a_time() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
    let key = farm.large_key(0).key().to_owned();
    let reads: Vec<(u64, u64)> = farm
        .rig
        .store
        .ranges()
        .into_iter()
        .filter(|(k, _, _)| *k == key)
        .map(|(_, offset, len)| (offset, len))
        .collect();
    let windows = (LARGE_LEN as u64).div_ceil(WINDOW);
    assert_eq!(reads.len() as u64, windows);
    for (i, (offset, len)) in reads.iter().enumerate() {
        assert_eq!(*offset, i as u64 * WINDOW, "windows in order");
        assert!(*len <= WINDOW, "a window of {len} bytes");
    }
    let notes = &farm.watching.notes;
    assert_eq!(notes.pieces.load(Ordering::SeqCst) as u64, windows);
    assert!(notes.largest.load(Ordering::SeqCst) as u64 <= WINDOW);
    assert_eq!(notes.ahead.load(Ordering::SeqCst), 0, "no read ahead");
    assert!(
        (WINDOW as usize) < LARGE_LEN / 4,
        "the bound is well below the object"
    );
}

/// Asserts that a large object whose bytes do not hash to its digest is a `HashMismatch`, nothing
/// is kept under that digest in the CAS, nothing is indexed and nothing is cached.
#[tokio::test]
async fn a_large_object_that_does_not_hash_is_a_hash_mismatch_and_not_cached() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    let mut wrong = large().0.to_vec();
    wrong[LARGE_LEN / 3] ^= 1;
    farm.put(farm.large_key(0), &wrong).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    let digest = large().1;
    let missing = farm.backend.find_missing(&[digest]).await.expect("asked");
    assert_eq!(missing, [digest], "nothing is kept under the digest");
    farm.nothing_indexed();
}

/// Asserts that a large object of another length than the manifest's is a `HashMismatch` found
/// by the length alone: no byte of it is read.
#[tokio::test]
async fn a_large_object_of_another_length_is_a_hash_mismatch_unread() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0[..LARGE_LEN - 1])
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    let key = farm.large_key(0).key();
    assert!(farm.rig.store.ranges().iter().all(|(k, _, _)| k != key));
    farm.nothing_indexed();
}

/// Asserts that an object that is shorter than its length said (the store answered a length that
/// the bytes do not reach) is a `HashMismatch` once the last window comes back short, and that
/// nothing is cached.
#[tokio::test]
async fn an_object_shorter_than_its_length_said_is_a_hash_mismatch() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0[..LARGE_LEN - 3])
        .await;
    farm.rig.store.overstate_len_of(farm.large_key(0).key(), 3);

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    farm.nothing_indexed();
}

/// Asserts that a large object the store does not hold is `Incomplete`, nothing is indexed and
/// nothing is cached.
#[tokio::test]
async fn a_missing_large_object_is_incomplete() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that a read of the large object that fails in the object store is `Unavailable`, counted
/// so and not as a refusal, and that nothing is indexed or cached.
#[tokio::test]
async fn a_large_object_the_store_cannot_read_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm.rig.store.reject_big_reads(1);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingest_unavailable(), 1);
    assert_eq!(metrics.ingests(), 0);
    farm.nothing_indexed();
    let cached = farm.cached().await.expect_err("nothing cached");
    assert_eq!(cached.code(), Code::NotFound);
}

/// Asserts that a storage node that cannot take the streamed blob makes the ingest `Unavailable`
/// (the object is fine, the lease may be ingested again), not a refusal of the object.
#[tokio::test]
async fn a_node_that_cannot_take_the_large_blob_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm.watching.refuse.store(true, Ordering::SeqCst);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
    farm.nothing_indexed();
}

/// Asserts that a digest that is a record of two packs is `Malformed` and nothing is indexed: a
/// digest is in one place.
#[tokio::test]
async fn a_digest_in_two_packs_is_malformed() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let twice = segment_of(&[&p.alpha, &p.in_dir, &p.beta]);
    farm.put(farm.pack_key(1), &twice).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Malformed).await;
    farm.nothing_indexed();
}

/// Asserts that a tree that names a file no pack and no large object holds is `Incomplete`
/// though the tree is in pack 0 and its other file in pack 1, and nothing is indexed.
#[tokio::test]
async fn a_tree_naming_a_file_no_pack_holds_is_incomplete() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let hollow = tree_of(&[sha256(&p.in_dir), blob("held nowhere").0]);
    let mut manifest = farm.manifest(2, &[]);
    manifest.output_directories[0].tree = hollow.0;
    let zero = Multi::pack_zero(&manifest, &[&hollow.1, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &segment_of(&[&p.alpha, &p.in_dir]))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that an output directory whose tree is a large object is `Incomplete` (a tree is read
/// from a record), found before the large object is read.
#[tokio::test]
async fn a_tree_that_is_a_large_object_is_incomplete_and_the_object_unread() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let digest = large().1;
    let mut manifest = farm.manifest(1, &[(0, digest)]);
    manifest.output_directories[0].tree = digest;
    let zero = Multi::pack_zero(&manifest, &[&p.alpha, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    let key = farm.large_key(0).key();
    assert!(farm.rig.store.ranges().iter().all(|(k, _, _)| k != key));
}

/// Asserts that the large cap is the limits': a manifest that names a large object over
/// `max_large_bytes`, or any when it is 0, is a `BadManifest`, and no large object is asked about.
#[tokio::test]
async fn a_large_object_over_the_limit_is_a_bad_manifest() {
    for cap in [0, LARGE_LEN as u64 - 1] {
        let farm = Multi::new().await;
        let (_, zero) = farm.large_run();
        farm.put(farm.pack_key(0), &zero).await;
        let limits = UntrustedLimits {
            max_large_bytes: cap,
            ..UntrustedLimits::default()
        };

        let got = farm.run_with(&farm.keys, &limits).await;

        farm.rejected(got, RejectReason::BadManifest).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a pack 1 whose records do not lie back to back is `Malformed` by the rules of a
/// task pack, and nothing is indexed.
#[tokio::test]
async fn a_second_pack_that_is_not_laid_out_is_malformed() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let records = [&p.alpha[..], &p.in_dir[..]].concat();
    let gap = p.alpha.len() as u64 + 1;
    let entries = [entry_of(&p.alpha, 0), entry_of(&p.in_dir, gap)];
    farm.put(farm.pack_key(1), &forged(&records, &entries))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Malformed).await;
    farm.nothing_indexed();
}

/// Asserts that a record of pack 1 that is not its digest is a `HashMismatch`, and that pack 0 is
/// not indexed and nothing is cached: pack 1 is committed before pack 0, so a run that fails
/// midway leaves no row for the pack that holds the manifest.
#[tokio::test]
async fn a_record_of_the_second_pack_that_is_not_its_digest_leaves_pack_zero_unrecorded() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let garbage = vec![0x5a; p.in_dir.len()];
    let records = [&p.alpha[..], &garbage[..]].concat();
    let entries = [
        entry_of(&p.alpha, 0),
        entry_of(&p.in_dir, p.alpha.len() as u64),
    ];
    farm.put(farm.pack_key(1), &forged(&records, &entries))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    assert!(
        farm.rig
            .index
            .pack(farm.pack_key(0).key())
            .expect("asked")
            .is_none()
    );
}

/// Asserts that a key set that is not the one issued is `WrongKey` and the store is not asked for a
/// byte: a pack key of another lease, keys in the wrong order, a pack key in a large place, a key
/// whose slot was changed to another slot of the run or to one a run does not have, no pack key, more
/// keys than a run has slots.
#[tokio::test]
async fn a_key_set_that_is_not_the_one_issued_is_wrong_key() {
    let farm = Multi::new().await;
    let (_, zero, one) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &one).await;
    let (packs, large) = (farm.keys.packs().to_vec(), farm.keys.large().to_vec());
    let foreign = "00000000000000010000000000000003";
    let foreign = farm
        .mint
        .mint_set(foreign, &farm.action, now(), 3, 2)
        .expect("keys");
    let changed = |key: &IssuedPackKey, slot: char| {
        let nonce = key.key().rsplit('/').next().expect("a nonce").to_owned();
        let moved = format!("{}{slot}", &nonce[..nonce.len() - 1]);
        IssuedPackKey::new(LEASE, &moved).expect("a key")
    };
    let sets = [
        vec![packs[0].clone(), foreign[1].clone(), packs[2].clone()],
        vec![packs[1].clone(), packs[0].clone(), packs[2].clone()],
        vec![packs[0].clone(), changed(&packs[1], '2'), packs[2].clone()],
        vec![packs[0].clone(), changed(&packs[1], '9'), packs[2].clone()],
        Vec::new(),
        vec![packs[0].clone(); 9],
    ];
    let mut cases: Vec<IssuedKeySet> = sets
        .into_iter()
        .map(|packs| IssuedKeySet::new(packs, large.clone()))
        .collect();
    cases.push(IssuedKeySet::new(packs.clone(), vec![packs[0].clone()]));

    for keys in &cases {
        let got = farm.run_with(keys, &UntrustedLimits::default()).await;
        assert_eq!(got, Ingested::Reject(RejectReason::WrongKey));
    }

    let counted = farm
        .rig
        .packs
        .metrics()
        .ingest_rejects(RejectReason::WrongKey);
    assert_eq!(counted, cases.len() as u64);
    assert!(farm.rig.store.ranges().is_empty(), "the store was not read");
    farm.nothing_indexed();
}

/// Asserts how a minted set is split: the first keys are the pack keys and the rest the large
/// ones, a count over the keys takes them all as pack keys, and one key is a set of one pack.
#[test]
fn a_minted_set_splits_into_pack_keys_then_large_keys() {
    let mint = PackKeyMint::new(SECRET).expect("a mint");
    let action = blob("an action").0;
    let keys = mint.mint_set(LEASE, &action, now(), 3, 2).expect("keys");

    let set = IssuedKeySet::from_minted(keys.clone(), 3);
    assert_eq!(set.packs(), &keys[..3]);
    assert_eq!(set.large(), &keys[3..]);
    let all = IssuedKeySet::from_minted(keys.clone(), 9);
    assert_eq!(all.packs(), &keys[..]);
    assert!(all.large().is_empty());
    let single = IssuedKeySet::single(keys[0].clone());
    assert_eq!(single.packs(), &keys[..1]);
    assert!(single.large().is_empty());
}
