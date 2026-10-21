//! `release verdict-write`: the verdict document a release tag carries as its message.
//!
//! It records that the gates of one commit passed: the commit, the set the files in `<dir>`
//! hash to ([`set_hash_of`]), the tag the release is planned to get, and the three runs
//! (`ci`, `beta`, `artifacts`) that passed it, each as a green gate and named in the note.
//! It judges nothing itself: the caller has already required those runs to be green.
//!
//! The stage is [`super::verdict::GATE_STAGE`]. A Gamma verdict says a bake happened, so it is refused
//! unless `--bake-file` holds one: a JSON object with `bake` (`start`, `end`, unix seconds)
//! that lasted [`MIN_BAKE_SECS`] and `load` (`actions`, `builds`, `hours`, each
//! `measured` and `required`) whose floors are met. A bake record for any other stage is
//! refused too, rather than carried along unchecked.
//!
//! Beta tests bytes it rebuilt itself, not the attested ones;
//! the note says so: `tested_bytes: rebuilt`.

use std::path::{Path, PathBuf};

use kbf_gate::{
    CommitId, GateResult, Header, Limit, MinLoad, ResultDoc, Sample, SetHash, Stage, Window,
};
use serde_json::Value;

use super::tags::Version;
use super::verdict::{MIN_BAKE_SECS, set_hash_of};
use crate::Error;
use crate::build_commit::full_commit;

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
    /// The bake record, for a Gamma verdict.
    #[arg(long)]
    pub bake_file: Option<PathBuf>,
    /// When the gates were decided, unix seconds (default: now).
    #[arg(long)]
    pub at: Option<u64>,
    /// Write the verdict here.
    #[arg(long)]
    pub out: PathBuf,
}

/// A bake record: the bake's span and the load it saw.
struct Bake {
    window: Window,
    load: MinLoad,
}

fn bake_of(file: &Path) -> Result<Bake, Error> {
    let bytes = crate::package::read(file)?;
    let at = file.display();
    let doc: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Usage(format!("--bake-file {at}: not JSON: {e}")))?;
    let part = |key: &str| {
        doc.get(key)
            .cloned()
            .ok_or_else(|| Error::Usage(format!("--bake-file {at}: no `{key}`")))
    };
    let window = serde_json::from_value(part("bake")?)
        .map_err(|e| Error::Usage(format!("--bake-file {at}: `bake`: {e}")))?;
    let load = serde_json::from_value(part("load")?)
        .map_err(|e| Error::Usage(format!("--bake-file {at}: `load`: {e}")))?;
    Ok(Bake { window, load })
}

/// Why a Gamma verdict may not be written for this bake (none, or one that is no bake).
fn bake_problems(bake: Option<&Bake>) -> Vec<String> {
    let Some(bake) = bake else {
        return vec![
            "a Gamma verdict says a bake happened: --bake-file is required for it".to_owned(),
        ];
    };
    let mut problems = Vec::new();
    match bake
        .window
        .end
        .map(|end| end.checked_sub(bake.window.start))
    {
        Some(Some(secs)) if secs >= MIN_BAKE_SECS => {}
        Some(Some(secs)) => problems.push(format!(
            "the bake lasted {secs} s, less than {MIN_BAKE_SECS} s (24 h)"
        )),
        Some(None) => problems.push("the bake ends before it starts".to_owned()),
        None => problems.push("the bake record has no end".to_owned()),
    }
    if !bake.load.met() {
        problems.push("the bake's minimum load was not met".to_owned());
    }
    problems
}

fn green_gate(name: &str, at: u64) -> GateResult {
    GateResult::decide(name, Sample::Value(1.0), Limit::AtLeast(1.0), at, at)
}

/// The verdict for `stage`: the document, or what refuses it.
pub fn build(a: &WriteArgs, stage: Stage, now: u64) -> Result<ResultDoc, Error> {
    full_commit("--commit", &a.commit)?;
    let commit = CommitId::parse(&a.commit).map_err(|e| Error::Failed(e.to_string()))?;
    if Version::parse_tag(&a.tag).is_none() {
        return Err(Error::Usage(format!(
            "--tag {:?} is not a release tag (v<MAJOR>.<MINOR>.<PATCH>)",
            a.tag
        )));
    }
    let bake = a.bake_file.as_deref().map(bake_of).transpose()?;
    match stage {
        Stage::Gamma => {
            let problems = bake_problems(bake.as_ref());
            if !problems.is_empty() {
                return Err(Error::Refused(problems));
            }
        }
        _ if bake.is_some() => {
            return Err(Error::Usage(format!(
                "--bake-file: a bake record belongs to a Gamma verdict, this is {stage:?}"
            )));
        }
        _ => {}
    }
    let at = a.at.unwrap_or(now);
    let set_hash: SetHash = set_hash_of(&a.dir)?;
    let mut doc = ResultDoc::begin(
        Header {
            stage,
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
    if let Some(bake) = bake {
        doc.bake = Some(bake.window);
        doc.load = Some(bake.load);
    }
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
pub fn run(a: &WriteArgs, stage: Stage) -> Result<Vec<String>, Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let doc = build(a, stage, now)?;
    let bytes = doc.to_bytes().map_err(|e| Error::Failed(e.to_string()))?;
    std::fs::write(&a.out, bytes).map_err(Error::spawn(format!("write {}", a.out.display())))?;
    Ok(vec![format!(
        "wrote the {stage:?} verdict of {} for {} to {}",
        a.tag,
        a.commit,
        a.out.display()
    )])
}
