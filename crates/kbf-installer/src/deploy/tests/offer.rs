//! `deploy --offer`: each check of the offer, each against a farm that would otherwise
//! deploy cleanly, so a refusal can only be the check under test. Every refusal also
//! proves the farm was not touched (no upload, no frontend update).

use kbf_gate::{OfferDoc, SetHash};
use sha2::{Digest, Sha256};

use super::*;
use crate::deploy::offer::{self, Attest, HostProbe, ProbeFault, Stage};

mod in_bundle;

/// The farm's clock at the start of a run (`Clk`).
const NOW: u64 = 1_790_000_000;
const HOUR: u64 = 3600;

/// A probe that says what the test wants the staged `kbf` to say.
struct Says(Result<Option<u64>, String>);

impl Probe for Says {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        self.0.clone().map_err(ProbeFault::Failed)
    }

    /// The build this host runs: the `previous_meta_log` of `Fields::good()`.
    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        Ok(Some(5))
    }
}

/// What one attestation check was shown: the commit, the tarball and the attestation.
type Shown = (String, Vec<u8>, Vec<u8>);

/// An attestation check that says `verdict` and keeps what it was shown.
struct Vouches {
    verdict: Result<(), String>,
    seen: std::sync::Mutex<Vec<Shown>>,
}

impl Vouches {
    fn new(verdict: Result<(), String>) -> std::sync::Arc<Vouches> {
        std::sync::Arc::new(Vouches {
            verdict,
            seen: Default::default(),
        })
    }
}

impl Attest for std::sync::Arc<Vouches> {
    fn verify(&self, commit: &CommitId, tarball: &[u8], attestation: &[u8]) -> Result<(), String> {
        self.seen.lock().unwrap().push((
            commit.as_str().to_owned(),
            tarball.to_vec(),
            attestation.to_vec(),
        ));
        self.verdict.clone()
    }
}

fn text(fault: ProbeFault) -> String {
    match fault {
        ProbeFault::Unattested(s) | ProbeFault::Failed(s) => s,
    }
}

fn tar_name(component: &str, platform: &str) -> String {
    format!("{component}-{TARGET}-{platform}.tar.gz")
}

/// The files of a release: `kbf` for both platforms the farm runs.
fn files() -> Vec<(String, Vec<u8>)> {
    vec![
        (tar_name("kbf", "linux-x86_64"), b"linux".to_vec()),
        (tar_name("kbf", "darwin-arm64"), b"darwin".to_vec()),
    ]
}

fn sums_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut text = String::new();
    for (name, bytes) in files {
        text.push_str(&format!("{}  {name}\n", hex::encode(Sha256::digest(bytes))));
    }
    text.into_bytes()
}

/// A bundle of `files` and the checksum file `sums` (none: left out).
fn bundle_of(files: &[(String, Vec<u8>)], sums: Option<&[u8]>) -> Vec<u8> {
    let mut all: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    if let Some(sums) = sums {
        all.push(("SHA256SUMS", sums));
    }
    tarball(&all)
}

/// What an offer for the release says; each test changes the one field it is about.
struct Fields {
    target: &'static str,
    commit: &'static str,
    set_hash: String,
    created: u64,
    platforms: Vec<&'static str>,
    meta_log: Option<u64>,
    previous_meta_log: Option<u64>,
}

impl Fields {
    fn good() -> Fields {
        Fields {
            target: "gamma",
            commit: TARGET,
            set_hash: SetHash::of_sums(&sums_of(&files())).as_str().to_owned(),
            created: NOW - HOUR,
            platforms: vec!["darwin-arm64", "linux-x86_64"],
            meta_log: Some(4),
            previous_meta_log: Some(5),
        }
    }

    fn doc(&self) -> OfferDoc {
        let json = json!({
            "version": 1,
            "created": self.created,
            "target": self.target,
            "commit": self.commit,
            "set_hash": self.set_hash,
            "platforms": self.platforms,
            "beta": {"kind": "attested", "run_id": "77"},
            "meta_log": self.meta_log,
            "previous": OLD,
            "previous_meta_log": self.previous_meta_log,
            "reversible": kbf_gate::reversible(self.meta_log, self.previous_meta_log),
        });
        OfferDoc::from_bytes(json.to_string().as_bytes()).expect("a valid offer")
    }
}

fn policy(stage: Stage, doc: OfferDoc) -> OfferPolicy {
    OfferPolicy::Offer {
        stage,
        doc: Box::new(doc),
        max_age: Duration::from_secs(72 * HOUR),
    }
}

/// One deploy of the release under `policy`, with the staged `kbf` saying `says`.
fn deploy_with(
    policy: OfferPolicy,
    bundle: Option<Vec<u8>>,
    says: Result<Option<u64>, String>,
) -> (Farm, Summary) {
    deploy_probing(policy, bundle, &Says(says))
}

/// [`deploy_with`], with the probe of the test's choosing.
fn deploy_probing(
    policy: OfferPolicy,
    bundle: Option<Vec<u8>>,
    probe: &dyn Probe,
) -> (Farm, Summary) {
    deploy_mode(true, policy, bundle, probe)
}

