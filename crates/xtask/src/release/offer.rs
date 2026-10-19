//! `release offer`: the checks that make a commit offerable to a farm
//! (docs/design/kbf-cicd.md, section 5.1), and `offer.json`, the record they leave.
//!
//! `offer` adds no way into a farm: it makes no network write, creates no key, token or
//! setting, and sets no desired state. It reads (the release files, the beta result, the
//! `kbf --version --json` of the two builds, and, through the pinned `gh`, the
//! attestations) and writes one file, `offer.json` ([`kbf_gate::OfferDoc`]), which
//! `kbf-installer deploy --offer` reads. Every check fails closed: any problem found
//! refuses the offer, all of them are listed, and nothing is written. `--plan` runs the
//! same checks and prints the document without writing it.
//!
//! The checks, in the order they run (the first group is offline, the second asks `gh`
//! and runs only when the first found nothing):
//!
//! 1. `<dir>` passes `release check` for `--commit` (sums, names, one bundle per tarball);
//! 2. every `--platform` is complete: a tarball, its SBOM and its bundle;
//! 3. beta: `--beta-result` is a green beta document for this commit and set whose gates
//!    are all green and within their limits, or `--waive-beta <reason> --operator <name>`
//!    is recorded in the offer (never for `--target prod`, never with a result given);
//! 4. reversibility: [`kbf_gate::reversible`] of the `meta_log` in the two builds'
//!    `--version --json` files (`--version-json`, `--previous-version-json`); a missing
//!    file or field is unknown, and unknown is not reversible. The offer is still made;
//!    the document says `reversible: false`;
//! 5. the artifacts' provenance and SBOM attestations for the commit
//!    ([`Attester::artifacts`]: `verify-assets attestations`);
//! 6. the beta result's attestation, for the same commit ([`Attester::beta`]): the
//!    bundle `<result>.sigstore.json` beside it, verified as a host verifies a tarball.
//!
//! The beta attestation does not exist yet (the beta workflow signs nothing): until it
//! does, check 6 refuses every result and `--waive-beta` is the only way past, for gamma.

use std::path::{Path, PathBuf};

use kbf_gate::{
    Beta, CommitId, Limit, OfferDoc, ResultDoc, Sample, SetHash, Stage, Target, Verdict, Waiver,
    judge, reversible, write_atomic,
};

use super::{ARTIFACTS_WORKFLOW, MAIN, verdict};
use crate::Error;
use crate::build_commit::full_commit;
use crate::package::Platform;
use crate::sums::{self, SUMS};

/// The workflow that is to attest the beta result (planned: the beta workflow signs
/// nothing today, so [`Gh::beta`] refuses every result until it does).
pub const BETA_WORKFLOW: &str = "beta.yml";

/// The most a `kbf --version --json` file may hold.
const IDENTITY_CAP: u64 = 8192;

/// The attestation checks, behind a seam so tests use recorded answers.
pub trait Attester {
    /// The artifacts of `commit` in `dir` carry the attestations of the artifacts workflow.
    fn artifacts(&self, dir: &Path, commit: &str) -> Result<(), Error>;
    /// The beta result file `result` is attested by the beta workflow for `commit`.
    fn beta(&self, result: &Path, commit: &str) -> Result<(), Error>;
}

/// The attestations as the pinned `gh` verifies them.
pub struct Gh<'a> {
    /// The repository, `<owner>/<repo>`.
    pub repo: &'a str,
    /// A `trusted_root.jsonl`, as `verify-assets attestations` takes it.
    pub trusted_root: Option<&'a Path>,
    /// The `gh` program.
    pub gh: &'a Path,
    /// An empty directory for the verification's flipped copy.
    pub scratch: &'a Path,
}

