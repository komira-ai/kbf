//! `ingest_put_dir`: the run a task committed into the put directory the frontend minted for it,
//! `cas/b/<lease>/<nonce>/{pack,pack1..,L0..}`. The frontend reads pack 0's footer and manifest,
//! the other packs' footers and the large objects' lengths, checks the outputs against the
//! Command, and records each pack's footer in the index without reading its records. The
//! fixtures are `multi`'s records under a Command that declares their paths.
//!
//! Every test says the behaviour it asserts.

use std::time::Duration;

use bytes::Bytes;
use kbf_front::{
    Backend, Ingested, IssuedPutDir, PackKeyMint, RejectReason, UntrustedLimits, ingest_put_dir,
};
use kbf_pack::{MIN_GRACE, Slot, TaskManifest};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex as _};
use kbf_proto::reapi;
use kbf_segments::layout::footer_len;
use kbf_segments::sha256;
use kbf_types::Digest;
use prost::Message as _;
use tonic::Code;

use crate::multi::{Multi, Parts, large, result_of};
use crate::support::{Rig, blob, update};
use crate::untrusted_task::{LEASE, SECRET, now, segment_of, wire};

/// The paths `multi`'s manifests name.
const DECLARED: [&str; 4] = ["out/alpha", "out/beta", "out/big0", "out/d"];

/// A frontend with packs, the Action of the lease (its Command declaring `declared`), and the put
/// directory minted for it.
struct PutDir {
    rig: Rig,
    backend: Backend,
    mint: PackKeyMint,
    action: Digest,
    dir: IssuedPutDir,
    p: Parts,
}

/// The bytes of a Command that declares `paths` as REAPI 2.1 does (`output_paths`), or as before
/// it (`output_files` and `output_directories`) when `legacy`.
#[allow(deprecated)] // `output_files` and `output_directories`: the form before REAPI 2.1.
fn command_bytes(paths: &[&str], legacy: bool) -> Bytes {
    let owned = |paths: &[&str]| paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
    let command = if legacy {
        let (dirs, files): (Vec<&str>, Vec<&str>) =
            paths.iter().partition(|path| path.ends_with("/d"));
        reapi::Command {
            arguments: vec!["build".to_owned()],
            output_files: owned(&files),
            output_directories: owned(&dirs),
            ..reapi::Command::default()
        }
    } else {
        reapi::Command {
            arguments: vec!["build".to_owned()],
            output_paths: owned(paths),
            ..reapi::Command::default()
        }
    };
    Bytes::from(command.encode_to_vec())
}

impl PutDir {
    async fn new() -> Self {
        Self::declaring(&DECLARED, false).await
    }

    /// The farm with an Action whose Command is `command`, stored unless `None` (a Command the
    /// store does not hold).
    async fn with_command(command: Option<Bytes>, named: Digest) -> Self {
        let action = reapi::Action {
            command_digest: Some(wire(&named)),
            input_root_digest: Some(wire(&blob("the inputs").0)),
            ..reapi::Action::default()
        };
        Self::with_action(Bytes::from(action.encode_to_vec()), command, named).await
    }

    /// The farm with the Action `action_bytes`, and `command` stored as `named` unless `None`.
    async fn with_action(action_bytes: Bytes, command: Option<Bytes>, named: Digest) -> Self {
        let rig = Rig::new().await;
        let action = sha256(&action_bytes);
        let mut blobs = vec![(action, action_bytes)];
        if let Some(command) = command {
            blobs.push((named, command));
        }
        let stored = update(&rig.front, &blobs).await;
        assert!(stored.iter().all(|code| *code == 0), "{stored:?}");
        let mint = PackKeyMint::new(SECRET).expect("a mint");
        let dir = mint.mint_put_dir(LEASE, &action, now()).expect("a dir");
        let backend =
            Backend::remote_over(std::sync::Arc::clone(rig.node()), Some(rig.packs.clone()));
        Self {
            rig,
            backend,
            mint,
            action,
            dir,
            p: Parts::new(),
        }
    }

    async fn declaring(paths: &[&str], legacy: bool) -> Self {
        let command = command_bytes(paths, legacy);
        let named = sha256(&command);
        Self::with_command(Some(command), named).await
    }

    /// `multi`'s manifest of an honest run, for this farm's action.
    fn manifest(&self, packs: u8, huge: &[(u8, Digest)]) -> TaskManifest {
        multi_manifest(&self.p, self.action, packs, huge)
    }

    /// The manifest and the two packs of an honest run: the tree, `beta` and the standard
    /// output in pack 0; `alpha` and the file of the tree in pack 1.
    fn two_packs(&self) -> (TaskManifest, Vec<u8>, Vec<u8>) {
        let (p, manifest) = (&self.p, self.manifest(2, &[]));
        let zero = Multi::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
        let one = segment_of(&[&p.alpha, &p.in_dir]);
        (manifest, zero, one)
    }

    fn key(&self, slot: Slot) -> String {
        self.dir
            .key(slot)
            .expect("a slot of a run")
            .key()
            .to_owned()
    }

    async fn put(&self, slot: Slot, bytes: &[u8]) {
        self.rig.store.seed(&self.key(slot), bytes).await;
    }

    async fn run_dir(&self, dir: &IssuedPutDir) -> Ingested {
        let (packs, backend, mint) = (&self.rig.packs, &self.backend, &self.mint);
        let limits = UntrustedLimits::default();
        ingest_put_dir(packs, backend, mint, LEASE, dir, &self.action, &limits).await
    }

    async fn run(&self) -> Ingested {
        self.run_dir(&self.dir).await
    }