/// [`deploy_probing`], as a plan (`apply` false) or an apply.
fn deploy_mode(
    apply: bool,
    policy: OfferPolicy,
    bundle: Option<Vec<u8>>,
    probe: &dyn Probe,
) -> (Farm, Summary) {
    let f = farm(two_nodes());
    let mut o = opts(apply);
    o.offer = policy;
    let clock = Clk(f.now.clone());
    let (smoke, check) = (FixedSmoke(SmokeResult::Pass), FixedCheck(Ok(true)));
    let observed = Obs(&f);
    let env = Env {
        api: &f,
        observe: &observed,
        clock: &clock,
        smoke: &smoke,
        check: &check,
        probe,
    };
    let mut input: &[u8] = bundle.as_deref().unwrap_or(&[]);
    let stream: Option<&mut dyn io::Read> =
        bundle.as_ref().map(|_| &mut input as &mut dyn io::Read);
    let s = deploy(&env, &o, stream);
    (f, s)
}

fn good_bundle() -> Vec<u8> {
    let files = files();
    bundle_of(&files, Some(&sums_of(&files)))
}

/// Deploy under `fields` with the good bundle and a staged `kbf` that says meta_log 4.
fn with(fields: &Fields) -> (Farm, Summary) {
    deploy_with(
        policy(Stage::Gamma, fields.doc()),
        Some(good_bundle()),
        Ok(Some(4)),
    )
}

/// Refused with `reason`, exit 3, and nothing uploaded, no update asked for.
fn refused(f: &Farm, s: &Summary, reason: &str) {
    assert_eq!((s.exit, s.reason.as_str()), (3, reason), "{}", s.line());
    assert_eq!(f.requests("PUT"), 0, "{}", s.line());
    assert_eq!(f.requests("POST"), 0, "{}", s.line());
}

/// Proves an offer that matches the stage, commit, set, files, `meta_log`, age and platforms
/// lets the deploy through, and that the summary records the offer's `reversible` and
/// `previous` (copied from the validated document) for the bake hand-off. Catches a check
/// that refuses a good offer, and a record that is dropped or read from elsewhere.
///
/// Mutants planted: the offer step not recording; `reversible` recorded as true always.
#[test]
fn an_offer_that_matches_lets_the_deploy_through_and_is_recorded() {
    let (f, s) = with(&Fields::good());
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(f.requests("PUT"), 2);
    let names: Vec<_> = s.steps.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names[..2], ["offer", "bundle"]);
    let rec = s.offer.expect("the offer is recorded");
    assert_eq!((rec.kind, rec.stage.as_str()), ("offer", "gamma"));
    assert_eq!(rec.commit.as_deref(), Some(TARGET));
    assert_eq!(rec.previous.as_deref(), Some(OLD));
    assert_eq!(rec.reversible, Some(true));
    assert_eq!((rec.meta_log, rec.previous_meta_log), (Some(4), Some(5)));
    assert_eq!(
        rec.set_hash.as_deref(),
        Some(Fields::good().set_hash.as_str())
    );
}

/// Proves an irreversible offer is recorded as such (the hand-off must not read it as yes).
///
/// Mutant planted: `reversible` recorded as true always.
#[test]
fn an_irreversible_offer_is_recorded_irreversible() {
    let mut fields = Fields::good();
    fields.meta_log = Some(6);
    let (_, s) = deploy_with(
        policy(Stage::Gamma, fields.doc()),
        Some(good_bundle()),
        Ok(Some(6)),
    );
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(s.offer.unwrap().reversible, Some(false));
}