impl Attester for Gh<'_> {
    fn artifacts(&self, dir: &Path, commit: &str) -> Result<(), Error> {
        let workflow_ref = format!("{}/.github/workflows/{ARTIFACTS_WORKFLOW}", self.repo);
        let source_ref = format!("refs/heads/{MAIN}");
        let a = sums::Attestations {
            repo: self.repo,
            workflow_ref: &workflow_ref,
            source_ref: &source_ref,
            commit,
            trusted_root: self.trusted_root,
            gh: self.gh,
            scratch: self.scratch,
        };
        sums::verify_attestations(dir, &a).map(drop)
    }

    fn beta(&self, result: &Path, commit: &str) -> Result<(), Error> {
        // As strict as a host's check of a tarball: the result's own bundle beside it,
        // offline with a trusted root, the issuer, the predicate type, the commit.
        let signer = format!("{}/.github/workflows/{BETA_WORKFLOW}", self.repo);
        let source_ref = format!("refs/heads/{MAIN}");
        let a = sums::Attestations {
            repo: self.repo,
            workflow_ref: &signer,
            source_ref: &source_ref,
            commit,
            trusted_root: self.trusted_root,
            gh: self.gh,
            scratch: self.scratch,
        };
        let name = result
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let bundle = result.with_file_name(crate::bundles::name_of(&name));
        let (verified, said) = a.bundle_run(result, &bundle, &signer, commit)?;
        if !verified {
            return Err(Error::Failed(format!(
                "{} is not attested by {BETA_WORKFLOW} for {commit} (bundle {}): {said}",
                result.display(),
                bundle.display()
            )));
        }
        // The control: the same result and bundle must not verify for another commit.
        let other = crate::bundles::other_commit(commit);
        if a.bundle_run(result, &bundle, &signer, &other)?.0 {
            return Err(Error::Failed(format!(
                "control failed: the beta bundle of {} verified for commit {other}, not the \
                 commit it attests",
                result.display()
            )));
        }
        Ok(())
    }
}

/// What the operator gives as the beta evidence.
pub enum Evidence<'a> {
    /// A beta result document, which must be attested.
    Result(&'a Path),
    /// No result: the operator waives beta.
    Waived {
        /// Why.
        reason: &'a str,
        /// Who.
        operator: &'a str,
    },
    /// Nothing: refused.
    Missing,
}

/// What an offer is made of.
pub struct Request<'a> {
    /// The verified release files (`SHA256SUMS` among them).
    pub dir: &'a Path,
    /// The commit offered, a full SHA.
    pub commit: &'a str,
    /// The stage offered to.
    pub target: Target,
    /// The platforms the farm runs.
    pub platforms: &'a [Platform],
    /// The beta evidence.
    pub beta: Evidence<'a>,
    /// `kbf --version --json` of the offered build.
    pub version_json: Option<&'a Path>,
    /// The commit the farm runs now, a full SHA.
    pub previous: Option<&'a str>,
    /// `kbf --version --json` of the previous build.
    pub previous_version_json: Option<&'a Path>,
    /// Unix seconds, for the document's `created`.
    pub now: u64,
}

/// The problems an `Error` stands for, one per line.
fn lines_of(e: Error) -> Vec<String> {
    match e {
        Error::Refused(lines) => lines,
        other => vec![other.to_string()],
    }
}

/// The `meta_log` the identity file `path` states for the build of `commit`: `None` when
/// the file lacks the field (a build before it). A file that is not the build's identity
/// is a problem, not an unknown.
fn meta_log_of(path: &Path, commit: &str, flag: &str) -> Result<Option<u64>, String> {
    let at = |e: &dyn std::fmt::Display| format!("{flag} {}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(|e| at(&e))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(file, IDENTITY_CAP + 1), &mut text)
        .map_err(|e| at(&e))?;
    if text.len() as u64 > IDENTITY_CAP {
        return Err(at(&format!("over {IDENTITY_CAP} bytes")));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| at(&format!("not JSON: {e}")))?;
    let stamp = value["commit"].as_str();
    let short = &commit[..12];
    if stamp != Some(short) {
        return Err(at(&format!("names commit {stamp:?}, not {short}")));
    }
    match value.get("meta_log") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| at(&format!("meta_log {v} is not a whole number"))),
    }
}

