//! `ingest`: the pack a task wrote under a key the frontend minted is read from the object store,
//! hashed to the last byte, checked against the manifest at its offset 0, and turned into an
//! action result that is written to the action cache only when the action ran cacheably and exited 0.
//! The minting of the key (`PackKeyMint`) is in `untrusted_mint`.
//!
//! The store is an in-memory object store that logs every range it is asked for, the index is an
//! in-memory pack index, and the action cache is a real storage node behind the frontend, read
//! back through the frontend's own GetActionResult. Every test says the behaviour it asserts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::{
    Backend, Ingested, IssuedPackKey, PackKeyMint, RejectReason, UntrustedLimits, ingest,
};
use kbf_pack::{
    EMPTY_DIGEST, InputStats, OutputDirectory, OutputFile, OutputSymlink, Slot, TaskManifest,
    Timing, Usage, write_pack, write_pack_with,
};
use kbf_pack_index::PackIndex as _;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{self, ActionResult, Directory, FileNode, GetActionResultRequest, Tree};
use kbf_segments::layout::{TRAILER_LEN, encode_footer, footer_len};
use kbf_segments::{IndexEntry, MAX_SEGMENT_BYTES, SegmentWriter, crc32c, sha256};
use kbf_types::Digest;
use prost::Message as _;
use tonic::Code;

use crate::support::{Rig, blob, update};

pub(crate) const LEASE: &str = "00000000000000010000000000000002";
pub(crate) const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
/// A minute boundary: 29833334 minutes after the epoch.
const NOW_SECS: u64 = 1_790_000_040;

const OBJECT: &str = "object code of the action";
const STDOUT: &str = "compiling\n";
const IN_DIR: &str = "a file inside the output directory";

pub(crate) fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_SECS)
}

pub(crate) fn wire(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// The `Tree` that names `files` (one directory, no children), and its digest.
pub(crate) fn tree_of(files: &[Digest]) -> (Digest, Bytes) {
    let named = |digest: &Digest| FileNode {
        name: digest.hash_hex(),
        digest: Some(wire(digest)),
        ..FileNode::default()
    };
    let tree = Tree {
        root: Some(Directory {
            files: files.iter().map(named).collect(),
            ..Directory::default()
        }),
        ..Tree::default()
    };
    let bytes = Bytes::from(tree.encode_to_vec());
    (sha256(&bytes), bytes)
}

/// The record of `bytes` at `offset`, as the footer lists it.
pub(crate) fn entry_of(bytes: &[u8], offset: u64) -> IndexEntry {
    IndexEntry {
        digest: sha256(bytes),
        offset,
        crc32c: crc32c(bytes),
    }
}

/// A pack of `records` back to back as given and a footer that lists `entries`, whatever the
/// records are: the footer is made, not derived, so it can lie about the layout.
pub(crate) fn forged(records: &[u8], entries: &[IndexEntry]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| entry.digest);
    let mut bytes = records.to_vec();
    bytes.extend(encode_footer(records.len() as u64, &sorted));
    bytes
}

/// A pack of `records` pushed in order (the first is the record at offset 0).
pub(crate) fn segment_of(records: &[&[u8]]) -> Vec<u8> {
    let mut writer = SegmentWriter::new(MAX_SEGMENT_BYTES);
    for record in records {
        writer.push(record).expect("pushed");
    }
    writer.finish()
}

/// Sets the record count in the trailer of `pack` (the trailer is its last 32 bytes, the count
/// a u32 after the index offset); nothing else in the pack changes.
fn patch_count(pack: &mut [u8], count: u32) {
    let at = pack.len() - TRAILER_LEN + 8;
    pack[at..at + 4].copy_from_slice(&count.to_le_bytes());
}

/// The bytes of an Action that says `do_not_cache` as given.
pub(crate) fn action_bytes(do_not_cache: bool) -> Bytes {
    let action = reapi::Action {
        command_digest: Some(wire(&blob("the command").0)),
        input_root_digest: Some(wire(&blob("the inputs").0)),
        do_not_cache,
        ..reapi::Action::default()
    };
    Bytes::from(action.encode_to_vec())
}

/// The frontend with packs, a mint, the Action of the lease and the key issued for it.
pub(crate) struct Farm {
    rig: Rig,
    backend: Backend,
    mint: PackKeyMint,
    action: Digest,
    key: IssuedPackKey,
}