/// (b) Proves an offer for the other stage is refused whichever way round: a prod offer on a
/// gamma host and a gamma offer on a prod host. Catches a target that is never compared, and
/// one compared against a constant.
///
/// Mutants planted: the target comparison removed; compared to Gamma always.
#[test]
fn an_offer_for_another_stage_is_refused() {
    let mut prod = Fields::good();
    prod.target = "prod";
    let (f, s) = with(&prod);
    refused(&f, &s, "offer_wrong_stage");
    let (f, s) = deploy_with(
        policy(Stage::Prod, Fields::good().doc()),
        Some(good_bundle()),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_wrong_stage");
    let (_, s) = deploy_with(
        policy(Stage::Prod, prod.doc()),
        Some(good_bundle()),
        Ok(Some(4)),
    );
    assert_eq!(s.exit, 0, "{}", s.line());
}

/// (c) Proves an offer for another commit is refused, whatever the bundle's file names say.
///
/// Mutant planted: the commit comparison removed.
#[test]
fn an_offer_for_another_commit_is_refused() {
    let mut fields = Fields::good();
    fields.commit = OLD;
    let (f, s) = with(&fields);
    refused(&f, &s, "offer_wrong_commit");
}

/// (c) Proves a bundle whose `SHA256SUMS` is not the offer's set is refused, though every
/// file name is right: the offer's `set_hash` is compared to the hash of the sums.
///
/// Mutants planted: the set comparison removed; the hash taken of the file list instead of
/// the sums' bytes.
#[test]
fn a_bundle_of_another_set_is_refused() {
    let files = files();
    let mut other = sums_of(&files);
    other.extend_from_slice(b"\n");
    let (f, s) = deploy_with(
        policy(Stage::Gamma, Fields::good().doc()),
        Some(bundle_of(&files, Some(&other))),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_set_mismatch");
}

/// (c) Proves a tarball that is not the file the set's sums name is refused, though the sums
/// are the offer's: the bytes are hashed, the file name is not believed. Also a tarball the
/// sums do not list.
///
/// Mutants planted: the digest comparison removed; an unlisted file allowed.
#[test]
fn a_tarball_the_set_does_not_name_is_refused() {
    let honest = files();
    let sums = sums_of(&honest);
    let mut swapped = honest.clone();
    swapped[0].1 = b"someone else's bytes".to_vec();
    let (f, s) = deploy_with(
        policy(Stage::Gamma, Fields::good().doc()),
        Some(bundle_of(&swapped, Some(&sums))),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_file_mismatch");
    let mut extra = honest.clone();
    extra.push((tar_name("kbf-daemon", "linux-x86_64"), b"daemon".to_vec()));
    let (f, s) = deploy_with(
        policy(Stage::Gamma, Fields::good().doc()),
        Some(bundle_of(&extra, Some(&sums))),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_file_mismatch");
}

/// (c) Proves an offer cannot be checked against a bundle with no `SHA256SUMS`, or against
/// no bundle: both refuse, since neither names the set.
///
/// Mutants planted: a missing `SHA256SUMS` skipped; a missing bundle skipped.
#[test]
fn an_offer_with_no_set_to_check_it_against_is_refused() {
    let (f, s) = deploy_with(
        policy(Stage::Gamma, Fields::good().doc()),
        Some(bundle_of(&files(), None)),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_set_unverified");
    let (f, s) = deploy_with(
        policy(Stage::Gamma, Fields::good().doc()),
        None,
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_set_unverified");
}

/// (d) Proves the offer's `meta_log` is compared with the staged binary's, in every
/// direction: different numbers, the binary naming none where the offer claims one, and the
/// binary naming one where the offer claims none. A probe that fails refuses too.
///
/// Mutants planted: the comparison skipped; compared only when both are numbers; a probe
/// failure read as no number.
#[test]
fn the_offers_meta_log_must_be_the_staged_binarys() {
    let good = || policy(Stage::Gamma, Fields::good().doc());
    for says in [Ok(Some(5)), Ok(None)] {
        let (f, s) = deploy_with(good(), Some(good_bundle()), says);
        refused(&f, &s, "offer_meta_log_mismatch");
    }
    let mut none = Fields::good();
    none.meta_log = None;
    let (f, s) = deploy_with(
        policy(Stage::Gamma, none.doc()),
        Some(good_bundle()),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_meta_log_mismatch");
    let (_, s) = deploy_with(
        policy(Stage::Gamma, none.doc()),
        Some(good_bundle()),
        Ok(None),
    );
    assert_eq!(s.exit, 0, "both name none: {}", s.line());
    let (f, s) = deploy_with(good(), Some(good_bundle()), Err("probe: boom".into()));
    refused(&f, &s, "offer_probe_failed");
}

/// (d) Proves a bundle with no `kbf` for the host's platform is refused: there is no
/// binary to read the number from.
///
/// Mutant planted: a missing host binary skipped.
#[test]
fn a_bundle_with_no_binary_for_this_host_is_refused() {
    let only_mac = vec![(tar_name("kbf", "darwin-arm64"), b"darwin".to_vec())];
    let mut fields = Fields::good();
    fields.set_hash = SetHash::of_sums(&sums_of(&only_mac)).as_str().to_owned();
    let bundle = bundle_of(&only_mac, Some(&sums_of(&only_mac)));
    let (f, s) = deploy_with(
        policy(Stage::Gamma, fields.doc()),
        Some(bundle),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_no_host_binary");
}

/// (e) Proves the offer's age is bounded on both sides, at the edges: exactly the limit is
/// accepted and a second more is stale; five minutes ahead is skew and a second more is the
/// future. Catches an age never checked, a date in the future accepted, and either boundary
/// off by one.
///
/// Mutants planted: the stale check removed; `>` made `>=`; the future check removed; the
/// skew 300 made 301 (the second ahead accepted).
#[test]
fn an_offer_too_old_or_from_the_future_is_refused() {
    let at = |created: u64| {
        let mut fields = Fields::good();
        fields.created = created;
        with(&fields)
    };
    let limit = 72 * HOUR;
    let (_, s) = at(NOW - limit);
    assert_eq!(s.exit, 0, "{}", s.line());
    let (f, s) = at(NOW - limit - 1);
    refused(&f, &s, "offer_stale");
    let (_, s) = at(NOW + 300);
    assert_eq!(s.exit, 0, "{}", s.line());
    let (f, s) = at(NOW + 301);
    refused(&f, &s, "offer_future");
}

/// (e) Proves the age limit is the policy's, not a constant: a 1 hour limit refuses a 2 hour
/// old offer the default would take.
///
/// Mutant planted: the limit taken from the default constant.
#[test]
fn the_age_limit_is_the_flags() {
    let mut fields = Fields::good();
    fields.created = NOW - 2 * HOUR;
    let one_hour = OfferPolicy::Offer {
        stage: Stage::Gamma,
        doc: Box::new(fields.doc()),
        max_age: Duration::from_secs(HOUR),
    };
    let (f, s) = deploy_with(one_hour, Some(good_bundle()), Ok(Some(4)));
    refused(&f, &s, "offer_stale");
}

/// Proves every platform the farm runs must be one the offer found complete.
///
/// Mutant planted: the platform check removed.
#[test]
fn a_farm_platform_the_offer_did_not_find_complete_is_refused() {
    let mut fields = Fields::good();
    fields.platforms = vec!["linux-x86_64"];
    let (f, s) = with(&fields);
    // The store takes the upload before the preflight reads the nodes; the farm is not asked.
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (3, "offer_platform"),
        "{}",
        s.line()
    );
    assert_eq!(f.requests("POST"), 0);
}

/// (g) Proves a gamma or prod deploy with no offer is refused, and `--no-offer` goes
/// through and is recorded as waived.
///
/// Mutants planted: `Missing` let through; `Waived` not recorded.
#[test]
fn no_offer_is_refused_unless_deliberately_waived() {
    for stage in [Stage::Gamma, Stage::Prod] {
        let (f, s) = deploy_with(
            OfferPolicy::Missing(stage),
            Some(good_bundle()),
            Ok(Some(4)),
        );
        refused(&f, &s, "offer_required");
        assert!(s.offer.is_none());
    }
    let (_, s) = deploy_with(
        OfferPolicy::Waived(Stage::Gamma),
        Some(good_bundle()),
        Ok(Some(4)),
    );
    assert_eq!(s.exit, 0, "{}", s.line());
    let rec = s.offer.expect("a waiver is recorded");
    assert_eq!(
        (rec.kind, rec.stage.as_str(), rec.commit),
        ("waived", "gamma", None)
    );
}

/// (a) Proves the offer file is read only through the validated parse and only as a small
/// regular file: an unknown field, a hand-edited `reversible`, a link and an oversize file
/// are all refused, and the good file is read.
///
/// Mutants planted: O_NOFOLLOW removed (the link followed); the size cap removed; the cap
/// off by one in either direction; the parse error shown whole.
#[test]
fn the_offer_file_is_read_only_through_the_validated_parse() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-read");
    let good = Fields::good().doc().to_bytes().unwrap();
    let write = |name: &str, bytes: &[u8]| {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    };
    assert!(offer::read(&write("good.json", &good)).is_ok());
    let text = String::from_utf8(good.clone()).unwrap();
    let extra = text.replacen('{', "{\"extra\": 1,", 1);
    assert!(offer::read(&write("extra.json", extra.as_bytes())).is_err());
    let edited = text.replace("\"reversible\": true", "\"reversible\": false");
    assert_ne!(edited, text);
    assert!(offer::read(&write("edited.json", edited.as_bytes())).is_err());
    let link = dir.join("link.json");
    std::os::unix::fs::symlink(dir.join("good.json"), &link).unwrap();
    assert_eq!(
        offer::read(&link).unwrap_err(),
        "--offer is a symbolic link"
    );
    // The size cap is inclusive: trailing blanks are valid JSON, so only the cap can refuse
    // the one-over file, and the one at the cap is read.
    let mut big = good.clone();
    big.resize(offer::MAX_OFFER as usize, b' ');
    assert!(offer::read(&write("at-cap.json", &big)).is_ok());
    big.push(b' ');
    let over = offer::read(&write("big.json", &big)).unwrap_err();
    assert!(over.contains("larger than"), "{over}");
    // Not a regular file: a directory, and a FIFO (which must not block the open).
    let not_file = offer::read(&dir).unwrap_err();
    assert_eq!(not_file, "--offer is not a regular file");
    let fifo = dir.join("fifo.json");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert_eq!(
        offer::read(&fifo).unwrap_err(),
        "--offer is not a regular file"
    );
    // A refusal never repeats what the file held, however the parse failed.
    let secret = "SECRET-MARKER-9";
    let typed = text.replace(
        "\"created\": ",
        &format!("\"created\": \"{secret}\", \"x\": "),
    );
    let unknown = text.replacen('{', &format!("{{\"{secret}\": 1,"), 1);
    let bad_hash = text.replace(Fields::good().set_hash.as_str(), secret);
    for (name, body) in [("typed", typed), ("unknown", unknown), ("hash", bad_hash)] {
        assert_ne!(body, text, "{name}");
        let e = offer::read(&write(&format!("{name}.json"), body.as_bytes())).unwrap_err();
        assert!(!e.contains(secret), "{name}: {e}");
    }
    let e = offer::read(&write("edited2.json", edited.as_bytes())).unwrap_err();
    assert!(e.contains("reversible"), "{e}");
    assert!(offer::read(&dir.join("absent.json")).is_err());
}

/// A runner that answers every program with `stdout` (or the exit code `code`), and keeps
/// what it was asked to run.
struct Plays {
    stdout: String,
    code: i32,
    timed_out: bool,
    asked: std::sync::Mutex<Vec<crate::run::Spawn>>,
}

impl crate::run::Runner for std::sync::Arc<Plays> {
    fn run(&self, spawn: &crate::run::Spawn) -> io::Result<crate::run::Ran> {
        self.asked.lock().unwrap().push(spawn.clone());
        Ok(crate::run::Ran {
            code: Some(self.code),
            stdout: self.stdout.clone().into_bytes(),
            stderr: Vec::new(),
            timed_out: self.timed_out,
        })
    }
}

/// (d) Proves the real probe unpacks the host's `kbf` tarball, starts `kbf --version --json`
/// as the unprivileged user in a scratch directory with an empty environment, and returns
/// what it prints: a number, none, and a refusal for a binary that names another commit,
/// fails, or is not a tarball with a `kbf` in it. The scratch directory is gone after.
/// Catches a probe that returns a constant, skips the commit check, runs as root, or leaves
/// bytes behind.
///
/// Mutants planted: the probe returning `Ok(None)`; the commit check removed; `run_as` not
/// passed on; the exit code not checked.
#[test]
fn the_host_probe_unpacks_the_binary_and_reads_its_meta_log() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-probe");
    let short = &TARGET[..12];
    let version = |meta: &str, commit: &str| {
        format!(
            "{{\"commit\":\"{commit}\",{meta}\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
        )
    };
    let tarball = || {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut b = tar::Builder::new(&mut gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o755);
        h.set_cksum();
        b.append_data(&mut h, "kbf", &b"bytes"[..]).unwrap();
        b.finish().unwrap();
        drop(b);
        gz.finish().unwrap()
    };
    let commit = c(TARGET);
    let run = |stdout: String, code: i32| {
        let plays = std::sync::Arc::new(Plays {
            stdout,
            code,
            timed_out: false,
            asked: Default::default(),
        });
        // Not root here, so the probe cannot chown its scratch directory to the user.
        let probe = HostProbe::new(
            dir.join("work"),
            None,
            Box::new(plays.clone()),
            Box::new(Vouches::new(Ok(()))),
        );
        let answer = probe.meta_log(&commit, &tarball(), Some(b"att"));
        (answer, plays)
    };
    let (answer, plays) = run(version("\"meta_log\":4,", short), 0);
    assert_eq!(answer, Ok(Some(4)));
    let asked = plays.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].args, ["--version", "--json"]);
    assert!(asked[0].env.is_empty());
    assert!(asked[0].program.ends_with("kbf"));
    assert!(asked[0].program.starts_with(dir.join("work")));
    drop(asked);
    assert_eq!(run(version("", short), 0).0, Ok(None));
    assert!(
        run(version("\"meta_log\":4,", "bbbbbbbbbbbb"), 0)
            .0
            .is_err()
    );
    assert!(run(String::new(), 3).0.is_err());
    // A non-zero exit refuses even when stdout is a good answer.
    let failed = run(version("\"meta_log\":4,", short), 3).0.unwrap_err();
    assert!(text(failed).contains("ended"));
    assert!(run("not json".into(), 0).0.is_err());
    let not_a_tarball = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(std::sync::Arc::new(Plays {
            stdout: version("", short),
            code: 0,
            timed_out: false,
            asked: Default::default(),
        })),
        Box::new(Vouches::new(Ok(()))),
    );
    assert!(
        not_a_tarball
            .meta_log(&commit, b"not a tarball", Some(b"att"))
            .is_err()
    );
    // The scratch directory is gone.
    let left: Vec<_> = std::fs::read_dir(dir.join("work")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    // The user the settings name is what the binary runs as (here the test's own user, which
    // the scratch directory can be handed to without being root).
    // SAFETY: geteuid and getegid have no preconditions and cannot fail.
    let me = crate::run::RunAs {
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
    };
    let plays = std::sync::Arc::new(Plays {
        stdout: version("", short),
        code: 0,
        timed_out: false,
        asked: Default::default(),
    });
    let as_me = HostProbe::new(
        dir.join("work"),
        Some(me),
        Box::new(plays.clone()),
        Box::new(Vouches::new(Ok(()))),
    );
    assert_eq!(as_me.meta_log(&commit, &tarball(), Some(b"att")), Ok(None));
    assert_eq!(plays.asked.lock().unwrap()[0].run_as, Some(me));
}

/// (d) Proves a probe that cannot run says so rather than answering: a binary that times out,
/// a scratch directory that cannot be made, and a build with no probe all refuse. Catches a
/// timeout read as a number and a failure read as "no meta_log".
///
/// Mutants planted: the timeout let through; `NoProbe` answering `Ok(None)`.
#[test]
fn a_probe_that_cannot_run_refuses() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-probe-fail");
    let commit = c(TARGET);
    let short = &TARGET[..12];
    let said = format!(
        "{{\"commit\":\"{short}\",\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(&mut gz);
    let mut h = tar::Header::new_gnu();
    h.set_size(1);
    h.set_mode(0o755);
    h.set_cksum();
    b.append_data(&mut h, "kbf", &b"x"[..]).unwrap();
    b.finish().unwrap();
    drop(b);
    let tarball = gz.finish().unwrap();
    let slow = std::sync::Arc::new(Plays {
        stdout: said.clone(),
        code: 0,
        timed_out: true,
        asked: Default::default(),
    });
    let probe = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(slow),
        Box::new(Vouches::new(Ok(()))),
    );
    assert!(text(probe.meta_log(&commit, &tarball, Some(b"att")).unwrap_err()).contains("ended"));
    // A file where the work directory's parent should be.
    std::fs::write(dir.join("blocker"), b"").unwrap();
    let fine = std::sync::Arc::new(Plays {
        stdout: said,
        code: 0,
        timed_out: false,
        asked: Default::default(),
    });
    let blocked = HostProbe::new(
        dir.join("blocker").join("work"),
        None,
        Box::new(fine),
        Box::new(Vouches::new(Ok(()))),
    );
    assert!(
        text(
            blocked
                .meta_log(&commit, &tarball, Some(b"att"))
                .unwrap_err()
        )
        .starts_with("probe:")
    );
    assert!(NoProbe.meta_log(&commit, &tarball, Some(b"att")).is_err());
    assert!(HostProbe::at(&dir, false).is_ok());
    assert!(HostProbe::at(&dir, true).is_ok());
    assert!(HostProbe::at(std::path::Path::new("relative/root"), false).is_err());
}

/// Proves `SHA256SUMS` is read as `<sha256>  <name>` lines and nothing looser: a binary mark,
/// a path before the name and blank lines are accepted; a short or upper-case digest, a
/// missing name, no separator and a name listed twice with two digests are refused (a reader
/// that took the first would not see the second).
///
/// Mutants planted: the digest length not checked; the hex alphabet not checked; an empty
/// name accepted; a conflicting duplicate accepted.
#[test]
fn the_checksum_file_is_read_strictly() {
    let (a, b) = ("a".repeat(64), "b".repeat(64));
    let read = |text: &str| offer::parse_sums(text.as_bytes());
    let ok = read(&format!(
        "{a}  kbf-1.tar.gz\n\n{b} *dist/kbf-2.tar.gz\n{a}  kbf-1.tar.gz\n"
    ))
    .unwrap();
    assert_eq!(ok.len(), 2);
    assert_eq!(ok["kbf-2.tar.gz"], b);
    for bad in [
        format!("{}  x\n", "a".repeat(63)),
        format!("{}  x\n", "A".repeat(64)),
        format!("{}  x\n", "g".repeat(64)),
        format!("{a}  \n"),
        format!("{a}  dir/\n"),
        format!("{a} x\n"),
        format!("{a}\n"),
        format!("{a}  x\n{b}  x\n"),
    ] {
        assert!(read(&bad).is_err(), "{bad:?}");
    }
    assert!(offer::parse_sums(&[0xff, 0xfe]).is_err());
}

/// Proves the bundle reader takes one non-empty `SHA256SUMS` of at most `MAX_SUMS` bytes and
/// refuses two, an empty one, a larger one and one cut short.
///
/// Mutants planted: a second `SHA256SUMS` accepted; the empty file accepted; the cap removed;
/// a read error ignored.
#[test]
fn the_bundle_reads_one_bounded_checksum_file() {
    let commit = c(TARGET);
    let tar_name = tar_name("kbf", "linux-x86_64");
    let with = |sums: &[&[u8]]| {
        let mut files: Vec<(&str, &[u8])> = vec![(tar_name.as_str(), b"t")];
        files.extend(sums.iter().map(|s| ("SHA256SUMS", *s)));
        tarball(&files)
    };
    let read = |bytes: &[u8]| bundle::read_bundle(bytes, &commit);
    assert_eq!(
        read(&with(&[b"sums"])).unwrap().sums.as_deref(),
        Some(&b"sums"[..])
    );
    assert_eq!(read(&with(&[])).unwrap().sums, None);
    assert!(read(&with(&[b"one", b"two"])).is_err());
    assert!(read(&with(&[b""])).is_err());
    let big = vec![b'x'; bundle::MAX_SUMS as usize + 1];
    assert!(read(&with(&[&big])).is_err());
    let whole = tarball(&[("SHA256SUMS", &[b'x'; 2000][..])]);
    assert!(read(&whole[..512 + 100]).is_err());
}

/// (c) Proves a bundle whose `SHA256SUMS` is the offer's set (the hash matches) but is not a
/// checksum file is refused with the set's reason, not waved through: the hash names the
/// bytes, and the bytes must still say which file is which.
///
/// Mutant planted: the parse error of the sums ignored (read as an empty list, so the file
/// check refuses with another reason).
#[test]
fn sums_that_hash_right_but_do_not_parse_are_refused() {
    let junk = b"this is not a checksum file\n".to_vec();
    let mut fields = Fields::good();
    fields.set_hash = SetHash::of_sums(&junk).as_str().to_owned();
    let bundle = bundle_of(&files(), Some(&junk));
    let (f, s) = deploy_with(
        policy(Stage::Gamma, fields.doc()),
        Some(bundle),
        Ok(Some(4)),
    );
    refused(&f, &s, "offer_set_mismatch");
    assert!(s.detail.contains("SHA256SUMS line 1"), "{}", s.line());
}

/// A gzip tarball with a `kbf` in it.
fn gz_kbf() -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(&mut gz);
    let mut h = tar::Header::new_gnu();
    h.set_size(5);
    h.set_mode(0o755);
    h.set_cksum();
    b.append_data(&mut h, "kbf", &b"bytes"[..]).unwrap();
    b.finish().unwrap();
    drop(b);
    gz.finish().unwrap()
}

/// (d) Proves the host's `kbf` is attested before it is unpacked or run: when the
/// attestation check refuses, or the bundle has no attestation, the probe's runner is never
/// asked to start anything and no scratch directory is left; when it vouches, it is shown
/// the exact tarball and attestation bytes, and then the binary runs once. Catches a probe
/// that runs first and checks after, and an attestation failure that is ignored.
///
/// Mutants planted: the run moved before `attest.verify`; the `verify` error dropped.
#[test]
fn the_host_binary_is_attested_before_it_is_run() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-attest-first");
    let short = &TARGET[..12];
    let said = format!(
        "{{\"commit\":\"{short}\",\"meta_log\":4,\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
    );
    let plays = || {
        std::sync::Arc::new(Plays {
            stdout: said.clone(),
            code: 0,
            timed_out: false,
            asked: Default::default(),
        })
    };
    let commit = c(TARGET);
    let tar = gz_kbf();

    let (run, no) = (plays(), Vouches::new(Err("not attested".into())));
    let probe = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(run.clone()),
        Box::new(no.clone()),
    );
    let fault = probe.meta_log(&commit, &tar, Some(b"att")).unwrap_err();
    assert!(
        matches!(&fault, ProbeFault::Unattested(d) if d.contains("not attested")),
        "{fault:?}"
    );
    assert_eq!(no.seen.lock().unwrap().len(), 1);
    assert!(run.asked.lock().unwrap().is_empty());
    assert!(
        std::fs::read_dir(dir.join("work")).is_ok_and(|mut d| d.next().is_none())
            || !dir.join("work").exists()
    );

    let (run, yes) = (plays(), Vouches::new(Ok(())));
    let probe = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(run.clone()),
        Box::new(yes.clone()),
    );
    let fault = probe.meta_log(&commit, &tar, None).unwrap_err();
    assert!(matches!(fault, ProbeFault::Unattested(_)));
    assert!(yes.seen.lock().unwrap().is_empty());
    assert!(run.asked.lock().unwrap().is_empty());

    assert_eq!(probe.meta_log(&commit, &tar, Some(b"att")), Ok(Some(4)));
    assert_eq!(
        *yes.seen.lock().unwrap(),
        vec![(TARGET.to_owned(), tar.clone(), b"att".to_vec())]
    );
    assert_eq!(run.asked.lock().unwrap().len(), 1);
}

