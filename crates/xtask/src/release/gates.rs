//! `release gates`: the checks a release waits for. The push runs of the workflows `ci`,
//! `beta` and `artifacts` for one commit on one branch must all have completed with the
//! conclusion `success` (docs/design/kbf-cicd.md). It only reads: every call is a GET.
//!
//! A run counts only when it is the `push` run of that workflow's file
//! (`.github/workflows/<file>`, by the run's `path`, never by its display name, which any
//! workflow may copy) for exactly `--sha` on exactly `--branch`. Of several such runs
//! (a re-push, a re-run) the latest decides: the highest run number, then the highest
//! attempt. It waits while the latest run of a workflow has not completed, polling every
//! `--poll-s`, and refuses at once when one is already red, without waiting for the rest.
//!
//! Red, each with its line: a run that concluded anything but `success` (failure,
//! cancelled, skipped, neutral, ...), with the one exception below; no run at all (beta's
//! concurrency group replaces a pending run, so a commit's beta may never have run); and the
//! end of `--timeout-s` with a run still going. A GitHub error is red too: run the job again.
//!
//! The exception, superseded: when a newer commit lands on `--branch`, GitHub cancels the
//! pending `ci`, `beta` and `artifacts` runs of the older one (their concurrency groups), so a
//! run that concluded `cancelled` is not the commit's own verdict. If every run that is not
//! green or going concluded `cancelled`, and the branch head is no longer `--sha` but a commit
//! ahead of it, the verb ends green with `superseded=true`. The newer commit's own run decides
//! whether anything is released: if that push is docs, site or pipeline only, nothing is
//! released until the next code push, whose release (a later tag) includes this commit. The
//! verdict does not wait for the newer commit's checks; if they fail, that commit goes red
//! itself. A `cancelled` run while `--sha` is still the head stays red (someone cancelled the
//! current commit's checks), and so does any other conclusion, beside it or not. A manual
//! cancel of a run of a commit that the branch has since passed also reads as superseded.
//! The head is read with two more GETs (`branches/<branch>`, `compare/<sha>...<head>`), made
//! only when a run was cancelled.
//!
//! Prints `ci_run`, `beta_run` and `artifacts_run` as `key=<id>` lines, then `superseded=false`,
//! and appends them to `--output` (on Actions, `GITHUB_OUTPUT`); `release verdict-write` takes
//! the three ids. A superseded commit prints `superseded=true` alone, and no ids.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::tag::is_repo;
use super::{api, append_output};
use crate::Error;
use crate::build_commit::full_commit;

/// The workflows that gate a release: the output key stem and the file under
/// `.github/workflows/`.
pub const GATES: [(&str, &str); 3] = [
    ("ci", "ci.yml"),
    ("beta", "beta.yml"),
    ("artifacts", "artifacts.yml"),
];

/// The arguments of `release gates`.
#[derive(clap::Args)]
pub struct GatesArgs {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The commit pushed, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub sha: String,
    /// The branch the push was to (on Actions, `GITHUB_REF_NAME`).
    #[arg(long)]
    pub branch: String,
    /// Give up, red, when a run is still going after this many seconds (3 hours).
    #[arg(long, default_value_t = 3 * 60 * 60)]
    pub timeout_s: u64,
    /// Seconds between two looks at the runs.
    #[arg(long, default_value_t = 30)]
    pub poll_s: u64,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// Where one workflow stands for the commit.
#[derive(Debug, PartialEq, Eq)]
enum Standing {
    /// Its latest run has not completed.
    Going(u64),
    /// Its latest run succeeded.
    Green(u64),
    /// It is red, and why.
    Red(String),
    /// Its latest run concluded `cancelled`: red unless the commit was superseded.
    Cancelled(String),
}

/// Whether `name` is a branch name this verb puts in a URL: no byte that needs escaping.
fn is_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/'])
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// A run's `path` without the `@refs/heads/<branch>` GitHub appends to a workflow that ran
/// from a branch. A path with any other `@` (another branch, a tag, a commit) is the file
/// as some other ref had it, so it names no file at all.
fn file_of<'a>(run: &'a Value, branch: &str) -> Option<&'a str> {
    let path = run["path"].as_str()?;
    match path.split_once('@') {
        None => Some(path),
        Some((file, r)) if r.strip_prefix("refs/heads/") == Some(branch) => Some(file),
        Some(_) => None,
    }
}

/// The latest run in `runs` that is the push run of `file` for `sha` on `branch`.
fn latest<'a>(runs: &'a [Value], file: &str, sha: &str, branch: &str) -> Option<&'a Value> {
    let path = format!(".github/workflows/{file}");
    runs.iter()
        .filter(|r| {
            r["head_sha"] == sha
                && r["head_branch"] == branch
                && r["event"] == "push"
                && file_of(r, branch) == Some(path.as_str())
        })
        .max_by_key(|r| {
            (
                r["run_number"].as_u64().unwrap_or(0),
                r["run_attempt"].as_u64().unwrap_or(0),
            )
        })
}