impl Farm {
    pub(crate) async fn new() -> Self {
        Self::of(false).await
    }

    /// A farm whose Action says `do_not_cache`.
    async fn of(do_not_cache: bool) -> Self {
        Self::with_action(action_bytes(do_not_cache)).await
    }

    /// A farm whose Action blob is `bytes`, whatever they are.
    async fn with_action(bytes: Bytes) -> Self {
        Self::build(bytes, true).await
    }

    /// A farm whose Action blob was never stored.
    async fn without_action() -> Self {
        Self::build(Bytes::from_static(b"an action nobody stored"), false).await
    }

    /// A farm over a store that replaces an object it holds (a task that writes again).
    async fn replacing() -> Self {
        Self::over(Rig::replacing().await, action_bytes(false), true).await
    }

    async fn build(bytes: Bytes, store: bool) -> Self {
        Self::over(Rig::new().await, bytes, store).await
    }

    async fn over(rig: Rig, bytes: Bytes, store: bool) -> Self {
        let action = sha256(&bytes);
        if store {
            let stored = update(&rig.front, &[(action, bytes)]).await;
            assert_eq!(stored, [0], "the Action is stored");
        }
        let backend = Backend::remote_over(rig.node().clone(), Some(rig.packs.clone()));
        let mint = PackKeyMint::new(SECRET).expect("a mint");
        let key = mint
            .mint(LEASE, &action, now(), Slot::Pack(0))
            .expect("a key");
        Self {
            rig,
            backend,
            mint,
            action,
            key,
        }
    }

    /// The manifest of an honest task that exited with `exit_code`: the files are listed out of
    /// order, one is empty (no record), and there is a directory and a symlink.
    pub(crate) fn manifest(&self, exit_code: i32) -> TaskManifest {
        let file = |path: &str, digest: Digest, is_executable: bool| OutputFile {
            path: path.to_owned(),
            digest,
            is_executable,
        };
        TaskManifest {
            lease: LEASE.to_owned(),
            action: self.action,
            exit_code,
            stdout: Some(blob(STDOUT).0),
            stderr: None,
            output_files: vec![
                file("out/z.o", blob(OBJECT).0, true),
                file("out/a.txt", EMPTY_DIGEST, false),
            ],
            output_directories: ["out/d", "out/c"]
                .map(|path| OutputDirectory {
                    path: path.to_owned(),
                    tree: self.tree().0,
                })
                .to_vec(),
            output_symlinks: [("out/l", "z.o"), ("out/k", "a.txt")]
                .map(|(path, target)| OutputSymlink {
                    path: path.to_owned(),
                    target: target.to_owned(),
                })
                .to_vec(),
            timing: Timing::default(),
            usage: Usage::default(),
            packs: 1,
            large: Vec::new(),
            inputs: None,
        }
    }

    fn tree(&self) -> (Digest, Bytes) {
        tree_of(&[blob(IN_DIR).0])
    }

    /// The pack of an honest task that exited with `exit_code`.
    fn pack(&self, exit_code: i32) -> Vec<u8> {
        self.pack_of(&self.manifest(exit_code))
    }

    /// The pack of the honest task's blobs under `manifest`, which may differ from the honest
    /// one in what the pack's records do not depend on (the timing, the usage).
    pub(crate) fn pack_of(&self, manifest: &TaskManifest) -> Vec<u8> {
        let (object, stdout, in_dir) = (blob(OBJECT).1, blob(STDOUT).1, blob(IN_DIR).1);
        let tree = self.tree().1;
        write_pack(manifest, &[&object, &stdout, &tree, &in_dir]).expect("a pack")
    }