/// (d) Proves the deploy refuses an offer whose host tarball is not attested with
/// `offer_set_unverified`, before any upload and without the binary being run, and does not
/// when it is attested. Catches `check_bundle` passing no attestation to the probe, and a
/// refusal of the probe mapped to a different reason.
///
/// Mutants planted: the bundle's attestation not passed on; `Unattested` mapped to
/// `offer_probe_failed`.
#[test]
fn a_deploy_with_an_unattested_host_binary_is_refused_without_running_it() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-attest-deploy");
    let short = &TARGET[..12];
    let said = format!(
        "{{\"commit\":\"{short}\",\"meta_log\":4,\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
    );
    let mut files = files();
    files[0].1 = gz_kbf();
    let sums = sums_of(&files);
    let mut fields = Fields::good();
    fields.set_hash = SetHash::of_sums(&sums).as_str().to_owned();
    // Irreversible, so the probe of this test is asked only for the staged `kbf` (it counts
    // its runs); the host's running build is the subject of `in_bundle`.
    fields.previous_meta_log = Some(3);
    let attested = |with_att: bool| {
        let mut all: Vec<(String, Vec<u8>)> = files.clone();
        if with_att {
            all.push((format!("{}.attestation", files[0].0), b"att".to_vec()));
        }
        let mut refs: Vec<(&str, &[u8])> = all
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        refs.push(("SHA256SUMS", &sums));
        tarball(&refs)
    };
    let try_it = |bundle: Vec<u8>, verdict: Result<(), String>| {
        let run = std::sync::Arc::new(Plays {
            stdout: said.clone(),
            code: 0,
            timed_out: false,
            asked: Default::default(),
        });
        let vouch = Vouches::new(verdict);
        let probe = HostProbe::new(
            dir.join("work"),
            None,
            Box::new(run.clone()),
            Box::new(vouch),
        );
        let (f, s) = deploy_probing(policy(Stage::Gamma, fields.doc()), Some(bundle), &probe);
        let ran = run.asked.lock().unwrap().len();
        (f, s, ran)
    };
    let (f, s, ran) = try_it(attested(true), Err("bad signature".into()));
    refused(&f, &s, "offer_set_unverified");
    assert_eq!(ran, 0);
    let (f, s, ran) = try_it(attested(false), Ok(()));
    refused(&f, &s, "offer_set_unverified");
    assert_eq!(ran, 0);
    let (_, s, ran) = try_it(attested(true), Ok(()));
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(ran, 1);
}