/// What is wrong with a beta result for this commit and set. A gate counts only when
/// it is green as recorded and green when judged again from its numbers.
fn beta_problems(doc: &ResultDoc, commit: &str, set: &SetHash) -> Vec<String> {
    let mut problems = Vec::new();
    if doc.stage != Stage::Beta {
        problems.push(format!("the beta result is for stage {:?}", doc.stage));
    }
    if !doc.passed() {
        problems.push(format!("the beta result is {:?}, not green", doc.status));
    }
    if doc.commit.as_str() != commit {
        problems.push(format!(
            "the beta result names commit {}, not {commit}",
            doc.commit.as_str()
        ));
    }
    if &doc.set_hash != set {
        problems.push(format!(
            "the beta result names set {}, the files are set {}",
            doc.set_hash.as_str(),
            set.as_str()
        ));
    }
    if doc.gates.is_empty() {
        problems.push("the beta result decided no gate".to_owned());
    }
    for g in &doc.gates {
        let sample = g.measured.map_or(Sample::Absent, Sample::Value);
        let again = judge(sample, g.threshold);
        if g.verdict != Verdict::Green || again != Verdict::Green {
            problems.push(format!(
                "beta gate `{}` is not green (recorded {:?}, measured {:?} against {})",
                g.name,
                g.verdict,
                g.measured,
                describe(g.threshold)
            ));
        }
    }
    problems
}

fn describe(l: Limit) -> String {
    match l {
        Limit::AtMost(x) => format!("at most {x}"),
        Limit::AtLeast(x) => format!("at least {x}"),
        Limit::Band { warn, red } => format!("warn {warn}, red {red}"),
    }
}

/// What is missing for `platform` in the listed files of `dir`: its tarball, the SBOM
/// beside it, the bundle beside it.
fn incomplete(dir: &Path, listed: &[String], commit: &str, platform: Platform) -> Vec<String> {
    let suffix = format!("-{commit}-{}.tar.gz", platform.name());
    let tarballs: Vec<&String> = listed.iter().filter(|n| n.ends_with(&suffix)).collect();
    if tarballs.is_empty() {
        return vec![format!(
            "platform {}: no tarball of {commit}",
            platform.name()
        )];
    }
    let mut missing = Vec::new();
    for t in tarballs {
        let sbom = format!("{}.cdx.json", t.trim_end_matches(".tar.gz"));
        if !listed.contains(&sbom) {
            missing.push(format!(
                "platform {}: {t} has no SBOM {sbom}",
                platform.name()
            ));
        }
        let bundle = crate::bundles::name_of(t);
        if !dir.join(&bundle).is_file() {
            missing.push(format!(
                "platform {}: {t} has no bundle {bundle}",
                platform.name()
            ));
        }
    }
    missing
}