    /// The result the frontend must rebuild from the honest pack: sorted by path.
    pub(crate) fn result(&self, exit_code: i32) -> ActionResult {
        let file = |path: &str, digest: &Digest, is_executable: bool| reapi::OutputFile {
            path: path.to_owned(),
            digest: Some(wire(digest)),
            is_executable,
            ..reapi::OutputFile::default()
        };
        ActionResult {
            output_files: vec![
                file("out/a.txt", &EMPTY_DIGEST, false),
                file("out/z.o", &blob(OBJECT).0, true),
            ],
            output_directories: ["out/c", "out/d"]
                .map(|path| reapi::OutputDirectory {
                    path: path.to_owned(),
                    tree_digest: Some(wire(&self.tree().0)),
                    ..reapi::OutputDirectory::default()
                })
                .to_vec(),
            output_symlinks: [("out/k", "a.txt"), ("out/l", "z.o")]
                .map(|(path, target)| reapi::OutputSymlink {
                    path: path.to_owned(),
                    target: target.to_owned(),
                    ..reapi::OutputSymlink::default()
                })
                .to_vec(),
            stdout_digest: Some(wire(&blob(STDOUT).0)),
            exit_code,
            ..ActionResult::default()
        }
    }

    /// The blobs the honest pack holds besides its manifest.
    fn blobs(&self) -> [Digest; 4] {
        [
            blob(OBJECT).0,
            blob(STDOUT).0,
            self.tree().0,
            blob(IN_DIR).0,
        ]
    }

    /// Stores `bytes` under the issued key, as the task's presigned write would.
    pub(crate) async fn put(&self, bytes: &[u8]) {
        self.rig.store.seed(self.key.key(), bytes).await;
    }

    async fn ingest_with(&self, limits: &UntrustedLimits) -> Ingested {
        let packs = &self.rig.packs;
        let (mint, key) = (&self.mint, &self.key);
        ingest(packs, &self.backend, mint, LEASE, key, &self.action, limits).await
    }

    pub(crate) async fn ingest(&self) -> Ingested {
        self.ingest_with(&UntrustedLimits::default()).await
    }

