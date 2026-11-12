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