    /// Asserts that the run was refused for `reason` alone, and that nothing of it is indexed.
    fn refused(&self, got: Ingested, reason: RejectReason) {
        assert_eq!(got, Ingested::Reject(reason));
        let metrics = self.rig.packs.metrics();
        for each in RejectReason::ALL {
            assert_eq!(
                metrics.ingest_rejects(each),
                u64::from(each == reason),
                "{each:?}"
            );
        }
        assert_eq!((metrics.ingests(), metrics.ingest_put_dirs()), (0, 0));
        assert_eq!(metrics.put_dir_records(), 0);
        assert!(self.rig.index.has_none(&self.p.digests()));
    }
}

/// `multi`'s manifest of `packs` packs and the large objects `huge`, for `action`.
fn multi_manifest(p: &Parts, action: Digest, packs: u8, huge: &[(u8, Digest)]) -> TaskManifest {
    // `Multi::manifest`, for an action of this farm's.
    let file = |path: String, digest: Digest| kbf_pack::OutputFile {
        path,
        digest,
        is_executable: false,
    };
    let mut output_files = vec![
        file("out/alpha".to_owned(), sha256(&p.alpha)),
        file("out/beta".to_owned(), sha256(&p.beta)),
    ];
    for (slot, digest) in huge {
        output_files.push(file(format!("out/big{slot}"), *digest));
    }
    TaskManifest {
        lease: LEASE.to_owned(),
        action,
        exit_code: 0,
        stdout: Some(sha256(&p.stdout)),
        stderr: None,
        output_files,
        output_directories: vec![kbf_pack::OutputDirectory {
            path: "out/d".to_owned(),
            tree: p.tree.0,
        }],
        output_symlinks: Vec::new(),
        timing: kbf_pack::Timing::default(),
        usage: kbf_pack::Usage::default(),
        packs,
        large: huge
            .iter()
            .map(|&(slot, digest)| kbf_pack::LargeEntry { slot, digest })
            .collect(),
        inputs: None,
        objects: None,
    }
}

/// Asserts that a run of two packs committed into its put directory is ingested into the result
/// its manifest describes and cached; that both packs are recorded where they lie, written by
/// `b/<lease>`, and every record of them is indexed; and that the ingest read no record but
/// pack 0's manifest: of `pack1` only its footer, of `pack` only its footer and record 0 (and the
/// output tree, which the action cache's record of the result reads).
#[tokio::test]
async fn a_two_pack_put_dir_run_is_ingested_from_its_manifest_and_footers() {
    let farm = PutDir::new().await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert!(farm.rig.index.has_all(&farm.p.digests()));
    for slot in [Slot::Pack(0), Slot::Pack(1)] {
        let info = farm.rig.index.pack(&farm.key(slot)).expect("asked");
        assert_eq!(info.expect("recorded").writer, format!("b/{LEASE}"));
    }
    let p = &farm.p;
    let one_records = (p.alpha.len() + p.in_dir.len()) as u64;
    let zero_records = zero.len() as u64 - footer_len(4);
    let manifest_len = zero_records - (p.tree.1.len() + p.beta.len() + p.stdout.len()) as u64;
    let tree_at = zero
        .windows(p.tree.1.len())
        .position(|window| window == &p.tree.1[..])
        .expect("the tree is in pack 0") as u64;
    for (key, offset, len) in farm.rig.store.ranges() {
        if key == farm.key(Slot::Pack(1)) {
            assert!(offset >= one_records, "{key} {offset} {len}");
        } else if key == farm.key(Slot::Pack(0)) {
            let manifest = (offset, len) == (0, manifest_len);
            // The action cache's record of the result reads the output tree (a read from a
            // pack, so hashed) to list its files: that read is the cache's, not the ingest's.
            let tree = (offset, len) == (tree_at, p.tree.1.len() as u64);
            assert!(
                manifest || tree || offset >= zero_records,
                "{key} {offset} {len}"
            );
        }
    }
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingests(), 1);
    assert_eq!(metrics.ingest_put_dirs(), 1);
    assert_eq!(metrics.ingest_packs(), 2);
    assert_eq!(metrics.put_dir_records(), 2);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    assert!(
        scrape.contains("kbf_pack_ingest_put_dirs_total 1\n"),
        "{scrape}"
    );
    assert!(
        scrape.contains("kbf_pack_put_dir_records_total 2\n"),
        "{scrape}"
    );
}

/// Asserts that ingesting the same put directory again (a frontend that restarted between the
/// ingest and the action cache) gives the same result and records nothing twice: the index
/// answers the same packs as recorded before.
#[tokio::test]
async fn a_second_ingest_of_the_same_dir_is_the_same_result() {
    let farm = PutDir::new().await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };

    assert_eq!(farm.run().await, want);
    assert_eq!(farm.run().await, want);

    let metrics = farm.rig.packs.metrics();
    assert_eq!((metrics.ingests(), metrics.ingest_put_dirs()), (2, 2));
    assert_eq!(metrics.put_dir_records(), 2);
}