/// (d) Proves the real attestation check hands `gh` the tarball and the bundle's attestation
/// as files the user can read, in a home of its own, through the stage's `GhCheck::verify`:
/// an accepting `gh` is `Ok` and the scratch directory is gone after; a refusing `gh`, an
/// untrusted `gh` (which is never started), a work directory that cannot be made and `gh`
/// failing to start are all errors. Catches a check that verifies other bytes than it was
/// given, drops the trust check, or reads a refusal as acceptance.
///
/// Mutants planted: the attestation file written with the tarball's bytes; the `verify`
/// result mapped to `Ok`.
#[test]
fn the_gh_attestation_check_verifies_the_bytes_it_is_given() {
    use crate::attest::GhCheck;
    use crate::fixtures::{me, ran, trusted_host};
    use crate::layout::{Layout, Platform};
    use crate::run::Spawn;
    use std::sync::{Arc, Mutex};

    let (_, dir) = crate::testkit::host_root("deploy-offer-gh-attest");
    let host = Layout::new(kbf_host::Root::new("/").unwrap(), Platform::Linux);
    let mut settings = crate::settings::Settings::defaults(&host);
    settings.run_as = me();
    let commit = c(TARGET);
    type Seen = (Spawn, Vec<u8>, Vec<u8>, bool);
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let check =
        |code: i32, trust: kbf_host::testkit::FakeExec, work: std::path::PathBuf, switch| {
            let log = seen.clone();
            type Answer = Box<dyn Fn(&Spawn) -> std::io::Result<crate::run::Ran>>;
            struct Gh(Answer);
            impl crate::run::Runner for Gh {
                fn run(&self, s: &Spawn) -> std::io::Result<crate::run::Ran> {
                    (self.0)(s)
                }
            }
            let runner = Gh(Box::new(move |s: &Spawn| {
                let arg = |i: usize| std::path::PathBuf::from(&s.args[i]);
                let (tar, bundle) = (arg(2), arg(4));
                let readable = std::fs::metadata(&tar).is_ok_and(|m| {
                    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o044 == 0o044
                });
                log.lock().unwrap().push((
                    s.clone(),
                    std::fs::read(&tar).unwrap(),
                    std::fs::read(&bundle).unwrap(),
                    readable,
                ));
                if code < 0 {
                    Err(std::io::Error::other("no gh"))
                } else {
                    Ok(ran(code, "", "no"))
                }
            }));
            GhAttest::new(
                work,
                GhCheck::pinned(&host, &settings),
                Box::new(trust),
                Box::new(runner),
                switch,
            )
        };
    let work = dir.join("work");
    let ok = check(0, trusted_host(), work.clone(), true);
    assert_eq!(ok.verify(&commit, b"the tarball", b"the bundle"), Ok(()));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, b"the tarball");
        assert_eq!(seen[0].2, b"the bundle");
        assert!(seen[0].3);
        assert_eq!(seen[0].0.run_as, Some(me()));
    }
    assert!(std::fs::read_dir(&work).unwrap().next().is_none());
    let no = check(1, trusted_host(), work.clone(), false);
    assert!(
        no.verify(&commit, b"t", b"b")
            .unwrap_err()
            .contains("NotAttested")
    );
    let broken = check(-1, trusted_host(), work.clone(), false);
    assert!(broken.verify(&commit, b"t", b"b").is_err());
    let before = seen.lock().unwrap().len();
    let untrusted = check(0, kbf_host::testkit::FakeExec::new(0), work.clone(), false);
    assert!(untrusted.verify(&commit, b"t", b"b").is_err());
    assert_eq!(seen.lock().unwrap().len(), before);
    std::fs::write(dir.join("blocker"), b"").unwrap();
    let stuck = check(0, trusted_host(), dir.join("blocker").join("work"), false);
    assert!(stuck.verify(&commit, b"t", b"b").is_err());
}