    /// The ingest of the object under `key`, which is not necessarily the one issued.
    async fn ingest_key(&self, key: &IssuedPackKey) -> Ingested {
        let (packs, limits) = (&self.rig.packs, UntrustedLimits::default());
        let (backend, mint) = (&self.backend, &self.mint);
        ingest(packs, backend, mint, LEASE, key, &self.action, &limits).await
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

    /// Asserts that the pack was refused for `reason`: counted under it and no other, and no
    /// action-cache entry.
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

    /// Asserts that the index has no row for the pack and none of its blobs.
    fn nothing_indexed(&self) {
        assert!(
            self.rig
                .index
                .pack(self.key.key())
                .expect("asked")
                .is_none()
        );
        assert!(self.rig.index.has_none(&self.blobs()));
    }
}

/// Asserts the happy path: an honest pack is ingested into exactly the action result its
/// manifest describes (paths sorted, the empty file listed with the empty digest and no record,
/// the symlink, the directory's tree, stdout), the result is cached and served by the frontend,
/// every blob the pack holds is in the index, and the pack is recorded. Catches a result built
/// from the wrong fields, one left unsorted, an empty blob given a record, a result not written to
/// the action cache and blobs not indexed.
#[tokio::test]
async fn an_honest_pack_is_ingested_into_the_exact_action_result_and_cached() {
    let farm = Farm::new().await;
    farm.put(&farm.pack(0)).await;

    let got = farm.ingest().await;

    let want = Ingested::Ran {
        result: Box::new(farm.result(0)),
        cached: true,
    };
    assert_eq!(got, want);
    assert_eq!(farm.cached().await.expect("a hit"), farm.result(0));
    assert!(farm.rig.index.has_all(&farm.blobs()));
    assert!(
        farm.rig
            .index
            .pack(farm.key.key())
            .expect("asked")
            .is_some()
    );
    assert_eq!(farm.rig.packs.metrics().ingests(), 1);
}

/// Asserts that an action that ran and failed is reported, not cached: the result carries the
/// exit code, nothing is written to the action cache, and the blobs (stdout among them) are
/// indexed so the client can read them. Catches a failed action cached.
#[tokio::test]
async fn a_failed_action_is_ran_and_not_cached() {
    let farm = Farm::new().await;
    farm.put(&farm.pack(3)).await;

    let got = farm.ingest().await;

    let want = Ingested::Ran {
        result: Box::new(farm.result(3)),
        cached: false,
    };
    assert_eq!(got, want);
    let miss = farm.cached().await.expect_err("not cached");
    assert_eq!(miss.code(), Code::NotFound);
    assert!(farm.rig.index.has_all(&farm.blobs()));
}

/// Asserts that an action that says `do_not_cache` is ingested and not cached even when it
/// exited 0. Catches an ingest that decides cacheability from the exit code alone.
#[tokio::test]
async fn a_do_not_cache_action_is_ran_and_not_cached() {
    let farm = Farm::of(true).await;
    farm.put(&farm.pack(0)).await;

    let got = farm.ingest().await;

    let want = Ingested::Ran {
        result: Box::new(farm.result(0)),
        cached: false,
    };
    assert_eq!(got, want);
    let miss = farm.cached().await.expect_err("not cached");
    assert_eq!(miss.code(), Code::NotFound);
}

/// Asserts that an Action that does not decode is not cached, since the frontend cannot say it
/// may be: the action is ingested as ran and the action cache stays empty.
#[tokio::test]
async fn an_action_that_does_not_decode_is_ran_and_not_cached() {
    let farm = Farm::with_action(blob("not an Action \u{ff}\u{ff}").1).await;
    farm.put(&farm.pack(0)).await;

    let got = farm.ingest().await;

    let want = Ingested::Ran {
        result: Box::new(farm.result(0)),
        cached: false,
    };
    assert_eq!(got, want);
    let miss = farm.cached().await.expect_err("not cached");
    assert_eq!(miss.code(), Code::NotFound);
}

/// Asserts that an Action the store does not hold, which the frontend must read to know whether
/// the result may be cached, is `Unavailable` and counted so, not a refusal of the pack.
#[tokio::test]
async fn an_action_the_store_does_not_hold_is_unavailable() {
    let farm = Farm::without_action().await;
    farm.put(&farm.pack(0)).await;

    let got = farm.ingest().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
}

/// Asserts that ingesting the same key again is idempotent: the same result, the same cache entry,
/// one pack recorded and counted once as recorded.
#[tokio::test]
async fn replaying_the_same_key_is_idempotent() {
    let farm = Farm::new().await;
    farm.put(&farm.pack(0)).await;

    let first = farm.ingest().await;
    let again = farm.ingest().await;

    assert_eq!(first, again);
    assert!(
        matches!(again, Ingested::Ran { cached: true, .. }),
        "{again:?}"
    );
    assert_eq!(farm.cached().await.expect("a hit"), farm.result(0));
    let mut text = String::new();
    farm.rig.packs.render_metrics(&mut text);
    assert!(
        text.contains("kbf_pack_untrusted_commits_total 1\n"),
        "{text}"
    );
    assert_eq!(farm.rig.packs.metrics().ingests(), 2);
}

/// Asserts that no object under the key is `Missing`, and that the store's length answer, not a
/// read, said so.
#[tokio::test]
async fn an_absent_object_is_missing() {
    let farm = Farm::new().await;

    let got = farm.ingest().await;

    farm.rejected(got, RejectReason::Missing).await;
    assert!(farm.rig.store.ranges().is_empty(), "nothing was read");
}

/// Asserts that a key the frontend did not mint is refused before the store is asked about it:
/// a key of the right shape with a made-up MAC, and a key minted for another action, another
/// lease or under another secret. Catches a key taken on its shape and a MAC that does not cover
/// the action, the lease or the secret.
#[tokio::test]
async fn a_key_not_minted_here_is_wrong_key() {
    let other_action = blob("another action").0;
    let secret = b"fedcba9876543210fedcba9876543210";
    let farm = Farm::new().await;
    let made_up = format!("{}{:08x}-0", "ab".repeat(16), NOW_SECS / 60);
    let keys = [
        IssuedPackKey::new(LEASE, &made_up).expect("a key"),
        farm.mint
            .mint(LEASE, &other_action, now(), Slot::Pack(0))
            .expect("a key"),
        farm.mint
            .mint("another", &farm.action, now(), Slot::Pack(0))
            .expect("a key"),
        PackKeyMint::new(secret)
            .expect("a mint")
            .mint(LEASE, &farm.action, now(), Slot::Pack(0))
            .expect("a key"),
    ];
    for key in &keys {
        farm.rig.store.seed(key.key(), &farm.pack(0)).await;
        let got = farm.ingest_key(key).await;
        assert_eq!(
            got,
            Ingested::Reject(RejectReason::WrongKey),
            "{}",
            key.key()
        );
    }
    assert!(farm.rig.store.ranges().is_empty(), "nothing was read");
    assert_eq!(
        farm.rig
            .packs
            .metrics()
            .ingest_rejects(RejectReason::WrongKey),
        4
    );
}

/// Asserts that a manifest that names another lease or another action is refused as the wrong
/// key, and indexes nothing. Catches a pack accepted because it is at the right key whatever it
/// says it is for.
#[tokio::test]
async fn a_manifest_for_another_lease_or_action_is_wrong_key() {
    let farm = Farm::new().await;
    let mut other_lease = farm.manifest(0);
    other_lease.lease = "someone-elses".to_owned();
    let mut other_action = farm.manifest(0);
    other_action.action = blob("another action").0;
    for manifest in [other_lease, other_action] {
        let farm = Farm::new().await;
        let (object, stdout, in_dir) = (blob(OBJECT).1, blob(STDOUT).1, blob(IN_DIR).1);
        let tree = farm.tree().1;
        let blobs: [&[u8]; 4] = [&object, &stdout, &tree, &in_dir];
        farm.put(&write_pack(&manifest, &blobs).expect("a pack"))
            .await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::WrongKey).await;
        farm.nothing_indexed();
    }
}