/// Asserts the lighter proof's two rules: a record whose bytes are not the digest its footer
/// names is recorded all the same (no record is read), and a digest another pack recorded first
/// keeps that first location, so a put directory cannot move it.
#[tokio::test]
async fn the_footer_is_recorded_unread_and_the_first_location_of_a_digest_wins() {
    let farm = PutDir::new().await;
    let p = &farm.p;
    let elsewhere = NewPack {
        key: format!("cas/f/front/{}", "e".repeat(8)),
        writer: "f/front".to_owned(),
        size: p.beta.len() as u64,
        blobs: vec![PackBlob {
            digest: BlobDigest {
                hash: sha256(&p.beta).hash,
                size: p.beta.len() as u64,
            },
            offset: 0,
        }],
    };
    farm.rig.index.record_pack(&elsewhere).expect("recorded");
    let manifest = farm.manifest(1, &[]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    let mut zero = Multi::pack_zero(&manifest, &records);
    // `alpha`'s bytes, after the manifest, become other bytes of the same length.
    let at = zero
        .windows(p.alpha.len())
        .position(|window| window == &p.alpha[..])
        .expect("alpha is in the pack");
    zero[at] ^= 0xff;
    farm.put(Slot::Pack(0), &zero).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { .. }), "{got:?}");
    let located = farm
        .rig
        .index
        .where_is(&[sha256(&p.beta), sha256(&p.alpha)]);
    assert_eq!(located[0].as_ref().expect("beta").pack_key, elsewhere.key);
    assert_eq!(
        located[1].as_ref().expect("alpha").pack_key,
        farm.key(Slot::Pack(0))
    );
}

/// Asserts that a large object committed as `L0` is streamed into the CAS under its digest and
/// the run's result names it, as in a run of presigned writes.
#[tokio::test]
async fn a_large_object_of_the_dir_is_stored_in_the_cas() {
    let farm = PutDir::new().await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[(0, large().1)]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(Slot::Pack(0), &Multi::pack_zero(&manifest, &records))
        .await;
    farm.put(Slot::Large(0), &large().0).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert!(farm.rig.node_holds(&large().1).await);
    assert_eq!(
        farm.rig.packs.metrics().ingest_large_bytes(),
        large().0.len() as u64
    );
}

/// Asserts that a Command of REAPI before 2.1, which declares `output_files` and
/// `output_directories` and no `output_paths`, is read for the declared paths too.
#[tokio::test]
async fn the_outputs_of_a_command_before_output_paths_are_declared() {
    let farm = PutDir::declaring(&DECLARED, true).await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
}

/// Asserts that an output the Command does not declare (a file, a directory, a symlink) makes
/// the run a `BadManifest`, and nothing of it is indexed or cached.
#[tokio::test]
async fn an_output_the_command_does_not_declare_is_a_bad_manifest() {
    for missing in ["out/alpha", "out/d", "out/link"] {
        let declared: Vec<&str> = DECLARED.iter().copied().filter(|p| *p != missing).collect();
        let farm = PutDir::declaring(&declared, false).await;
        let (mut manifest, _, one) = farm.two_packs();
        manifest.output_symlinks.push(kbf_pack::OutputSymlink {
            path: "out/link".to_owned(),
            target: "alpha".to_owned(),
        });
        let p = &farm.p;
        let zero = Multi::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
        farm.put(Slot::Pack(0), &zero).await;
        farm.put(Slot::Pack(1), &one).await;

        let got = farm.run().await;

        farm.refused(got, RejectReason::BadManifest);
    }
}