/// (d) Proves a plan (no `--apply`) runs the same bundle checks an apply does, so a dry run
/// cannot say OK for a bundle the apply would refuse: a bundle of another set, a tarball the
/// sums do not name, an unattested host binary and a `meta_log` other than the offer's are
/// each refused in plan mode, with nothing uploaded; the good bundle plans (exit 0, no
/// upload, no update). The probe and the attestation run in a plan too: they are read-only (a scratch
/// directory, the pinned `gh`, the binary as the unprivileged user) and need no `--apply`.
/// Catches `check_bundle` skipped unless applying.
///
/// Mutants planted: `check_bundle` only when `apply`; the plan returning before the probe.
#[test]
fn a_plan_runs_the_bundle_checks_an_apply_would() {
    let plan = |fields: &Fields, bundle: Vec<u8>, says: Result<Option<u64>, String>| {
        deploy_mode(
            false,
            policy(Stage::Gamma, fields.doc()),
            Some(bundle),
            &Says(says),
        )
    };
    let mut other_set = Fields::good();
    other_set.set_hash = SetHash::of_sums(b"another set").as_str().to_owned();
    let (f, s) = plan(&other_set, good_bundle(), Ok(Some(4)));
    refused(&f, &s, "offer_set_mismatch");

    let mut swapped = files();
    swapped[0].1 = b"not the file the sums name".to_vec();
    let bundle = bundle_of(&swapped, Some(&sums_of(&files())));
    let (f, s) = plan(&Fields::good(), bundle, Ok(Some(4)));
    refused(&f, &s, "offer_file_mismatch");

    let (f, s) = plan(&Fields::good(), good_bundle(), Ok(Some(9)));
    refused(&f, &s, "offer_meta_log_mismatch");

    let (f, s) = plan(&Fields::good(), good_bundle(), Err("no".into()));
    refused(&f, &s, "offer_probe_failed");

    let (f, s) = plan(&Fields::good(), good_bundle(), Ok(Some(4)));
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(f.requests("PUT"), 0, "{}", s.line());
    assert_eq!(f.requests("POST"), 0, "{}", s.line());
}

