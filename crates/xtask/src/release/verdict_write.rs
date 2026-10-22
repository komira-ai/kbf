//! `release verdict-write`: the Beta record a release tag carries as its message.
//!
//! It records that the gates of one commit passed: the commit, the set the files in `<dir>`
//! hash to ([`set_hash_of`]), the tag the release is planned to get, and the three runs
//! (`ci`, `beta`, `artifacts`) that passed it, each as a green gate and named in the note.
//! It judges nothing itself: the caller has already required those runs to be green. The
//! record is an audit record: `release verify` does not read it, and the release notes
//! only show it.
//!
//! The stage is [`STAGE`], Beta. It records no bake and no load: a bake belongs to the
//! gamma farm check, which is not a stage of this record. Beta tests bytes it rebuilt
//! itself, not the attested ones; the note says so: `tested_bytes: rebuilt`.

use std::path::PathBuf;

use kbf_gate::{CommitId, GateResult, Header, Limit, ResultDoc, Sample, SetHash, Stage};

use super::tags::Version;
use super::verify::set_hash_of;
use crate::Error;
use crate::build_commit::full_commit;

/// The stage of the record a release carries: Beta, the last stage whose runs the release
/// workflow requires.
pub const STAGE: Stage = Stage::Beta;

/// The arguments of `release verdict-write`.
#[derive(clap::Args)]
pub struct WriteArgs {
    /// The verified release files (`SHA256SUMS` among them): the verdict names their set.
    pub dir: PathBuf,
    /// The commit the gates passed, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// The tag the release is planned to get.
    #[arg(long)]
    pub tag: String,
    /// This release run (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The attempt of this run (on Actions, `GITHUB_RUN_ATTEMPT`).
    #[arg(long, default_value_t = 1)]
    pub attempt: u32,
    /// The push run of `ci` that passed for the commit.
    #[arg(long)]
    pub ci_run: u64,
    /// The push run of `beta` that passed for the commit.
    #[arg(long)]
    pub beta_run: u64,
    /// The push run of `artifacts` that passed for the commit.
    #[arg(long)]
    pub artifacts_run: u64,
    /// What makes the release not reversible, one line (repeat for more).
    #[arg(long)]
    pub not_reversible: Vec<String>,
    /// A scenario only a later stage found, one line (repeat for more).
    #[arg(long)]
    pub escape: Vec<String>,
    /// An earlier release this one withdraws, one line (repeat for more).
    #[arg(long)]
    pub withdraws: Vec<String>,
    /// When the gates were decided, unix seconds (default: now).
    #[arg(long)]
    pub at: Option<u64>,
    /// Write the verdict here.
    #[arg(long)]
    pub out: PathBuf,
}

fn green_gate(name: &str, at: u64) -> GateResult {
    GateResult::decide(name, Sample::Value(1.0), Limit::AtLeast(1.0), at, at)
}

/// The record: the document, or what refuses it.
pub fn build(a: &WriteArgs, now: u64) -> Result<ResultDoc, Error> {
    full_commit("--commit", &a.commit)?;
    let commit = CommitId::parse(&a.commit).map_err(|e| Error::Failed(e.to_string()))?;
    if Version::parse_tag(&a.tag).is_none() {
        return Err(Error::Usage(format!(
            "--tag {:?} is not a release tag (v<MAJOR>.<MINOR>.<PATCH>)",
            a.tag
        )));
    }
    let at = a.at.unwrap_or(now);
    let set_hash: SetHash = set_hash_of(&a.dir)?;
    let mut doc = ResultDoc::begin(
        Header {
            stage: STAGE,
            commit,
            set_hash,
            run_id: a.run_id.to_string(),
            attempt: a.attempt,
        },
        at,
    );
    doc.gates = vec![
        green_gate("ci", at),
        green_gate("beta", at),
        green_gate("artifacts", at),
    ];
    doc.status = doc.status_from_gates();
    doc.finished = Some(at);
    doc.tag = Some(a.tag.clone());
    doc.note = Some(format!(
        "gate runs: ci {}, beta {}, artifacts {}; tested_bytes: rebuilt",
        a.ci_run, a.beta_run, a.artifacts_run
    ));
    doc.not_reversible = a.not_reversible.clone();
    doc.escapes = a.escape.clone();
    doc.withdrawn = a.withdraws.clone();
    Ok(doc)
}

/// The `release verdict-write` verb: the lines to print.
pub fn run(a: &WriteArgs) -> Result<Vec<String>, Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let doc = build(a, now)?;
    let bytes = doc.to_bytes().map_err(|e| Error::Failed(e.to_string()))?;
    std::fs::write(&a.out, bytes).map_err(Error::spawn(format!("write {}", a.out.display())))?;
    Ok(vec![format!(
        "wrote the {STAGE:?} record of {} for {} to {}",
        a.tag,
        a.commit,
        a.out.display()
    )])
}