/// Asserts that an Action whose Command cannot be read as one is a `BadManifest` (the outputs
/// cannot be checked), and that a Command the store does not hold is `Unavailable`.
#[tokio::test]
async fn a_command_that_cannot_be_checked_refuses_or_waits() {
    let junk = Bytes::from_static(b"\xff\xff not a command");
    let farm = PutDir::with_command(Some(junk.clone()), sha256(&junk)).await;
    let (_, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    farm.refused(farm.run().await, RejectReason::BadManifest);

    let farm = PutDir::with_command(None, blob("a command nobody stored").0).await;
    let (_, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    let got = farm.run().await;
    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
    assert!(farm.rig.index.has_none(&farm.p.digests()));
}

/// Asserts that a run whose `pack` was never committed is `Missing`, and one whose manifest
/// names a `pack1` that was not is `Incomplete`, nothing indexed either way.
#[tokio::test]
async fn a_pack_not_committed_is_missing_or_incomplete() {
    let farm = PutDir::new().await;
    farm.refused(farm.run().await, RejectReason::Missing);

    let farm = PutDir::new().await;
    let (_, zero, _) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.refused(farm.run().await, RejectReason::Incomplete);
    assert!(
        farm.rig
            .index
            .pack(&farm.key(Slot::Pack(0)))
            .expect("asked")
            .is_none()
    );
}

/// Asserts that a directory this frontend did not mint for this lease and action is
/// `WrongKey` before the store is asked: another secret's, another lease's, or one of another
/// action; and that one minted as long ago as the janitor's least grace is `Expired`.
#[tokio::test]
async fn a_dir_not_minted_here_for_this_run_is_refused_unread() {
    let farm = PutDir::new().await;
    let (_, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    let other_secret = PackKeyMint::new(&[9; 32]).expect("a mint");
    let dirs = [
        other_secret.mint_put_dir(LEASE, &farm.action, now()),
        farm.mint
            .mint_put_dir("00000000000000010000000000000003", &farm.action, now()),
        farm.mint
            .mint_put_dir(LEASE, &blob("another action").0, now()),
    ];
    for dir in dirs {
        let dir = dir.expect("a dir");
        assert_eq!(
            farm.run_dir(&dir).await,
            Ingested::Reject(RejectReason::WrongKey)
        );
    }
    assert!(
        farm.rig.store.ranges().is_empty(),
        "{:?}",
        farm.rig.store.ranges()
    );

    let old = now() - MIN_GRACE - Duration::from_secs(60);
    let dir = farm
        .mint
        .mint_put_dir(LEASE, &farm.action, old)
        .expect("a dir");
    assert_eq!(
        farm.run_dir(&dir).await,
        Ingested::Reject(RejectReason::Expired)
    );
    assert!(farm.rig.store.ranges().is_empty());
}

/// Asserts that the put directory is the lease's and that its keys are the names `kbf-task`
/// commits, so that the store's error for one is not taken for another's.
#[tokio::test]
async fn the_dir_is_the_leases_and_its_keys_are_the_task_names() {
    let farm = PutDir::new().await;
    assert!(
        farm.dir.dir().starts_with(&format!("cas/b/{LEASE}/")),
        "{}",
        farm.dir.dir()
    );
    assert_eq!(farm.dir.build(), LEASE);
    let set = farm.dir.key_set();
    assert_eq!(set.packs().len(), 8);
    assert_eq!(set.large().len(), 8);
    let names: Vec<String> = set
        .packs()
        .iter()
        .chain(set.large())
        .map(|key| {
            key.key()
                .trim_start_matches(&format!("{}/", farm.dir.dir()))
                .to_owned()
        })
        .collect();
    let want = [
        "pack", "pack1", "pack2", "pack3", "pack4", "pack5", "pack6", "pack7", "L0", "L1", "L2",
        "L3", "L4", "L5", "L6", "L7",
    ];
    assert_eq!(names, want);
    let missing = farm.cached_code().await;
    assert_eq!(missing, Code::NotFound);
}

impl PutDir {
    async fn cached_code(&self) -> Code {
        let request = reapi::GetActionResultRequest {
            action_digest: Some(wire(&self.action)),
            ..reapi::GetActionResultRequest::default()
        };
        let mut client = reapi::action_cache_client::ActionCacheClient::new(self.rig.front.clone());
        match client.get_action_result(request).await {
            Ok(_) => Code::Ok,
            Err(status) => status.code(),
        }
    }
}

/// Asserts that an Action that does not decode, or names no Command, is a `BadManifest`: the
/// outputs cannot be checked against what it declares.
#[tokio::test]
async fn an_action_whose_command_cannot_be_found_is_a_bad_manifest() {
    let command = command_bytes(&DECLARED, false);
    let named = sha256(&command);
    let nameless = reapi::Action {
        input_root_digest: Some(wire(&blob("the inputs").0)),
        ..reapi::Action::default()
    };
    for action in [
        Bytes::from_static(b"\xff\xff not an action"),
        Bytes::from(nameless.encode_to_vec()),
    ] {
        let farm = PutDir::with_action(action, Some(command.clone()), named).await;
        let (_, zero, one) = farm.two_packs();
        farm.put(Slot::Pack(0), &zero).await;
        farm.put(Slot::Pack(1), &one).await;
        farm.refused(farm.run().await, RejectReason::BadManifest);
    }
}

/// Asserts that a pack index that fails the record leaves the run `Unavailable`, with nothing
/// counted as recorded and nothing cached; the same directory is ingested once the index
/// answers.
#[tokio::test]
async fn an_index_that_fails_the_record_leaves_the_run_unavailable() {
    let farm = PutDir::new().await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    farm.rig.index.fail_record(1);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().put_dir_records(), 0);
    assert_eq!(farm.cached_code().await, Code::NotFound);
    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(farm.run().await, want);
}

// ---- the one-pass object ----

use kbf_pack::one_pass::ObjectWriter;

/// A one-pass object of `manifest` and `records`, each written whole.
fn one_pass_of(manifest: &TaskManifest, records: &[&[u8]]) -> Vec<u8> {
    let json = manifest.to_json_with(512 << 20).expect("a manifest");
    one_pass_indexed(&json, records, &index_for(manifest, records))
}

/// The neutral file index of `manifest`'s outputs: its files and symlinks, and for each output
/// directory the files of its `Tree` (which `records` holds; the tests' trees are one directory).
fn index_for(manifest: &TaskManifest, records: &[&[u8]]) -> Vec<u8> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let file = |path: &str, digest: &Digest, exec: bool| {
        let offset = if digest.size_bytes == 0 {
            String::new()
        } else {
            ",\"offset\":0".to_owned()
        };
        format!(
            r#"{{"path":"{path}","size":{},"exec":{exec},"sha256":"{}"{offset}}}"#,
            digest.size_bytes,
            digest.hash_hex()
        )
    };
    for out in &manifest.output_files {
        files.push((
            out.path.clone(),
            file(&out.path, &out.digest, out.is_executable),
        ));
    }
    for dir in &manifest.output_directories {
        dirs.push(format!("\"{}\"", dir.path));
        let record = records
            .iter()
            .find(|r| sha256(r) == dir.tree)
            .expect("the tree is a record");
        let tree = reapi::Tree::decode(*record).expect("a tree");
        for node in tree.root.expect("root").files {
            let digest = kbf_types::Digest::parse(
                kbf_types::DigestFunction::Sha256,
                &format!(
                    "{}/{}",
                    node.digest.as_ref().expect("digest").hash,
                    node.digest.as_ref().expect("digest").size_bytes
                ),
            )
            .expect("a digest");
            let path = format!("{}/{}", dir.path, node.name);
            files.push((path.clone(), file(&path, &digest, node.is_executable)));
        }
    }
    files.sort();
    let files: Vec<String> = files.into_iter().map(|(_, json)| json).collect();
    format!(
        r#"{{"v":1,"files":[{}],"symlinks":[],"dirs":[{}]}}"#,
        files.join(","),
        dirs.join(",")
    )
    .into_bytes()
}

/// A one-pass object of the manifest `json`, `records` and the file index `index`.
fn one_pass_indexed(json: &[u8], records: &[&[u8]], index: &[u8]) -> Vec<u8> {
    let mut writer = ObjectWriter::new(Vec::new());
    for record in records {
        writer.blob(&sha256(record), record).expect("written");
    }
    writer.finish(json, index).expect("finished").0
}

/// A one-pass object whose manifest record is `json`, as it is.
fn one_pass_json(json: &[u8], records: &[&[u8]]) -> Vec<u8> {
    let mut writer = ObjectWriter::new(Vec::new());
    for record in records {
        writer.blob(&sha256(record), record).expect("written");
    }
    writer.finish(json, b"").expect("finished").0
}

impl PutDir {
    /// The manifest of an honest run of one object: `multi`'s outputs, in one pack.
    fn one_object_manifest(&self) -> TaskManifest {
        self.manifest(1, &[])
    }

    /// The records of an honest run.
    fn records(&self) -> [&[u8]; 5] {
        let p = &self.p;
        [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir]
    }

    /// Ingests with `limits`.
    async fn run_with(&self, limits: &UntrustedLimits) -> Ingested {
        let (packs, backend, mint) = (&self.rig.packs, &self.backend, &self.mint);
        ingest_put_dir(packs, backend, mint, LEASE, &self.dir, &self.action, limits).await
    }
}

/// Asserts that a one-pass object is ingested into the result its manifest describes and cached;
/// that its one object is recorded, written by `b/<lease>`, with a row for every record; and that
/// the ingest read the trailer, then the manifest and the index in one range, and no record (but
/// the output tree, which the action cache's record of the result reads).
#[tokio::test]
async fn a_one_pass_object_is_ingested_from_its_trailer_manifest_and_index() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let object = one_pass_of(&manifest, &farm.records());
    farm.put(Slot::Pack(0), &object).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert!(farm.rig.index.has_all(&farm.p.digests()));
    let info = farm
        .rig
        .index
        .pack(&farm.key(Slot::Pack(0)))
        .expect("asked");
    assert_eq!(info.expect("recorded").writer, format!("b/{LEASE}"));
    let p = &farm.p;
    let end = kbf_pack::one_pass::TAIL_LEN;
    let trailer =
        kbf_pack::one_pass::Trailer::parse(&object[object.len() - end..], object.len() as u64)
            .expect("an honest end");
    let tree_at = object
        .windows(p.tree.1.len())
        .position(|window| window == &p.tree.1[..])
        .expect("the tree is in the object") as u64;
    let ranges = farm.rig.store.ranges();
    assert_eq!(
        ranges[0],
        (
            farm.key(Slot::Pack(0)),
            object.len() as u64 - end as u64,
            end as u64
        )
    );
    assert_eq!(
        ranges[1],
        (
            farm.key(Slot::Pack(0)),
            trailer.manifest_offset,
            object.len() as u64 - trailer.manifest_offset
        )
    );
    for (key, offset, len) in &ranges[2..] {
        if *key == farm.key(Slot::Pack(0)) {
            assert_eq!((*offset, *len), (tree_at, p.tree.1.len() as u64));
        }
    }
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingests(), 1);
    assert_eq!(metrics.ingest_put_dirs(), 1);
    assert_eq!(metrics.ingest_packs(), 1);
    assert_eq!(metrics.put_dir_records(), 1);
}