/// Asserts the caps, each refused before a record is read: a pack over `max_pack_bytes` (before
/// any read at all), more records than `max_blobs`, and a record over `max_blob_bytes`.
#[tokio::test]
async fn a_pack_over_a_cap_is_too_large() {
    let big = "b".repeat(5000);
    let farm = Farm::new().await;
    let honest = farm.pack(0);
    let (object, stdout, tree) = (blob(OBJECT).1, blob(STDOUT).1, farm.tree().1);
    let manifest = farm.manifest(0);
    let in_dir = blob(IN_DIR).1;
    let mut with_big = manifest;
    with_big.output_files[0].digest = blob(&big).0;
    let blobs: [&[u8]; 5] = [big.as_bytes(), &stdout, &tree, &in_dir, &object];
    let bigger = write_pack(&with_big, &blobs).expect("a pack");
    let default = UntrustedLimits::default();
    let mut counting = honest.clone();
    patch_count(&mut counting, 20_000);
    let cases = [
        ("trailer", counting, default),
        (
            "pack",
            honest.clone(),
            UntrustedLimits {
                max_pack_bytes: honest.len() as u64 - 1,
                ..default
            },
        ),
        (
            "blobs",
            honest.clone(),
            UntrustedLimits {
                max_blobs: 4,
                ..default
            },
        ),
        (
            "blob",
            bigger,
            UntrustedLimits {
                max_blob_bytes: 3000,
                ..default
            },
        ),
    ];
    for (what, bytes, limits) in cases {
        let farm = Farm::new().await;
        farm.put(&bytes).await;

        let got = farm.ingest_with(&limits).await;

        farm.rejected(got, RejectReason::TooLarge).await;
        farm.nothing_indexed();
        if what == "pack" {
            assert!(
                farm.rig.store.ranges().is_empty(),
                "{what}: nothing was read"
            );
        }
    }
}

/// Asserts that a record at offset 0 over 4 MiB is refused without being read: the only ranges
/// asked of the store are in the footer (the trailer and the footer), none in the records.
/// Catches a manifest buffered whole before its size is checked.
#[tokio::test]
async fn a_manifest_over_four_mebibytes_is_too_large_and_unread() {
    let farm = Farm::new().await;
    let manifest = vec![b'x'; (4 << 20) + 1];
    let bytes = segment_of(&[&manifest]);
    let records = bytes.len() as u64 - footer_len(1);
    farm.put(&bytes).await;

    let got = farm.ingest().await;

    farm.rejected(got, RejectReason::TooLarge).await;
    let asked = farm.rig.store.ranges();
    assert!(!asked.is_empty());
    for (_, offset, _) in asked {
        assert!(offset >= records, "a record was read at {offset}");
    }
}

