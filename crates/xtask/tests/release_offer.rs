//! `cargo xtask release offer`: every check refuses and changes nothing, and the record
//! it leaves reads back. The library tests give the checks a recorded attester (the
//! seam [`Attester`]); the CLI tests run the binary against the fake `gh` of this crate.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::{Engine, engine::general_purpose::STANDARD};
use kbf_gate::{
    Beta, CommitId, GateResult, Header, Limit, OfferDoc, ResultDoc, Sample, SetHash, Stage, Status,
    Target,
};
use sha2::Digest;
use xtask::Error;
use xtask::package::Platform;
use xtask::release::offer::{Attester, Evidence, Request, plan, write};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const C: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const PREV: &str = "badbad0123456789badbad0123456789badbad01";
const REPO: &str = "komira-ai/kbf";
const ARTIFACTS: &str = "komira-ai/kbf/.github/workflows/artifacts.yml";
const BETA: &str = "komira-ai/kbf/.github/workflows/beta.yml";
const PLATFORMS: [Platform; 2] = [Platform::LinuxX86_64, Platform::DarwinArm64];

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-offer")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

fn bundle(sha: &str, fake_flags: Option<&str>) -> String {
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": "asset", "digest": { "sha256": sha } }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {},
    });
    let mut b = serde_json::json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": {
            "payloadType": "application/vnd.in-toto+json",
            "payload": STANDARD.encode(statement.to_string()),
            "signatures": [],
        },
    });
    if let Some(f) = fake_flags {
        b["fakeAttestations"] = serde_json::json!([format!("{sha} {f}")]);
    }
    format!("{b}\n")
}

/// The assets of commit [`C`] for `platforms` in `<root>/dist`: tarball, SBOM, bundle.
/// Returns the dist directory and the (sha, name) lines of its `SHA256SUMS`.
fn make_dist(root: &Path, platforms: &[&str]) -> (PathBuf, Vec<(String, String)>) {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let mut lines = Vec::new();
    for p in platforms {
        for ext in ["cdx.json", "tar.gz"] {
            let name = format!("kbf-{C}-{p}.{ext}");
            let bytes = format!("{name} bytes");
            std::fs::write(dist.join(&name), &bytes).unwrap();
            let sha = sha_of(bytes.as_bytes());
            if ext == "tar.gz" {
                std::fs::write(
                    dist.join(format!("{name}.sigstore.json")),
                    bundle(&sha, Some(&bundle_flags())),
                )
                .unwrap();
            }
            lines.push((sha, name));
        }
    }
    lines.sort_by(|a, b| a.1.cmp(&b.1));
    let text: String = lines.iter().map(|(s, n)| format!("{s}  {n}\n")).collect();
    std::fs::write(dist.join("SHA256SUMS"), text).unwrap();
    (dist, lines)
}

fn bundle_flags() -> String {
    format!(
        "--repo {REPO} --signer-workflow {ARTIFACTS} --cert-oidc-issuer \
         https://token.actions.githubusercontent.com --source-ref refs/heads/main \
         --source-digest {C} --predicate-type https://slsa.dev/provenance/v1 \
         --deny-self-hosted-runners"
    )
}

fn set_of(dist: &Path) -> SetHash {
    SetHash::of_sums(&std::fs::read(dist.join("SHA256SUMS")).unwrap())
}

/// A beta result for commit `commit` and `set`, green, one gate in its limit.
fn beta_doc(commit: &str, set: &SetHash) -> ResultDoc {
    let mut d = ResultDoc::begin(
        Header {
            stage: Stage::Beta,
            commit: CommitId::parse(commit).unwrap(),
            set_hash: set.clone(),
            run_id: "beta-77".to_owned(),
            attempt: 1,
        },
        100,
    );
    d.gates.push(GateResult::decide(
        "lost-blobs",
        Sample::Value(0.0),
        Limit::AtMost(0.0),
        100,
        200,
    ));
    d.status = Status::Green;
    d.finished = Some(200);
    d
}

fn put_beta(dir: &Path, doc: &ResultDoc) -> PathBuf {
    let path = dir.join("beta-result.json");
    std::fs::write(&path, doc.to_bytes().unwrap()).unwrap();
    path
}