/// Asserts the rollout rule (docs/design/one-pass-task-object.md section 6): one frontend ingests
/// the packs of the format before and the one-pass object of this one, each into the same result.
#[tokio::test]
async fn one_pass_and_packs_ingest_side_by_side() {
    let old = PutDir::new().await;
    let (manifest, zero, one) = old.two_packs();
    old.put(Slot::Pack(0), &zero).await;
    old.put(Slot::Pack(1), &one).await;
    let new = PutDir::new().await;
    new.put(
        Slot::Pack(0),
        &one_pass_of(&new.one_object_manifest(), &new.records()),
    )
    .await;

    let (before, after) = (old.run().await, new.run().await);

    assert_eq!(before, after);
    assert_eq!(
        before,
        Ingested::Ran {
            result: Box::new(result_of(&manifest)),
            cached: true,
        }
    );
}

/// Asserts that a one-pass object has no length cap of the packs' (`max_pack_bytes`), while the
/// pack of the format before still has one.
#[tokio::test]
async fn a_one_pass_object_has_no_length_cap_and_a_pack_still_has() {
    let limits = UntrustedLimits {
        max_pack_bytes: 1,
        ..UntrustedLimits::default()
    };
    let farm = PutDir::new().await;
    farm.put(
        Slot::Pack(0),
        &one_pass_of(&farm.one_object_manifest(), &farm.records()),
    )
    .await;
    assert!(matches!(farm.run_with(&limits).await, Ingested::Ran { .. }));

    let farm = PutDir::new().await;
    let (_, zero, one) = farm.two_packs();
    farm.put(Slot::Pack(0), &zero).await;
    farm.put(Slot::Pack(1), &one).await;
    let got = farm.run_with(&limits).await;
    farm.refused(got, RejectReason::TooLarge);
}

/// Asserts that a blob over the record cap is an entry of the one object: its row is recorded
/// where it lies (not at offset 0, after other records), the node is given it by a ranged read
/// of the object, and its bytes are counted as a large object's are.
#[tokio::test]
async fn a_large_blob_is_an_entry_of_the_object_and_is_read_by_range() {
    let farm = PutDir::new().await;
    let mut manifest = farm.one_object_manifest();
    manifest.output_files.push(kbf_pack::OutputFile {
        path: "out/big0".to_owned(),
        digest: large().1,
        is_executable: false,
    });
    let mut records: Vec<&[u8]> = farm.records().to_vec();
    records.push(&large().0);
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &records))
        .await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    let row = farm.rig.index.where_is(&[large().1]);
    let row = row[0].as_ref().expect("the large blob has a row");
    assert_eq!(row.pack_key, farm.key(Slot::Pack(0)));
    assert!(row.offset > 0);
    assert!(farm.rig.node_holds(&large().1).await);
    assert_eq!(
        farm.rig.packs.metrics().ingest_large_bytes(),
        large().0.len() as u64
    );
    let past = farm.rig.store.ranges();
    assert!(
        past.iter()
            .any(|(key, offset, _)| *key == farm.key(Slot::Pack(0)) && *offset >= row.offset),
        "{past:?}"
    );
}