/// Runs every check; the offer, or every problem found. Nothing is written.
pub fn plan(r: &Request, attester: &dyn Attester) -> Result<OfferDoc, Error> {
    full_commit("--commit", r.commit)?;
    if let Some(p) = r.previous {
        full_commit("--previous", p)?;
    }
    if r.platforms.is_empty() {
        return Err(Error::Usage("at least one --platform is needed".to_owned()));
    }
    if r.previous.is_none() && r.previous_version_json.is_some() {
        return Err(Error::Usage(
            "--previous-version-json without --previous".to_owned(),
        ));
    }
    if r.target == Target::Prod && matches!(r.beta, Evidence::Waived { .. }) {
        return Err(Error::Refused(vec![
            "--waive-beta is refused for --target prod: prod needs the attested beta result"
                .to_owned(),
        ]));
    }
    let mut problems = Vec::new();

    // 1. The release.
    if let Err(e) = super::check(r.dir, r.commit) {
        problems.extend(lines_of(e));
    }
    let set_hash = verdict::set_hash_of(r.dir)?;
    // 2. The platforms.
    let mut platforms: Vec<String> = Vec::new();
    match sums::read_sums(&r.dir.join(SUMS)) {
        Ok(lines) => {
            let listed: Vec<String> = lines.into_iter().map(|l| l.name).collect();
            for p in r.platforms {
                let missing = incomplete(r.dir, &listed, r.commit, *p);
                if missing.is_empty() {
                    platforms.push(p.name().to_owned());
                }
                problems.extend(missing);
            }
        }
        Err(e) => problems.extend(lines_of(e)),
    }
    platforms.sort();
    platforms.dedup();
    // 3. Beta.
    let beta = match &r.beta {
        Evidence::Missing => {
            problems.push(
                "no beta result: give --beta-result, or waive it with --waive-beta <reason> \
                 --operator <name> (gamma only)"
                    .to_owned(),
            );
            None
        }
        Evidence::Waived { reason, operator } => match Waiver::new("beta", operator, reason) {
            Ok(waiver) => Some(Beta::Waived { waiver }),
            Err(_) => {
                problems.push(
                    "a beta waiver names its operator (--operator) and its reason".to_owned(),
                );
                None
            }
        },
        Evidence::Result(path) => match read_beta(path) {
            Ok(doc) => {
                let found = beta_problems(&doc, r.commit, &set_hash);
                if found.is_empty() {
                    Some(Beta::Attested { run_id: doc.run_id })
                } else {
                    problems.extend(found);
                    None
                }
            }
            Err(e) => {
                problems.push(e);
                None
            }
        },
    };
    // 4. Reversibility.
    let mut meta = |path: Option<&Path>, commit: &str, flag: &str| match path {
        None => None,
        Some(p) => meta_log_of(p, commit, flag).unwrap_or_else(|e| {
            problems.push(e);
            None
        }),
    };
    let meta_log = meta(r.version_json, r.commit, "--version-json");
    let previous_meta_log = r
        .previous
        .and_then(|p| meta(r.previous_version_json, p, "--previous-version-json"));
    if r.previous == Some(r.commit) {
        problems.push("--previous is the commit offered: there is nothing to offer".to_owned());
    }
    // 5 and 6. Attestations, only for a release that is otherwise acceptable.
    if problems.is_empty() {
        if let Err(e) = attester.artifacts(r.dir, r.commit) {
            problems.extend(lines_of(e));
        }
        if let Evidence::Result(path) = &r.beta
            && let Err(e) = attester.beta(path, r.commit)
        {
            problems.extend(lines_of(e));
        }
    }
    let (Some(beta), true) = (beta, problems.is_empty()) else {
        return Err(Error::Refused(problems));
    };
    let doc = OfferDoc {
        version: kbf_gate::OFFER_VERSION,
        created: r.now,
        target: r.target,
        commit: parse_id(r.commit)?,
        set_hash,
        platforms,
        beta,
        meta_log,
        previous: r.previous.map(parse_id).transpose()?,
        previous_meta_log,
        reversible: reversible(meta_log, previous_meta_log),
    };
    doc.validate()
        .map_err(|e| Error::Refused(vec![e.to_string()]))?;
    Ok(doc)
}

fn parse_id(s: &str) -> Result<CommitId, Error> {
    CommitId::parse(s).map_err(|e| Error::Usage(e.to_string()))
}

fn read_beta(path: &Path) -> Result<ResultDoc, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("--beta-result {}: {e}", path.display()))?;
    ResultDoc::from_bytes(&bytes)
        .map_err(|e| format!("--beta-result {} is no usable result: {e}", path.display()))
}

/// Writes the offer to `out`, atomically.
pub fn write(out: &Path, doc: &OfferDoc) -> Result<(), Error> {
    let bytes = doc.to_bytes().map_err(|e| Error::Failed(e.to_string()))?;
    write_atomic(out, &bytes).map_err(Error::spawn(format!("write {}", out.display())))
}

/// The lines `--plan` and an applied offer print: what was read, what is recorded.
pub fn summary(doc: &OfferDoc) -> Vec<String> {
    let beta = match &doc.beta {
        Beta::Attested { run_id } => {
            format!("beta: attested result of run {run_id}, every gate green")
        }
        Beta::Waived { waiver } => format!("beta: WAIVED by {} ({})", waiver.by, waiver.reason),
    };
    let back = match (doc.reversible, doc.previous.as_ref()) {
        (true, Some(p)) => format!("reversible to {}", p.as_str()),
        (false, Some(p)) => format!(
            "NOT reversible to {} (meta_log {:?} against {:?}; unknown counts as not)",
            p.as_str(),
            doc.meta_log,
            doc.previous_meta_log
        ),
        _ => "NOT reversible: no previous commit given".to_owned(),
    };
    vec![
        format!(
            "offer of {} (set {}) to {:?}: platforms {}",
            doc.commit.as_str(),
            doc.set_hash.as_str(),
            doc.target,
            doc.platforms.join(", ")
        ),
        beta,
        back,
    ]
}