/// A beta document written as raw JSON, to hold what `ResultDoc` would not write green.
fn put_raw(dir: &Path, edit: impl FnOnce(&mut serde_json::Value), doc: &ResultDoc) -> PathBuf {
    let mut v: serde_json::Value = serde_json::from_slice(&doc.to_bytes().unwrap()).unwrap();
    edit(&mut v);
    let path = dir.join("beta-result.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    path
}

fn identity(dir: &Path, name: &str, commit: &str, meta_log: Option<&str>) -> PathBuf {
    let ml = meta_log.map_or(String::new(), |m| format!("\"meta_log\":{m},"));
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "{{\"commit\":\"{}\",{ml}\"profile\":\"release\",\"protocol\":1}}\n",
            &commit[..12]
        ),
    )
    .unwrap();
    path
}

/// The recorded attestations: those of one commit for the artifacts, and the beta result
/// attested for one commit, if any. Records every question it is asked.
struct Recorded {
    artifacts_for: String,
    beta_for: Option<String>,
    asked: RefCell<Vec<String>>,
}

impl Recorded {
    fn all(commit: &str) -> Self {
        Self {
            artifacts_for: commit.to_owned(),
            beta_for: Some(commit.to_owned()),
            asked: RefCell::default(),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

impl Attester for Recorded {
    fn artifacts(&self, _dir: &Path, commit: &str) -> Result<(), Error> {
        self.asked.borrow_mut().push(format!("artifacts {commit}"));
        if commit == self.artifacts_for {
            Ok(())
        } else {
            Err(Error::Failed("no attestation for this commit".to_owned()))
        }
    }

    fn beta(&self, _result: &Path, commit: &str) -> Result<(), Error> {
        self.asked.borrow_mut().push(format!("beta {commit}"));
        if self.beta_for.as_deref() == Some(commit) {
            Ok(())
        } else {
            Err(Error::Failed("beta result is not attested".to_owned()))
        }
    }
}

/// A request for gamma over `dist` with `beta`, no lineage; the tests edit it.
fn request<'a>(dist: &'a Path, beta: Evidence<'a>) -> Request<'a> {
    Request {
        dir: dist,
        commit: C,
        target: Target::Gamma,
        platforms: &PLATFORMS,
        beta,
        version_json: None,
        previous: None,
        previous_version_json: None,
        now: 1_700_000_000,
    }
}

fn refused(r: Result<OfferDoc, Error>) -> String {
    match r {
        Err(Error::Refused(lines)) => lines.join("\n"),
        Err(e) => e.to_string(),
        Ok(doc) => panic!("expected a refusal, got an offer: {doc:?}"),
    }
}

struct Fx {
    root: PathBuf,
    dist: PathBuf,
    set: SetHash,
}

fn fx(name: &str) -> Fx {
    let root = scratch(name);
    let (dist, _) = make_dist(&root, &["linux-x86_64", "darwin-arm64"]);
    let set = set_of(&dist);
    Fx { root, dist, set }
}

#[test]
fn a_release_with_an_attested_green_beta_is_offered_and_the_record_reads_back() {
    // Proves the accepting path: both attestation questions are asked, for the offered
    // commit; the document names commit, set, beta run, platforms; written then read it
    // is equal. Mutants: skip either attester call (asked list); record another run id.
    let f = fx("accept");
    let beta = put_beta(&f.root, &beta_doc(C, &f.set));
    let a = Recorded::all(C);
    let doc = plan(&request(&f.dist, Evidence::Result(&beta)), &a).unwrap();
    assert_eq!(a.asked(), [format!("artifacts {C}"), format!("beta {C}")]);
    assert_eq!(doc.commit.as_str(), C);
    assert_eq!(doc.set_hash, f.set);
    assert_eq!(doc.platforms, ["darwin-arm64", "linux-x86_64"]);
    assert_eq!(
        doc.beta,
        Beta::Attested {
            run_id: "beta-77".to_owned()
        }
    );
    assert_eq!(
        (doc.meta_log, doc.previous.is_some(), doc.reversible),
        (None, false, false)
    );
    let out = f.root.join("offer.json");
    write(&out, &doc).unwrap();
    assert_eq!(
        OfferDoc::from_bytes(&std::fs::read(&out).unwrap()).unwrap(),
        doc
    );
}

#[test]
fn an_attestation_for_another_commit_refuses_the_artifacts_and_the_beta_result() {
    // Mutants: drop the artifacts check; drop the beta attestation check.
    let f = fx("wrong-commit");
    let beta = put_beta(&f.root, &beta_doc(C, &f.set));
    let mut a = Recorded::all(C);
    a.artifacts_for = PREV.to_owned();
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("no attestation for this commit"), "{why}");
    let mut a = Recorded::all(C);
    a.beta_for = Some(PREV.to_owned());
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("beta result is not attested"), "{why}");
    a.beta_for = None;
    assert!(refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("not attested"));
}

#[test]
fn a_beta_result_for_another_commit_or_set_or_stage_is_refused() {
    // Mutants: drop the commit comparison; drop the set-hash comparison; drop the stage check.
    let f = fx("beta-identity");
    let a = Recorded::all(C);
    let other_set = SetHash::of_sums(b"another set\n");
    let beta = put_beta(&f.root, &beta_doc(C, &other_set));
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("names set"), "{why}");
    let beta = put_beta(&f.root, &beta_doc(PREV, &f.set));
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("names commit"), "{why}");
    let mut gamma = beta_doc(C, &f.set);
    gamma.stage = Stage::Gamma;
    let beta = put_beta(&f.root, &gamma);
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("stage"), "{why}");
    assert!(
        a.asked().is_empty(),
        "no attestation is asked about a bad document"
    );
}