/// Asserts that bytes of a duplicate that sit between two records (a gap no entry names) are
/// accepted, and the object is ingested as the object without them is.
#[tokio::test]
async fn dead_bytes_between_records_are_accepted() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let mut writer = ObjectWriter::new(Vec::new());
    for record in farm.records() {
        let mut at = writer.begin().expect("begun");
        writer.chunk(&mut at, record).expect("chunk");
        writer.end(at, &sha256(record)).expect("ended");
        // The same bytes again: written, not indexed.
        let mut again = writer.begin().expect("begun");
        writer.chunk(&mut again, record).expect("chunk");
        writer.end(again, &sha256(record)).expect("ended");
    }
    let object = writer
        .finish(
            &manifest.to_json().expect("json"),
            &index_for(&manifest, &farm.records()),
        )
        .expect("done")
        .0;
    farm.put(Slot::Pack(0), &object).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { .. }), "{got:?}");
    assert!(farm.rig.index.has_all(&farm.p.digests()));
}

/// Asserts the refusals of a one-pass object's own rules, each before any record is read and
/// with nothing indexed: a damaged manifest, a damaged end record, an entry of method 8 (deflate:
/// the format has no compression), a directory past the entry limit, a manifest past the cap.
#[tokio::test]
async fn a_damaged_or_oversize_one_pass_object_is_refused_unread() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let honest = one_pass_of(&manifest, &farm.records());
    let trailer = kbf_pack::one_pass::Trailer::parse(
        &honest[honest.len() - kbf_pack::one_pass::TAIL_LEN..],
        honest.len() as u64,
    )
    .expect("an honest end");

    // The manifest member's bytes, a locator field, and the first central directory entry's
    // method: 8 is deflate, which this format never has, so the object is refused whatever its
    // bytes are.
    let mut manifest_bytes = honest.clone();
    manifest_bytes[trailer.manifest_offset as usize] ^= 1;
    let mut locator = honest.clone();
    let at = locator.len() - kbf_pack::one_pass::TAIL_LEN + 104;
    locator[at] ^= 1;
    let mut deflated = honest.clone();
    deflated[trailer.cd_offset as usize + 10] = 8;
    for bytes in [manifest_bytes, locator, deflated] {
        let farm = PutDir::new().await;
        farm.put(Slot::Pack(0), &bytes).await;
        let got = farm.run().await;
        farm.refused(got, RejectReason::Malformed);
    }

    let farm = PutDir::new().await;
    farm.put(Slot::Pack(0), &honest).await;
    let few = UntrustedLimits {
        max_object_entries: 4,
        ..UntrustedLimits::default()
    };
    let got = farm.run_with(&few).await;
    farm.refused(got, RejectReason::TooLarge);

    let farm = PutDir::new().await;
    let mut writer = ObjectWriter::new(Vec::new());
    writer
        .blob(&sha256(&farm.p.beta), &farm.p.beta)
        .expect("written");
    let big = writer
        .finish(&vec![b' '; (4 << 20) + 1], b"")
        .expect("done")
        .0;
    farm.put(Slot::Pack(0), &big).await;
    let got = farm.run().await;
    farm.refused(got, RejectReason::TooLarge);
}

/// Asserts the manifest's own rules for a one-pass object: another lease is `WrongKey`; more
/// than one pack, or a large entry, is `BadManifest`; a digest it names that no record holds is
/// `Incomplete`; and an output the Command does not declare is `BadManifest`.
#[tokio::test]
async fn a_one_pass_manifest_is_held_to_its_lease_its_one_object_and_its_records() {
    let honest = PutDir::new().await;
    let base = honest.one_object_manifest();
    let mut other_lease = base.clone();
    other_lease.lease = "00000000000000010000000000000003".to_owned();
    // A manifest of version 2 that says two packs, which the writer's own rules would not write.
    let mut stats = base.clone();
    stats.inputs = Some(kbf_pack::InputStats {
        from: Some(kbf_pack::InputsFrom::Tree),
        ..kbf_pack::InputStats::default()
    });
    let json = stats.to_json().expect("json");
    let text = String::from_utf8(json).expect("utf-8");
    assert!(text.contains("\"packs\":1"));
    let two_packs = text.replace("\"packs\":1", "\"packs\":2").into_bytes();
    let with_large = honest.manifest(1, &[(0, large().1)]);
    let records = honest.records();
    let cases = [
        (one_pass_of(&other_lease, &records), RejectReason::WrongKey),
        (
            one_pass_json(&two_packs, &records),
            RejectReason::BadManifest,
        ),
        (
            one_pass_of(&with_large, &records),
            RejectReason::BadManifest,
        ),
    ];
    for (object, reason) in cases {
        let farm = PutDir::new().await;
        farm.put(Slot::Pack(0), &object).await;
        let got = farm.run().await;
        farm.refused(got, reason);
    }

    // `beta` is named and no record holds it.
    let farm = PutDir::new().await;
    let p = &farm.p;
    let records: [&[u8]; 4] = [&p.alpha, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(Slot::Pack(0), &one_pass_of(&base, &records)).await;
    let got = farm.run().await;
    farm.refused(got, RejectReason::Incomplete);

    // The Command does not declare `out/alpha`.
    let declared: Vec<&str> = DECLARED
        .iter()
        .copied()
        .filter(|p| *p != "out/alpha")
        .collect();
    let farm = PutDir::declaring(&declared, false).await;
    let manifest = farm.one_object_manifest();
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &farm.records()))
        .await;
    let got = farm.run().await;
    farm.refused(got, RejectReason::BadManifest);
}