/// Where the workflow `file` stands, from its runs as GitHub listed them.
fn standing(runs: &[Value], file: &str, sha: &str, branch: &str) -> Standing {
    let Some(run) = latest(runs, file, sha, branch) else {
        return Standing::Red(format!(
            "{file}: no push run for {sha} on {branch} (it did not run, or was replaced)"
        ));
    };
    let Some(id) = run["id"].as_u64() else {
        return Standing::Red(format!("{file}: its run for {sha} has no id"));
    };
    if run["status"] != "completed" {
        return Standing::Going(id);
    }
    let why = |conclusion: &str| {
        format!("{file}: run {id} for {sha} on {branch} concluded {conclusion}, not success")
    };
    match run["conclusion"].as_str() {
        Some("success") => Standing::Green(id),
        Some("cancelled") => Standing::Cancelled(why("cancelled")),
        other => Standing::Red(why(other.unwrap_or("nothing"))),
    }
}

/// The head of `branch` when it is a commit ahead of `sha`: the commit that replaced it. `None`
/// when `sha` is still the head, when the head is not a descendant of `sha` (`sha` is not in
/// the branch's history, so a later release would not include it), and when GitHub does not
/// say.
fn replaced_by(gh: &Path, repo: &str, sha: &str, branch: &str) -> Result<Option<String>, Error> {
    let Some(branch_json) = api(gh, &format!("repos/{repo}/branches/{branch}"))? else {
        return Ok(None);
    };
    let Some(head) = branch_json["commit"]["sha"].as_str() else {
        return Ok(None);
    };
    if head == sha || full_commit("the branch head", head).is_err() {
        return Ok(None);
    }
    let compare = api(gh, &format!("repos/{repo}/compare/{sha}...{head}"))?;
    Ok(compare
        .filter(|c| c["status"] == "ahead")
        .map(|_| head.to_owned()))
}

/// The most runs one request asks for. One commit's push runs of one workflow are a handful
/// (a push, a few re-runs), so a full page means the list may be cut short, and the latest
/// run may be on a page not read: `listed` refuses rather than judge a partial list.
const PAGE: usize = 100;

/// The runs of the workflow `file` for `sha` on `branch`, as GitHub lists them.
fn listed(gh: &Path, repo: &str, file: &str, sha: &str, branch: &str) -> Result<Vec<Value>, Error> {
    let runs = api(
        gh,
        &format!(
            "repos/{repo}/actions/workflows/{file}/runs?head_sha={sha}&event=push\
             &branch={branch}&per_page={PAGE}"
        ),
    )?;
    let runs = runs
        .as_ref()
        .and_then(|v| v["workflow_runs"].as_array())
        .cloned()
        .unwrap_or_default();
    if runs.len() >= PAGE {
        return Err(Error::Refused(vec![format!(
            "{file}: GitHub listed {} runs for {sha} on {branch}, a full page; the latest may be \
             on a page this verb does not read",
            runs.len()
        )]));
    }
    Ok(runs)
}

/// The end of a superseded commit: green, with `superseded=true` and no run ids.
fn superseded(a: &GatesArgs, newer: &str) -> Result<Vec<String>, Error> {
    eprintln!(
        "superseded: {} was replaced on {} by {newer}; a later code release includes it",
        a.sha, a.branch
    );
    let lines = vec!["superseded=true".to_owned()];
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}

/// The `release gates` verb: the lines to print.
pub fn run(a: &GatesArgs) -> Result<Vec<String>, Error> {
    if !is_repo(&a.repo) {
        return Err(Error::Usage(format!(
            "--repo {:?} is not <owner>/<name>",
            a.repo
        )));
    }
    full_commit("--sha", &a.sha)?;
    if !is_branch(&a.branch) {
        return Err(Error::Usage(format!(
            "--branch {:?} is not a plain branch name ([A-Za-z0-9._/-])",
            a.branch
        )));
    }
    if a.poll_s == 0 {
        return Err(Error::Usage("--poll-s must be at least 1".to_owned()));
    }
    let start = Instant::now();
    let limit = Duration::from_secs(a.timeout_s);
    let ids = loop {
        let mut states = Vec::new();
        for (_, file) in GATES {
            let runs = listed(&a.gh, &a.repo, file, &a.sha, &a.branch)?;
            states.push(standing(&runs, file, &a.sha, &a.branch));
        }
        let red: Vec<String> = states
            .iter()
            .filter_map(|s| match s {
                Standing::Red(why) | Standing::Cancelled(why) => Some(why.clone()),
                _ => None,
            })
            .collect();
        if !red.is_empty() {
            let only_cancelled = states.iter().all(|s| !matches!(s, Standing::Red(_)));
            if only_cancelled && let Some(newer) = replaced_by(&a.gh, &a.repo, &a.sha, &a.branch)? {
                return superseded(a, &newer);
            }
            return Err(Error::Refused(red));
        }
        let green: Vec<u64> = states
            .iter()
            .filter_map(|s| match s {
                Standing::Green(id) => Some(*id),
                _ => None,
            })
            .collect();
        if green.len() == GATES.len() {
            break green;
        }
        let waited = start.elapsed();
        if waited >= limit {
            return Err(Error::Refused(
                GATES
                    .iter()
                    .zip(&states)
                    .filter_map(|((_, file), s)| match s {
                        Standing::Going(id) => Some(format!(
                            "{file}: run {id} was still going after {} s",
                            a.timeout_s
                        )),
                        _ => None,
                    })
                    .collect(),
            ));
        }
        std::thread::sleep(Duration::from_secs(a.poll_s).min(limit - waited));
    };
    let lines: Vec<String> = GATES
        .iter()
        .zip(&ids)
        .map(|((name, _), id)| format!("{name}_run={id}"))
        .chain(["superseded=false".to_owned()])
        .collect();
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}