#[test]
fn a_beta_result_that_is_red_unreadable_or_has_a_gate_out_of_its_limit_is_refused() {
    // Mutants: accept any status; recheck removed (a green gate with a number past its
    // limit passes); `!= Green` weakened to `== Red` (a warned gate passes); an absent
    // measurement read as zero; an empty gate list accepted.
    let f = fx("beta-gates");
    let a = Recorded::all(C);
    let ok = beta_doc(C, &f.set);
    let mut red = ok.clone();
    red.status = Status::Red;
    let beta = put_beta(&f.root, &red);
    assert!(refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("not green"));
    // The recorded verdict says green, the number says otherwise.
    let beta = put_raw(&f.root, |v| v["gates"][0]["measured"] = 3.0.into(), &ok);
    let why = refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a));
    assert!(why.contains("beta gate `lost-blobs` is not green"), "{why}");
    // A gate recorded as a warning.
    let beta = put_raw(&f.root, |v| v["gates"][0]["verdict"] = "warn".into(), &ok);
    assert!(refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("not green"));
    // A gate that measured nothing.
    let beta = put_raw(
        &f.root,
        |v| v["gates"][0]["measured"] = serde_json::Value::Null,
        &ok,
    );
    assert!(refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("not green"));
    // A green document with a red gate is refused by the reader itself.
    let beta = put_raw(&f.root, |v| v["gates"][0]["verdict"] = "red".into(), &ok);
    assert!(
        refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("no usable result")
    );
    // Unreadable, and missing.
    let junk = f.root.join("junk.json");
    std::fs::write(&junk, "not json").unwrap();
    assert!(
        refused(plan(&request(&f.dist, Evidence::Result(&junk)), &a)).contains("no usable result")
    );
    let gone = f.root.join("gone.json");
    assert!(
        refused(plan(&request(&f.dist, Evidence::Result(&gone)), &a)).contains("--beta-result")
    );
    // No gate at all.
    let mut empty = ok.clone();
    empty.gates.clear();
    empty.status = Status::Red;
    let beta = put_beta(&f.root, &empty);
    assert!(
        refused(plan(&request(&f.dist, Evidence::Result(&beta)), &a)).contains("decided no gate")
    );
    assert!(a.asked().is_empty());
}

#[test]
fn no_beta_and_no_waiver_is_refused() {
    // Mutant: Evidence::Missing treated as a pass.
    let f = fx("beta-missing");
    let a = Recorded::all(C);
    let why = refused(plan(&request(&f.dist, Evidence::Missing), &a));
    assert!(why.contains("no beta result"), "{why}");
    assert!(why.contains("--waive-beta"), "{why}");
    assert!(
        a.asked().is_empty(),
        "attestations are asked only of an acceptable release"
    );
}

#[test]
fn a_waiver_needs_its_reason_and_its_operator() {
    // Mutants: Waiver::new bypassed; operator defaulted.
    let f = fx("waiver-fields");
    let a = Recorded::all(C);
    for (reason, operator) in [
        ("", "op"),
        ("   ", "op"),
        ("no beta yet", ""),
        ("no beta yet", " "),
    ] {
        let why = refused(plan(
            &request(&f.dist, Evidence::Waived { reason, operator }),
            &a,
        ));
        assert!(
            why.contains("names its operator"),
            "{reason:?} {operator:?}: {why}"
        );
    }
}