/// Asserts that a manifest that is not JSON, in an object whose CRCs are right, is `BadManifest`.
#[tokio::test]
async fn a_one_pass_manifest_that_is_not_a_manifest_is_bad() {
    let farm = PutDir::new().await;
    let mut writer = ObjectWriter::new(Vec::new());
    writer
        .blob(&sha256(&farm.p.beta), &farm.p.beta)
        .expect("written");
    let object = writer.finish(b"{not json", b"").expect("done").0;
    farm.put(Slot::Pack(0), &object).await;
    let got = farm.run().await;
    farm.refused(got, RejectReason::BadManifest);
}

/// Asserts that the range a one-pass object's end asks the frontend to read is bounded by what
/// the entry limit allows, whatever the file index holds: a file index of 5 MiB in an object of
/// seven entries is `TooLarge` when the limit is seven, and read when it is more.
#[tokio::test]
async fn a_file_index_longer_than_the_entry_limit_allows_is_too_large_unread() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let mut writer = ObjectWriter::new(Vec::new());
    for record in farm.records() {
        writer.blob(&sha256(record), record).expect("written");
    }
    let json = manifest.to_json().expect("json");
    let mut index = index_for(&manifest, &farm.records());
    index.extend(vec![b' '; 5 << 20]);
    let object = writer.finish(&json, &index).expect("done").0;
    farm.put(Slot::Pack(0), &object).await;
    let seven = UntrustedLimits {
        max_object_entries: 7,
        ..UntrustedLimits::default()
    };
    let got = farm.run_with(&seven).await;
    farm.refused(got, RejectReason::TooLarge);
    // The index is not JSON the frontend reads: with room for it the object is ingested.
    assert!(matches!(farm.run().await, Ingested::Ran { .. }));
}

// ---- the file index against the manifest, and the manifests it publishes ----

/// Asserts that an object whose file index disagrees with its manifest is a `BadManifest`: a
/// Tree derived from the index that is not the Tree the manifest names, a top-level file whose
/// digest or mode differs, a symlink the index does not have, an index that is not an index, and
/// an index with no output directory the manifest names.
#[tokio::test]
async fn a_file_index_that_disagrees_with_the_manifest_is_a_bad_manifest() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let records = farm.records();
    let honest = String::from_utf8(index_for(&manifest, &records)).expect("utf-8");
    let beta = farm.p.beta.len();
    let tree_file = format!("out/d/{}", sha256(&farm.p.in_dir).hash_hex());
    let mut with_link = manifest.clone();
    with_link.output_symlinks.push(kbf_pack::OutputSymlink {
        path: "out/link".to_owned(),
        target: "alpha".to_owned(),
    });
    let cases: Vec<(&str, TaskManifest, String)> = vec![
        (
            "another mode",
            manifest.clone(),
            honest.replacen("\"exec\":false", "\"exec\":true", 1),
        ),
        (
            "another digest",
            manifest.clone(),
            honest.replace(
                &sha256(&farm.p.beta).hash_hex(),
                &sha256(&farm.p.alpha).hash_hex(),
            ),
        ),
        (
            "a file short",
            manifest.clone(),
            honest.replacen(
                &format!("{{\"path\":\"{tree_file}\""),
                "{\"path\":\"out/d/other\"",
                1,
            ),
        ),
        ("a symlink the index lacks", with_link, honest.clone()),
        ("not JSON", manifest.clone(), "{not json".to_owned()),
        (
            "no directory",
            manifest.clone(),
            honest.replace("\"dirs\":[\"out/d\"]", "\"dirs\":[]"),
        ),
    ];
    let _ = beta;
    for (what, manifest, index) in cases {
        let farm = PutDir::new().await;
        let json = manifest.to_json_with(512 << 20).expect("json");
        let object = one_pass_indexed(&json, &farm.records(), index.as_bytes());
        farm.put(Slot::Pack(0), &object).await;
        let got = farm.run().await;
        assert_eq!(got, Ingested::Reject(RejectReason::BadManifest), "{what}");
        assert!(farm.rig.index.has_none(&farm.p.digests()), "{what}");
    }
}

/// Asserts that a file the index names that no record holds is `Incomplete`, though the manifest
/// names nothing of it (a file inside an output directory whose Tree, built from the index, would
/// name a blob nobody can read): here the tree's file, absent from the object's records.
#[tokio::test]
async fn an_indexed_file_no_record_holds_is_incomplete() {
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    let p = &farm.p;
    // The Tree record is there, the file it names is not.
    let records: [&[u8]; 4] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1];
    let object = one_pass_indexed(
        &manifest.to_json_with(512 << 20).expect("json"),
        &records,
        &index_for(&manifest, &farm.records()),
    );
    farm.put(Slot::Pack(0), &object).await;
    let got = farm.run().await;
    farm.refused(got, RejectReason::Incomplete);
}