/// The arguments of `release offer`.
#[derive(clap::Args)]
pub struct OfferArgs {
    /// The verified release files (`SHA256SUMS` among them).
    pub dir: PathBuf,
    /// The repository, `<owner>/<repo>`.
    #[arg(long)]
    pub repo: String,
    /// The commit offered, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// The stage offered to.
    #[arg(long, value_enum, default_value = "gamma")]
    pub target: TargetArg,
    /// A platform the farm runs (repeat); each must be complete in the release.
    #[arg(long = "platform", value_enum, required = true)]
    pub platforms: Vec<Platform>,
    /// The beta result document (`beta-result.json`), which must be attested.
    #[arg(long, conflicts_with = "waive_beta")]
    pub beta_result: Option<PathBuf>,
    /// Offer without a beta result, for this reason; recorded in the offer. Gamma only.
    #[arg(long)]
    pub waive_beta: Option<String>,
    /// Who waives beta (recorded with the reason).
    #[arg(long, requires = "waive_beta")]
    pub operator: Option<String>,
    /// `kbf --version --json` of the offered build (for `meta_log`).
    #[arg(long)]
    pub version_json: Option<PathBuf>,
    /// The commit the farm runs now, a full SHA.
    #[arg(long)]
    pub previous: Option<String>,
    /// `kbf --version --json` of the previous build.
    #[arg(long)]
    pub previous_version_json: Option<PathBuf>,
    /// A `trusted_root.jsonl` (`gh attestation trusted-root`) to verify with.
    #[arg(long)]
    pub trusted_root: Option<PathBuf>,
    /// Run every check and print the offer; write nothing.
    #[arg(long)]
    pub plan: bool,
    /// Where the offer is written (no default: a refusal never leaves a stale file with
    /// a default name that looks current).
    #[arg(long)]
    pub out: PathBuf,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The stage `--target` names.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum TargetArg {
    Gamma,
    Prod,
}

/// The `release offer` verb: the lines to print.
pub fn run(a: &OfferArgs) -> Result<Vec<String>, Error> {
    let scratch = std::env::temp_dir().join(format!("xtask-offer-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)
        .map_err(Error::spawn(format!("create {}", scratch.display())))?;
    let gh = Gh {
        repo: &a.repo,
        trusted_root: a.trusted_root.as_deref(),
        gh: &a.gh,
        scratch: &scratch,
    };
    let beta = match (&a.beta_result, &a.waive_beta) {
        (Some(p), _) => Evidence::Result(p),
        (None, Some(reason)) => Evidence::Waived {
            reason,
            operator: a.operator.as_deref().unwrap_or(""),
        },
        (None, None) => Evidence::Missing,
    };
    let request = Request {
        dir: &a.dir,
        commit: &a.commit,
        target: match a.target {
            TargetArg::Gamma => Target::Gamma,
            TargetArg::Prod => Target::Prod,
        },
        platforms: &a.platforms,
        beta,
        version_json: a.version_json.as_deref(),
        previous: a.previous.as_deref(),
        previous_version_json: a.previous_version_json.as_deref(),
        now: kbf_gate::system_clock(),
    };
    let planned = plan(&request, &gh);
    // Nothing in it is needed after the run, whatever its result.
    let _ = std::fs::remove_dir_all(&scratch);
    let doc = planned?;
    let mut lines = summary(&doc);
    if a.plan {
        let text = doc.to_bytes().map_err(|e| Error::Failed(e.to_string()))?;
        lines.push(format!("plan: would write {}:", a.out.display()));
        lines.push(String::from_utf8_lossy(&text).trim_end().to_owned());
    } else {
        write(&a.out, &doc)?;
        lines.push(format!("wrote {}", a.out.display()));
    }
    Ok(lines)
}