#[test]
fn a_waiver_is_refused_for_prod_and_recorded_for_gamma() {
    // Mutants: drop the prod refusal; record nothing/other than the reason; ask the beta
    // attester anyway.
    let f = fx("waiver-target");
    let a = Recorded::all(C);
    let waived = || Evidence::Waived {
        reason: "no full beta exists yet",
        operator: "adi",
    };
    let mut r = request(&f.dist, waived());
    r.target = Target::Prod;
    let why = refused(plan(&r, &a));
    assert!(why.contains("refused for --target prod"), "{why}");
    assert!(a.asked().is_empty(), "a refused waiver asks nobody");
    let doc = plan(&request(&f.dist, waived()), &a).unwrap();
    match &doc.beta {
        Beta::Waived { waiver } => {
            assert_eq!(
                (
                    waiver.what.as_str(),
                    waiver.by.as_str(),
                    waiver.reason.as_str()
                ),
                ("beta", "adi", "no full beta exists yet")
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        a.asked(),
        [format!("artifacts {C}")],
        "no beta result, no beta attestation"
    );
}

#[test]
fn an_incomplete_platform_or_a_damaged_release_is_refused() {
    // Mutants: platform check dropped; SBOM requirement dropped; bundle requirement
    // dropped; `release check` skipped.
    let root = scratch("incomplete");
    let (dist, _) = make_dist(&root, &["linux-x86_64"]);
    let a = Recorded::all(C);
    let why = refused(plan(
        &request(
            &dist,
            Evidence::Waived {
                reason: "r",
                operator: "o",
            },
        ),
        &a,
    ));
    assert!(why.contains("platform darwin-arm64: no tarball"), "{why}");
    // A tarball whose bundle is gone fails the release check (pairing) and the platform.
    std::fs::remove_file(dist.join(format!("kbf-{C}-linux-x86_64.tar.gz.sigstore.json"))).unwrap();
    let why = refused(plan(
        &request(
            &dist,
            Evidence::Waived {
                reason: "r",
                operator: "o",
            },
        ),
        &a,
    ));
    assert!(why.contains("no provenance bundle"), "{why}");
    assert!(why.contains("has no bundle"), "{why}");
    // A tarball whose SBOM the release does not hold.
    let root = scratch("no-sbom");
    let (dist, lines) = make_dist(&root, &["linux-x86_64", "darwin-arm64"]);
    let sbom = format!("kbf-{C}-darwin-arm64.cdx.json");
    std::fs::remove_file(dist.join(&sbom)).unwrap();
    let kept: String = lines
        .iter()
        .filter(|(_, n)| *n != sbom)
        .map(|(s, n)| format!("{s}  {n}\n"))
        .collect();
    std::fs::write(dist.join("SHA256SUMS"), kept).unwrap();
    let why = refused(plan(
        &request(
            &dist,
            Evidence::Waived {
                reason: "r",
                operator: "o",
            },
        ),
        &a,
    ));
    assert!(why.contains("has no SBOM"), "{why}");
    // A byte changed: SHA256SUMS no longer matches.
    let root = scratch("damaged");
    let (dist, _) = make_dist(&root, &["linux-x86_64", "darwin-arm64"]);
    std::fs::write(dist.join(format!("kbf-{C}-darwin-arm64.cdx.json")), "other").unwrap();
    let why = refused(plan(
        &request(
            &dist,
            Evidence::Waived {
                reason: "r",
                operator: "o",
            },
        ),
        &a,
    ));
    assert!(!why.is_empty());
    assert!(a.asked().is_empty());
}

/// The offer for `n` against `p` (their `meta_log` as the identity files state them).
fn reversible_for(name: &str, n: Option<&str>, p: Option<&str>) -> bool {
    let f = fx(name);
    let a = Recorded::all(C);
    let new = identity(&f.root, "new.json", C, n);
    let old = identity(&f.root, "old.json", PREV, p);
    let mut r = request(
        &f.dist,
        Evidence::Waived {
            reason: "r",
            operator: "o",
        },
    );
    r.version_json = Some(&new);
    r.previous = Some(PREV);
    r.previous_version_json = Some(&old);
    let doc = plan(&r, &a).unwrap();
    assert_eq!(
        (doc.meta_log, doc.previous_meta_log),
        (n.map(|s| s.parse().unwrap()), p.map(|s| s.parse().unwrap()))
    );
    doc.reversible
}

#[test]
fn reversibility_follows_meta_log_and_unknown_on_either_side_is_not_reversible() {
    // Mutants: `<=` to `<`, to `>=`; arguments swapped (the 3-against-4 and 4-against-3
    // rows differ); unknown read as 0; the offer refused instead of marked.
    assert!(reversible_for("rev-equal", Some("4"), Some("4")), "equal");
    assert!(
        reversible_for("rev-lower", Some("3"), Some("4")),
        "N lower than N-1"
    );
    assert!(
        !reversible_for("rev-raised", Some("5"), Some("4")),
        "N raised the log"
    );
    assert!(
        !reversible_for("rev-new-unknown", None, Some("4")),
        "N has no meta_log"
    );
    assert!(
        !reversible_for("rev-old-unknown", Some("4"), None),
        "N-1 has no meta_log"
    );
    assert!(
        !reversible_for("rev-both-unknown", None, None),
        "neither has"
    );
    assert!(
        !reversible_for("rev-zero-over-unknown", Some("0"), None),
        "zero is a number"
    );
}

#[test]
fn a_missing_identity_file_or_previous_commit_is_unknown_not_reversible() {
    // The offer is made; the record says reversible: false.
    let f = fx("rev-missing");
    let a = Recorded::all(C);
    let w = || Evidence::Waived {
        reason: "r",
        operator: "o",
    };
    let new = identity(&f.root, "new.json", C, Some("4"));
    let mut r = request(&f.dist, w());
    r.version_json = Some(&new);
    r.previous = Some(PREV);
    let doc = plan(&r, &a).unwrap();
    assert!(!doc.reversible);
    assert_eq!(doc.previous.as_ref().map(CommitId::as_str), Some(PREV));
    let mut r = request(&f.dist, w());
    r.version_json = Some(&new);
    let doc = plan(&r, &a).unwrap();
    assert!(!doc.reversible && doc.previous.is_none());
}

#[test]
fn an_identity_file_of_another_build_or_a_bad_meta_log_is_refused() {
    // Mutants: the commit comparison dropped (a stale file would set reversible).
    let f = fx("rev-bad-identity");
    let a = Recorded::all(C);
    let w = || Evidence::Waived {
        reason: "r",
        operator: "o",
    };
    let stale = identity(&f.root, "new.json", PREV, Some("4"));
    let old = identity(&f.root, "old.json", PREV, Some("4"));
    let mut r = request(&f.dist, w());
    r.version_json = Some(&stale);
    r.previous = Some(PREV);
    r.previous_version_json = Some(&old);
    assert!(refused(plan(&r, &a)).contains("--version-json"));
    let new = identity(&f.root, "ok.json", C, Some("4"));
    r.version_json = Some(&new);
    r.previous_version_json = Some(&new);
    assert!(refused(plan(&r, &a)).contains("--previous-version-json"));
    let text = f.root.join("text.json");
    std::fs::write(
        &text,
        format!("{{\"commit\":\"{}\",\"meta_log\":\"4\"}}", &C[..12]),
    )
    .unwrap();
    r.version_json = Some(&text);
    r.previous_version_json = Some(&old);
    assert!(refused(plan(&r, &a)).contains("not a whole number"));
    let mut r = request(&f.dist, w());
    r.previous = Some(C);
    assert!(refused(plan(&r, &a)).contains("nothing to offer"));
}

#[test]
fn usage_errors_are_refused_before_anything_is_read() {
    let f = fx("usage");
    let a = Recorded::all(C);
    let mut r = request(&f.dist, Evidence::Missing);
    r.commit = "abc";
    assert!(matches!(plan(&r, &a), Err(Error::Usage(_))));
    let mut r = request(&f.dist, Evidence::Missing);
    r.platforms = &[];
    assert!(matches!(plan(&r, &a), Err(Error::Usage(_))));
    let mut r = request(&f.dist, Evidence::Missing);
    let old = f.root.join("old.json");
    r.previous_version_json = Some(&old);
    assert!(matches!(plan(&r, &a), Err(Error::Usage(_))));
}

// ---- the CLI, against the fake gh ----

struct Cli {
    fx: Fx,
    gh: PathBuf,
    out: PathBuf,
}

fn cli(name: &str) -> Cli {
    let f = fx(name);
    let gh_dir = f.root.join("gh");
    std::fs::create_dir_all(&gh_dir).unwrap();
    let gh = gh_dir.join("gh");
    std::fs::hard_link(FAKE, &gh).unwrap();
    let out = f.root.join("offer.json");
    Cli { fx: f, gh, out }
}

impl Cli {
    /// Records the attestations the artifacts workflow made of the release, and, when
    /// `beta` is given, the beta workflow's of that file.
    fn attest(&self, beta: Option<&Path>) {
        let sums = std::fs::read_to_string(self.fx.dist.join("SHA256SUMS")).unwrap();
        let mut store = String::new();
        let signed = format!(
            "--repo {REPO} --signer-workflow {ARTIFACTS} --source-ref refs/heads/main \
             --deny-self-hosted-runners"
        );
        for l in sums.lines() {
            let (sha, name) = l.split_once("  ").unwrap();
            store.push_str(&format!("{sha} {signed}\n"));
            if name.ends_with(".tar.gz") {
                store.push_str(&format!(
                    "{sha} {signed} --predicate-type https://cyclonedx.org/bom\n"
                ));
            }
        }
        if let Some(b) = beta {
            self.beta_bundle(b, C);
        }
        std::fs::write(self.gh.with_file_name("attested"), store).unwrap();
    }

    /// The bundle beside the beta result `b`, attesting it for `commit` under the flags
    /// a host-grade check passes: offline, issuer, predicate type, commit.
    fn beta_bundle(&self, b: &Path, commit: &str) {
        let flags = format!(
            "--repo {REPO} --signer-workflow {BETA} --cert-oidc-issuer \
             https://token.actions.githubusercontent.com --source-ref refs/heads/main \
             --source-digest {commit} --predicate-type https://slsa.dev/provenance/v1 \
             --deny-self-hosted-runners"
        );
        let sha = sha_of(&std::fs::read(b).unwrap());
        let name = format!("{}.sigstore.json", b.file_name().unwrap().to_str().unwrap());
        std::fs::write(b.with_file_name(name), bundle(&sha, Some(&flags))).unwrap();
    }

    fn run(&self, extra: &[&str]) -> (Option<i32>, String, String) {
        let tmp = self.fx.root.join("tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let out = Command::new(XTASK)
            .args(["release", "offer"])
            .arg(&self.fx.dist)
            .args(["--repo", REPO, "--commit", C, "--gh"])
            .arg(&self.gh)
            .arg("--out")
            .arg(&self.out)
            .args(["--platform", "linux-x86_64", "--platform", "darwin-arm64"])
            .args(extra)
            .env("TMPDIR", &tmp)
            .env("NO_PROXY", "*")
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run xtask");
        let t = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), t(&out.stdout), t(&out.stderr))
    }
}

#[test]
fn the_cli_writes_the_offer_after_the_real_gh_calls_and_plan_writes_nothing() {
    // Proves the pinned-gh path end to end: the artifacts and the beta result verify
    // under the exact flags, the offer is written, and `--plan` runs the same checks and
    // leaves no file. Mutant: plan writes; a flag of the beta call changed (the fake
    // verifies only the exact flags).
    let c = cli("cli-accept");
    let beta = put_beta(&c.fx.root, &beta_doc(C, &c.fx.set));
    c.attest(Some(&beta));
    let b = beta.to_str().unwrap();
    let (code, out, err) = c.run(&["--beta-result", b, "--plan"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("plan: would write"), "{out}");
    assert!(out.contains("\"kind\": \"attested\""), "{out}");
    assert!(!c.out.exists(), "--plan wrote a file");
    let (code, out, err) = c.run(&["--beta-result", b]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("NOT reversible: no previous commit given"),
        "{out}"
    );
    let doc = OfferDoc::from_bytes(&std::fs::read(&c.out).unwrap()).unwrap();
    assert_eq!(doc.commit.as_str(), C);
    assert_eq!(
        doc.beta,
        Beta::Attested {
            run_id: "beta-77".to_owned()
        }
    );
}

#[test]
fn the_cli_refuses_and_leaves_no_file() {
    // Each refusal exits 1 and writes nothing, an existing offer.json is left as it was.
    // Mutants: write before the checks finish; swallow a refusal.
    let c = cli("cli-refuse");
    let beta = put_beta(&c.fx.root, &beta_doc(C, &c.fx.set));
    let b = beta.to_str().unwrap();
    // The beta result is not attested (the store holds only the artifacts).
    c.attest(None);
    let (code, _, err) = c.run(&["--beta-result", b]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("is not attested by beta.yml"), "{err}");
    assert!(!c.out.exists());
    // Nothing is attested at all.
    std::fs::write(c.gh.with_file_name("attested"), "").unwrap();
    let (code, _, err) = c.run(&["--waive-beta", "no beta yet", "--operator", "adi"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(!c.out.exists());
    // No beta, no waiver.
    c.attest(None);
    let (code, _, err) = c.run(&[]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("no beta result"), "{err}");
    // A waiver for prod; an earlier offer.json survives a refusal.
    std::fs::write(&c.out, "earlier\n").unwrap();
    let (code, _, err) = c.run(&["--target", "prod", "--waive-beta", "x", "--operator", "adi"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("refused for --target prod"), "{err}");
    assert_eq!(std::fs::read_to_string(&c.out).unwrap(), "earlier\n");
    // A waiver and a result together is a usage error, not a silent choice.
    let (code, _, _) = c.run(&["--beta-result", b, "--waive-beta", "x", "--operator", "adi"]);
    assert_eq!(code, Some(2));
    // A waiver without an operator.
    let (code, _, err) = c.run(&["--waive-beta", "x"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("names its operator"), "{err}");
    assert_eq!(std::fs::read_to_string(&c.out).unwrap(), "earlier\n");
}

#[test]
fn the_cli_records_a_waiver_for_gamma() {
    let c = cli("cli-waiver");
    c.attest(None);
    let (code, out, err) = c.run(&[
        "--waive-beta",
        "no full beta exists yet",
        "--operator",
        "adi",
    ]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("beta: WAIVED by adi (no full beta exists yet)"),
        "{out}"
    );
    let doc = OfferDoc::from_bytes(&std::fs::read(&c.out).unwrap()).unwrap();
    assert!(matches!(doc.beta, Beta::Waived { .. }));
    assert_eq!(doc.target, Target::Gamma);
}

#[test]
fn the_cli_refuses_a_beta_bundle_of_another_commit_and_one_without_flags_pinned() {
    // The beta check is as strict as the tarball check: the fake gh accepts only the
    // exact flag set (issuer, predicate type, source digest, offline root), so a bundle
    // attesting another commit is refused, and the accepting run above pins the flags.
    // Mutant: the beta call drops --cert-oidc-issuer / --predicate-type / the commit.
    let c = cli("cli-beta-other");
    let beta = put_beta(&c.fx.root, &beta_doc(C, &c.fx.set));
    c.attest(None);
    c.beta_bundle(&beta, PREV);
    let (code, _, err) = c.run(&["--beta-result", beta.to_str().unwrap()]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("is not attested by beta.yml"), "{err}");
    assert!(!c.out.exists());
}

#[test]
fn the_beta_attestation_is_asked_with_a_previous_commit_too() {
    // Mutant: the beta attester asked for `previous.unwrap_or(commit)`.
    let f = fx("beta-with-previous");
    let beta = put_beta(&f.root, &beta_doc(C, &f.set));
    let a = Recorded::all(C);
    let new = identity(&f.root, "new.json", C, Some("4"));
    let old = identity(&f.root, "old.json", PREV, Some("4"));
    let mut r = request(&f.dist, Evidence::Result(&beta));
    r.version_json = Some(&new);
    r.previous = Some(PREV);
    r.previous_version_json = Some(&old);
    let doc = plan(&r, &a).unwrap();
    assert!(doc.reversible);
    assert_eq!(a.asked(), [format!("artifacts {C}"), format!("beta {C}")]);
}

#[test]
fn the_cli_has_no_default_out() {
    let c = cli("cli-no-out");
    let out = Command::new(XTASK)
        .args(["release", "offer"])
        .arg(&c.fx.dist)
        .args(["--repo", REPO, "--commit", C, "--platform", "linux-x86_64"])
        .args(["--waive-beta", "x", "--operator", "adi", "--plan"])
        .current_dir(&c.fx.root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!c.fx.root.join("offer.json").exists());
}