/// Asserts that, given the frontend's tree writer, a one-pass object's output directory is
/// published as a tierstore manifest: stored under `cas/l/<sha256>`, read by tierstore's own
/// reader, naming each file with `source` the span of this object (its key, and the offset the
/// pack index records), so that a later input root holding the same directory finds it in the
/// writer's memo; and that a store that refuses the write costs the reuse, never the run.
#[tokio::test]
async fn an_output_directory_is_published_as_a_tierstore_manifest_pointing_into_the_object() {
    use kbf_front::action_tree::TreeWriter;
    use kbf_front::ingest_put_dir_with;
    use kbf_objstore::{ByteRange, ObjectKey, ObjectStore as _};

    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &farm.records()))
        .await;
    let clock: kbf_front::Clock = std::sync::Arc::new(std::time::SystemTime::now);
    let writer = TreeWriter::new(
        std::sync::Arc::new(farm.backend.clone()),
        std::sync::Arc::new(farm.rig.packs.clone()),
        clock,
    );
    let (packs, backend, mint) = (&farm.rig.packs, &farm.backend, &farm.mint);
    let limits = UntrustedLimits::default();
    let got = ingest_put_dir_with(
        packs,
        backend,
        mint,
        LEASE,
        &farm.dir,
        &farm.action,
        &limits,
        Some(&writer),
    )
    .await;
    assert!(matches!(got, Ingested::Ran { .. }), "{got:?}");

    let keys: Vec<String> = farm
        .rig
        .store
        .keys()
        .await
        .into_iter()
        .filter(|k| k.starts_with("cas/l/"))
        .collect();
    assert_eq!(keys.len(), 1, "one directory, one manifest: {keys:?}");
    let key = ObjectKey::new(keys[0].clone()).expect("a key");
    let len = farm.rig.store.len(&key).await.expect("len").expect("there");
    let bytes = farm
        .rig
        .store
        .get_range(&key, ByteRange::new(0, len).expect("range"))
        .await
        .expect("read");
    let digest = tierstore::TreeDigest::of(&bytes);
    assert_eq!(keys[0], format!("cas/l/{digest}"));
    let children = tierstore::parse_manifest(&bytes, &digest).expect("tierstore's reader takes it");
    assert_eq!(children.children.len(), 1);
    let at = farm.rig.index.where_is(&[sha256(&farm.p.in_dir)]);
    let at = at[0].as_ref().expect("the file's row");
    let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
    assert!(
        text.contains(&format!(
            "\"source\":{{\"key\":\"{}\",\"offset\":{}}}",
            farm.key(Slot::Pack(0)),
            at.offset
        )),
        "{text}"
    );
}

/// A store that refuses every write and holds nothing.
struct Refusing;

impl kbf_front::large_backfill::LargeStore for Refusing {
    fn len<'a>(
        &'a self,
        _: &'a kbf_objstore::ObjectKey,
    ) -> futures::future::BoxFuture<'a, Result<Option<u64>, kbf_objstore::ObjectStoreError>> {
        Box::pin(async { Ok(None) })
    }

    fn put<'a>(
        &'a self,
        _: &'a kbf_objstore::ObjectKey,
        _: u64,
        _: kbf_objstore::BodyStream,
    ) -> futures::future::BoxFuture<'a, Result<(), kbf_objstore::ObjectStoreError>> {
        Box::pin(async { Err(kbf_objstore::ObjectStoreError::Body("refused".to_owned())) })
    }
}

#[tokio::test]
async fn a_manifest_that_cannot_be_published_costs_the_reuse_and_not_the_run() {
    use kbf_front::action_tree::TreeWriter;
    let farm = PutDir::new().await;
    let manifest = farm.one_object_manifest();
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &farm.records()))
        .await;
    let clock: kbf_front::Clock = std::sync::Arc::new(std::time::SystemTime::now);
    let writer = TreeWriter::new(
        std::sync::Arc::new(farm.backend.clone()),
        std::sync::Arc::new(Refusing),
        clock,
    );
    let (packs, backend, mint) = (&farm.rig.packs, &farm.backend, &farm.mint);
    let limits = UntrustedLimits::default();
    let got = kbf_front::ingest_put_dir_with(
        packs,
        backend,
        mint,
        LEASE,
        &farm.dir,
        &farm.action,
        &limits,
        Some(&writer),
    )
    .await;
    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
    assert!(farm.rig.index.has_all(&farm.p.digests()));
}

/// Asserts that the two bounds of a one-pass object's tail are the limits the ingest is given: a
/// manifest cap below the manifest's length, and a per-record allowance too small for the tail,
/// each refuse the object as too large unread, and the defaults take it.
#[tokio::test]
async fn the_tail_bounds_are_the_limits_the_ingest_is_given() {
    let manifest = PutDir::new().await.one_object_manifest();
    let json_len = manifest.to_json().expect("json").len() as u64;
    let small_manifest = UntrustedLimits {
        max_object_manifest_bytes: json_len - 1,
        ..UntrustedLimits::default()
    };
    // The tail is the manifest, the file index and the central directory: the base is the
    // manifest cap, so a cap of the manifest alone and one byte for each of seven records is
    // too little.
    let tight = UntrustedLimits {
        max_object_manifest_bytes: json_len,
        max_object_entries: 7,
        object_bytes_per_entry: 1,
        ..UntrustedLimits::default()
    };
    for limits in [small_manifest, tight] {
        let farm = PutDir::new().await;
        farm.put(Slot::Pack(0), &one_pass_of(&manifest, &farm.records()))
            .await;
        let got = farm.run_with(&limits).await;
        farm.refused(got, RejectReason::TooLarge);
    }
    let farm = PutDir::new().await;
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &farm.records()))
        .await;
    assert!(matches!(farm.run().await, Ingested::Ran { .. }));
}

/// Asserts that a run with no output at all (a Command that wrote nothing it declared) is
/// ingested with no file index to check, and its result is cached.
#[tokio::test]
async fn a_one_pass_object_with_no_outputs_has_no_index_to_agree_with() {
    let farm = PutDir::new().await;
    let mut manifest = farm.one_object_manifest();
    manifest.stdout = None;
    manifest.output_files.clear();
    manifest.output_directories.clear();
    farm.put(Slot::Pack(0), &one_pass_of(&manifest, &[])).await;
    let got = farm.run().await;
    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
}