/// Asserts that a layout that is not records back to back from 0 is `Malformed`, before a
/// record is read: a record of no bytes, a gap between records, an overlap, and a footer or a
/// trailer that is damaged.
#[tokio::test]
async fn a_pack_that_is_not_laid_out_is_malformed() {
    let farm = Farm::new().await;
    let manifest = farm.manifest(0).to_json().expect("json");
    let m = manifest.len() as u64;
    let blob_bytes = blob(OBJECT).1;
    let with = |gap: &[u8]| [&manifest[..], gap, &blob_bytes[..]].concat();
    let zero = forged(&manifest, &[entry_of(&manifest, 0), entry_of(b"", m)]);
    let gap = forged(
        &with(&[0; 3]),
        &[entry_of(&manifest, 0), entry_of(&blob_bytes, m + 3)],
    );
    let overlap = forged(
        &with(&[]),
        &[entry_of(&manifest, 0), entry_of(&blob_bytes, m - 2)],
    );
    let honest = farm.pack(0);
    let honest_again = honest.clone();
    let mut in_footer = honest.clone();
    let last = in_footer.len() - 40;
    in_footer[last] ^= 0xff;
    let mut in_trailer = honest;
    let end = in_trailer.len() - 1;
    in_trailer[end] ^= 0xff;
    let mut long_footer = honest_again.clone();
    patch_count(&mut long_footer, 1000);
    let empty = forged(b"", &[]);
    let tiny = vec![0; 10];
    for bytes in [
        zero,
        gap,
        overlap,
        in_footer,
        in_trailer,
        long_footer,
        empty,
        tiny,
    ] {
        let farm = Farm::new().await;
        farm.put(&bytes).await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::Malformed).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a record whose bytes are not its digest is a `HashMismatch` and indexes nothing:
/// a flipped byte in a blob, in a tree (checked before the commit), and in the manifest itself.
/// Catches a record taken on the word of the footer.
#[tokio::test]
async fn a_record_that_is_not_its_digest_is_a_hash_mismatch() {
    let probe = Farm::new().await;
    let honest = probe.pack(0);
    let manifest_len = probe.manifest(0).to_json().expect("json").len();
    let tree_at = manifest_len + OBJECT.len() + STDOUT.len() + 2;
    for at in [manifest_len + 3, 5, tree_at] {
        let farm = Farm::new().await;
        let mut bytes = honest.clone();
        bytes[at] ^= 0xff;
        farm.put(&bytes).await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::HashMismatch).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a record at offset 0 that is not a valid manifest is `BadManifest`: not JSON, and
/// JSON with a field the manifest does not have.
#[tokio::test]
async fn a_record_at_offset_zero_that_is_not_a_manifest_is_bad() {
    let junk: [&[u8]; 2] = [b"not json at all", br#"{"version":1,"surprise":true}"#];
    for text in junk {
        let farm = Farm::new().await;
        farm.put(&segment_of(&[text, OBJECT.as_bytes()])).await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::BadManifest).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a manifest that names a digest the pack has no record for (an output file, stdout,
/// a tree) is `Incomplete` and nothing is indexed; and that a tree that names a file the pack does
/// not hold is `Incomplete` too, and a tree that does not decode is a `BadManifest`.
#[tokio::test]
async fn a_manifest_naming_a_digest_outside_the_pack_is_incomplete() {
    let farm = Farm::new().await;
    let (object, stdout, in_dir) = (blob(OBJECT).1, blob(STDOUT).1, blob(IN_DIR).1);
    let tree = farm.tree().1;
    let elsewhere = blob("held nowhere").0;
    let mut no_stdout = farm.manifest(0);
    no_stdout.stdout = Some(elsewhere);
    let mut no_file = farm.manifest(0);
    no_file.output_files[0].digest = elsewhere;
    let mut no_tree = farm.manifest(0);
    no_tree.output_directories[0].tree = elsewhere;
    for manifest in [no_stdout, no_file, no_tree] {
        let farm = Farm::new().await;
        let blobs: [&[u8]; 4] = [&object, &stdout, &tree, &in_dir];
        farm.put(&write_pack(&manifest, &blobs).expect("a pack"))
            .await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::Incomplete).await;
        farm.nothing_indexed();
    }
    let hollow = tree_of(&[blob(IN_DIR).0, elsewhere]);
    let junk = blob("not a Tree \u{ff}\u{ff}");
    let cases = [
        (hollow, RejectReason::Incomplete),
        (junk, RejectReason::BadManifest),
    ];
    for ((digest, bytes), reason) in cases {
        let farm = Farm::new().await;
        let mut manifest = farm.manifest(0);
        for dir in &mut manifest.output_directories {
            dir.tree = digest;
        }
        let blobs: [&[u8]; 4] = [&object, &stdout, &bytes, &in_dir];
        farm.put(&write_pack(&manifest, &blobs).expect("a pack"))
            .await;

        let got = farm.ingest().await;

        farm.rejected(got, reason).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a digest held in the pack under another size is not held: a file, a stdout, a
/// tree, and a file a tree names, each given the size of the record plus one with the hash of the
/// record, are `Incomplete` and nothing is indexed. Catches a held set keyed by hash alone.
#[tokio::test]
async fn a_digest_held_under_another_size_is_incomplete() {
    let one_more = |digest: Digest| Digest {
        size_bytes: digest.size_bytes + 1,
        ..digest
    };
    let (object, stdout, in_dir) = (blob(OBJECT).1, blob(STDOUT).1, blob(IN_DIR).1);
    let probe = Farm::new().await;
    let tree = probe.tree().1;
    let mut file = probe.manifest(0);
    file.output_files[0].digest = one_more(blob(OBJECT).0);
    let mut out = probe.manifest(0);
    out.stdout = Some(one_more(blob(STDOUT).0));
    let mut dir = probe.manifest(0);
    dir.output_directories[0].tree = one_more(probe.tree().0);
    let lying = tree_of(&[one_more(blob(IN_DIR).0)]);
    let mut inner = probe.manifest(0);
    for dir in &mut inner.output_directories {
        dir.tree = lying.0;
    }
    let cases = [
        (file, tree.clone()),
        (out, tree.clone()),
        (dir, tree),
        (inner, lying.1),
    ];
    for (manifest, tree) in cases {
        let farm = Farm::new().await;
        let blobs: [&[u8]; 4] = [&object, &stdout, &tree, &in_dir];
        farm.put(&write_pack(&manifest, &blobs).expect("a pack"))
            .await;

        let got = farm.ingest().await;

        farm.rejected(got, RejectReason::Incomplete).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a different object under a key already recorded is refused as `WrongKey` (the
/// commit's conflict) and that the first pack stays what the index says: a task that writes its
/// key again, with another result, cannot change what was recorded. Catches a replay accepted
/// because the key is the same.
#[tokio::test]
async fn another_object_under_a_recorded_key_is_wrong_key() {
    let farm = Farm::replacing().await;
    farm.put(&farm.pack(0)).await;
    let first = farm.ingest().await;
    assert!(matches!(first, Ingested::Ran { .. }), "{first:?}");

    farm.put(&farm.pack(3)).await;
    let got = farm.ingest().await;

    assert_eq!(got, Ingested::Reject(RejectReason::WrongKey));
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingest_rejects(RejectReason::WrongKey), 1);
    assert_eq!(metrics.ingests(), 1);
}

/// Asserts that an object longer than the store's length said (the length is read first, the
/// object grows after it) is `Missing`, the commit's answer for a pack that is not as long as it
/// was said to be, and indexes nothing: the extra byte is found by the commit's read of one byte
/// past the trailer.
#[tokio::test]
async fn an_object_longer_than_its_length_said_is_missing() {
    let farm = Farm::new().await;
    let mut bytes = farm.pack(0);
    bytes.push(0);
    farm.put(&bytes).await;
    farm.rig.store.understate_len(1);

    let got = farm.ingest().await;

    farm.rejected(got, RejectReason::Missing).await;
    farm.nothing_indexed();
}

/// Asserts that an output path with a `..` component, which no honest task writes, is a
/// `BadManifest` and indexes nothing: the manifest is parsed by `TaskManifest::parse`, which
/// refuses it, so the path never reaches the action result.
#[tokio::test]
async fn an_output_path_with_a_parent_component_is_a_bad_manifest() {
    let farm = Farm::new().await;
    let json = farm.manifest(0).to_json().expect("json");
    let text = String::from_utf8(json).expect("utf8");
    let dotted = text.replace("out/a.txt", "out/../a.txt");
    assert_ne!(dotted, text, "the path was replaced");
    let (object, stdout) = (blob(OBJECT).1, blob(STDOUT).1);
    let (tree, in_dir) = (farm.tree().1, blob(IN_DIR).1);
    farm.put(&segment_of(&[
        dotted.as_bytes(),
        &object,
        &stdout,
        &tree,
        &in_dir,
    ]))
    .await;

    let got = farm.ingest().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    farm.nothing_indexed();
}

/// Asserts that a version 2 manifest that names a second pack is a `BadManifest` and indexes
/// nothing: this ingest reads one pack, so it must not take the first of two as the whole run.
#[tokio::test]
async fn a_manifest_naming_two_packs_is_a_bad_manifest() {
    let farm = Farm::new().await;
    let mut manifest = farm.manifest(0);
    manifest.packs = 2;
    let (object, stdout) = (blob(OBJECT).1, blob(STDOUT).1);
    let (tree, in_dir) = (farm.tree().1, blob(IN_DIR).1);
    let pack = write_pack_with(&manifest, &[&object, &stdout, &tree, &in_dir], 0).expect("a pack");
    farm.put(&pack).await;

    let got = farm.ingest().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    farm.nothing_indexed();
}

/// Asserts that a manifest carrying the advisory `inputs` object is ingested like any other: the
/// numbers are carried for metrics and decide nothing.
#[tokio::test]
async fn a_manifest_with_inputs_is_ingested() {
    let farm = Farm::new().await;
    let mut manifest = farm.manifest(0);
    manifest.inputs = Some(InputStats {
        blobs: 3,
        requests: 4,
        ..InputStats::default()
    });
    let (object, stdout) = (blob(OBJECT).1, blob(STDOUT).1);
    let (tree, in_dir) = (farm.tree().1, blob(IN_DIR).1);
    let pack = write_pack(&manifest, &[&object, &stdout, &tree, &in_dir]).expect("a pack");
    farm.put(&pack).await;

    let got = farm.ingest().await;

    let want = Ingested::Ran {
        result: Box::new(farm.result(0)),
        cached: true,
    };
    assert_eq!(got, want);
}

/// Asserts that a store or an index that fails is `Unavailable`, counted as neither a refusal nor
/// an ingest, and that the same key ingests once they answer: a length that fails, a read that
/// fails, an index that cannot record.
#[tokio::test]
async fn a_failing_store_or_index_is_unavailable_and_retried_on_the_same_key() {
    for what in ["len", "read", "record"] {
        let farm = Farm::new().await;
        farm.put(&farm.pack(0)).await;
        match what {
            "len" => farm.rig.store.reject_lens(1),
            "read" => farm.rig.store.reject_reads(1),
            _ => farm.rig.index.fail_record(1),
        }

        let got = farm.ingest().await;

        assert!(matches!(got, Ingested::Unavailable(_)), "{what}: {got:?}");
        let metrics = farm.rig.packs.metrics();
        for each in RejectReason::ALL {
            assert_eq!(metrics.ingest_rejects(each), 0, "{what}: {each:?}");
        }
        assert_eq!(metrics.ingests(), 0);
        assert_eq!(metrics.ingest_unavailable(), 1, "{what}");
        let again = farm.ingest().await;
        assert!(
            matches!(again, Ingested::Ran { cached: true, .. }),
            "{what}: {again:?}"
        );
    }
}

/// Asserts that the scrape carries the ingest counts: the ingests, and the refusals by reason
/// with every label present.
#[tokio::test]
async fn ingests_are_scraped_by_reason() {
    let farm = Farm::new().await;
    let scrape = || {
        let mut text = String::new();
        farm.rig.packs.render_metrics(&mut text);
        text
    };
    let before = scrape();
    assert!(before.contains("kbf_pack_ingests_total 0\n"), "{before}");
    for reason in RejectReason::ALL {
        let line = format!(
            "kbf_pack_ingest_rejects_total{{reason=\"{}\"}} 0\n",
            reason.label()
        );
        assert!(before.contains(&line), "{line}in {before}");
    }
    assert!(
        before.contains("kbf_pack_ingest_unavailable_total 0\n"),
        "{before}"
    );

    let missing = farm.ingest().await;
    assert_eq!(missing, Ingested::Reject(RejectReason::Missing));
    farm.put(&farm.pack(0)).await;
    farm.ingest().await;

    let after = scrape();
    assert!(after.contains("kbf_pack_ingests_total 1\n"), "{after}");
    let line = "kbf_pack_ingest_rejects_total{reason=\"missing\"} 1\n";
    assert!(after.contains(line), "{after}");
}