/// (d) Proves that with two platforms' attestations in the bundle, the one verified is the
/// host's `kbf` against the host's own attestation, for the deployed commit, and only that
/// one: the other platform's tarball and attestation are never shown to the check. Catches an
/// attestation looked up for another platform, a tarball paired with the wrong attestation,
/// and a check given another commit.
///
/// Mutants planted: the attestation taken from the first `kbf.attestation` of any platform;
/// the platform filter on the tarball removed; the commit passed to `verify` replaced.
#[test]
fn only_the_hosts_kbf_is_attested_against_its_own_attestation() {
    let (_, dir) = crate::testkit::host_root("deploy-offer-attest-platform");
    let short = &TARGET[..12];
    let said = format!(
        "{{\"commit\":\"{short}\",\"meta_log\":4,\"min_protocol\":3,\"profile\":\"release\",\"protocol\":4,\"state_schema\":1,\"version\":\"0.1.0\"}}"
    );
    // The darwin tarball sorts first, so a lookup that ignores the platform finds it first.
    let mut files = files();
    files[0].1 = gz_kbf();
    files.swap(0, 1);
    let host_tar = files[1].1.clone();
    assert_eq!(files[1].0, tar_name("kbf", "linux-x86_64"));
    let sums = sums_of(&files);
    let mut fields = Fields::good();
    fields.set_hash = SetHash::of_sums(&sums).as_str().to_owned();
    // Irreversible: this probe names no running build, and only the staged `kbf` is asked.
    fields.previous_meta_log = Some(3);
    let mut all = files.clone();
    all.push((
        format!("{}.attestation", files[0].0),
        b"att-darwin".to_vec(),
    ));
    all.push((format!("{}.attestation", files[1].0), b"att-linux".to_vec()));
    let mut refs: Vec<(&str, &[u8])> = all
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    refs.push(("SHA256SUMS", &sums));
    let vouch = Vouches::new(Ok(()));
    let run = std::sync::Arc::new(Plays {
        stdout: said,
        code: 0,
        timed_out: false,
        asked: Default::default(),
    });
    let probe = HostProbe::new(
        dir.join("work"),
        None,
        Box::new(run),
        Box::new(vouch.clone()),
    );
    let (_, s) = deploy_probing(
        policy(Stage::Gamma, fields.doc()),
        Some(tarball(&refs)),
        &probe,
    );
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(
        *vouch.seen.lock().unwrap(),
        vec![(TARGET.to_owned(), host_tar, b"att-linux".to_vec())]
    );
}
