//! `release-red --repo <owner/repo> --run-id <id> [--gh gh]`: the red of a run on `main`, kept
//! in one GitHub issue.
//!
//! `.github/workflows/suites.yml` runs this after the suites that no longer gate the release.
//! It reads the run (`repos/{repo}/actions/runs/{id}`) and its jobs (the latest attempt, every
//! page) and acts only on a push to `main`; any other run is reported and left alone. Jobs
//! that have not completed (the job that runs this verb is one) are ignored. Of the completed
//! jobs:
//!
//! - one has the conclusion `failure` or `timed_out`: the run is red, and `Failing` names
//!   those jobs;
//! - else one has a conclusion that is not `success`, `skipped` or `neutral` (`cancelled`,
//!   `action_required`, `stale`): undecided, nothing is changed (a cancelled run proves
//!   nothing, so it never closes the issue);
//! - else the run is green.
//!
//! The issue is the open, non-pull-request issue labelled [`LABEL`] whose title starts with
//! [`PREFIX`]. A red run with none opens one titled `RELEASE-RED: kbf <first failing job>`
//! (and creates the label first when the repository has none); with one it adds a comment and
//! leaves the title and body alone. A green run comments `Fixed: <run name> is green at <sha>
//! (<run url>)` and then closes the issue, unless a newer run of the workflow on `main` exists
//! (a green re-run of an older run does not speak for main); with no issue it does nothing. The body of an issue
//! and of a comment is the same lines, the format the team's alert reader parses:
//!
//! ```text
//! Run: <run html_url>
//! Head: <sha>
//! Last green: <sha or none>
//! Culprits: #12, #13            (or: Culprits: unknown)
//! Failing: <job name>           (one line per failing job, at most 5)
//! Log: <annotation>             (0 to 2 lines)
//! ```
//!
//! `Last green` is the head of the newest successful push run of the same workflow on `main`
//! created before this run, other than this head. `Culprits` are the `(#N)` suffixes of the
//! subjects of the commits from it to this head (in order, without repeats, the newest 20);
//! `unknown` when there is no last green, the comparison lists fewer commits than it counts,
//! or no subject has a number. `Log` is the first two annotations of the first failing job,
//! each on one line of at most 300 characters. These three are read on a best effort: a read
//! GitHub refuses is printed, and the issue is kept with `none`, `unknown` or no `Log` line.
//!
//! The verb exits 0 whatever it found and 1 when GitHub refuses a call. GitHub is spoken to
//! through [`crate::advisories`]' `gh api` calls.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Error;
use crate::advisories::{Call, Process, Tools, check_repo};
use crate::pages::{PER_PAGE, Pages};
use crate::pipeline_watch::jobs_path;

/// The label of the issue.
pub const LABEL: &str = "main-red";

/// What every issue title starts with.
pub const PREFIX: &str = "RELEASE-RED: kbf ";

/// `Failing` lines in one body.
const MAX_FAILING: usize = 5;

/// `Log` lines in one body.
const MAX_LOG: usize = 2;

/// Characters of one `Log` line.
const MAX_LOG_CHARS: usize = 300;

/// Culprits in one body.
const MAX_CULPRITS: usize = 20;

/// Flags of `release-red`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The run to report (on Actions, `GITHUB_RUN_ID`).
    #[arg(long)]
    pub run_id: u64,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The GitHub calls of the verb, so its decisions are tested without `gh`.
pub trait Github {
    /// Make `call`: its JSON reply, or `None` for a GET that GitHub answered with 404.
    fn call(&mut self, call: &Call) -> Result<Option<Value>, Error>;
}

/// The real `gh api`.
struct Gh<'a>(&'a Path);

impl Github for Gh<'_> {
    fn call(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        Process {
            cargo: "cargo".as_ref(),
            gh: self.0,
            root: Path::new("."),
        }
        .github(call)
    }
}

/// The run being reported.
struct Run {
    id: u64,
    url: String,
    head: String,
    branch: String,
    event: String,
    workflow_id: u64,
    name: String,
    created_at: String,
}

/// One completed job.
struct Job {
    id: u64,
    name: String,
    conclusion: String,
}

/// `release-red`: the lines to print when it finished.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    check_repo(&args.repo)?;
    sync(&mut Gh(&args.gh), &args.repo, args.run_id)
}

/// Read the run and bring the issue to match it.
pub fn sync(gh: &mut impl Github, repo: &str, run_id: u64) -> Result<Vec<String>, Error> {
    let run = read_run(gh, repo, run_id)?;
    if run.event != "push" || run.branch != "main" {
        return Ok(vec![format!(
            "run {run_id} is a {} on {}, not a push to main: nothing changed",
            run.event, run.branch
        )]);
    }
    let jobs = completed_jobs(gh, repo, run_id)?;
    let failing: Vec<&Job> = jobs
        .iter()
        .filter(|j| matches!(j.conclusion.as_str(), "failure" | "timed_out"))
        .collect();
    if let Some(first) = failing.first() {
        return red(gh, repo, &run, &failing, first);
    }
    if jobs
        .iter()
        .any(|j| !matches!(j.conclusion.as_str(), "success" | "skipped" | "neutral"))
    {
        return Ok(vec![format!(
            "run {run_id} has a job that was cancelled or is undecided: nothing changed"
        )]);
    }
    green(gh, repo, &run)
}

/// A reply that must exist: a 404 is an error.
fn get(gh: &mut impl Github, path: &str) -> Result<Value, Error> {
    gh.call(&Call::get(path.to_owned()))?
        .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))
}

/// A write, whose reply must exist (only a GET is answered with `None`).
fn write(gh: &mut impl Github, call: Call) -> Result<Value, Error> {
    let path = call.path.clone();
    gh.call(&call)?
        .ok_or_else(|| Error::Failed(format!("gh api {path}: no reply")))
}

/// The items of an array reply, or an error naming `path`.
fn array(reply: &Value, key: Option<&str>, path: &str) -> Result<Vec<Value>, Error> {
    let items = key.map_or(reply, |k| &reply[k]);
    items
        .as_array()
        .cloned()
        .ok_or_else(|| Error::Failed(format!("gh api {path}: not a list")))
}

fn read_run(gh: &mut impl Github, repo: &str, run_id: u64) -> Result<Run, Error> {
    let path = format!("repos/{repo}/actions/runs/{run_id}");
    let reply = get(gh, &path)?;
    let text = |key: &str| {
        reply[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Failed(format!("gh api {path}: no string `{key}`")))
    };
    Ok(Run {
        id: run_id,
        url: text("html_url")?,
        head: text("head_sha")?,
        branch: text("head_branch")?,
        event: text("event")?,
        workflow_id: reply["workflow_id"]
            .as_u64()
            .ok_or_else(|| Error::Failed(format!("gh api {path}: no number `workflow_id`")))?,
        name: text("name")?,
        created_at: text("created_at")?,
    })
}

/// The completed jobs of the latest attempt, in the API's order, from every page.
fn completed_jobs(gh: &mut impl Github, repo: &str, run_id: u64) -> Result<Vec<Job>, Error> {
    let mut jobs = vec![];
    let mut pages = Pages::new(format!("repos/{repo}/actions/runs/{run_id}/jobs"));
    loop {
        let path = jobs_path(repo, run_id, pages.number());
        let items = array(&get(gh, &path)?, Some("jobs"), &path)?;
        let more = pages.more(&items)?;
        let text = |job: &Value, key: &str| job[key].as_str().unwrap_or_default().to_owned();
        jobs.extend(
            items
                .iter()
                .filter(|j| j["status"] == "completed")
                .map(|j| Job {
                    id: j["id"].as_u64().unwrap_or_default(),
                    name: text(j, "name"),
                    conclusion: text(j, "conclusion"),
                }),
        );
        if !more {
            return Ok(jobs);
        }
    }
}

/// The number of the open issue of the red, if there is one. A pull request is not one,
/// though GitHub lists them with the issues.
fn open_issue(gh: &mut impl Github, repo: &str) -> Result<Option<u64>, Error> {
    let mut pages = Pages::new(format!("repos/{repo}/issues?state=open&labels={LABEL}"));
    loop {
        let page = pages.number();
        let path = format!(
            "repos/{repo}/issues?state=open&labels={LABEL}&per_page={PER_PAGE}&page={page}"
        );
        let items = array(&get(gh, &path)?, None, &path)?;
        let found = items.iter().find(|i| {
            i.get("pull_request").is_none()
                && i["title"].as_str().is_some_and(|t| t.starts_with(PREFIX))
        });
        if let Some(issue) = found {
            return issue["number"]
                .as_u64()
                .map(Some)
                .ok_or_else(|| Error::Failed(format!("gh api {path}: an issue with no number")));
        }
        if !pages.more(&items)? {
            return Ok(None);
        }
    }
}

fn red(
    gh: &mut impl Github,
    repo: &str,
    run: &Run,
    failing: &[&Job],
    first: &Job,
) -> Result<Vec<String>, Error> {
    let open = open_issue(gh, repo)?;
    // The context of the red is read on a best effort: a refused read of it is printed and
    // the issue is still kept, with `none`, `unknown` or no `Log` line in its place.
    let mut notes = vec![];
    let mut noted = |what: &str, e: Error| notes.push(format!("could not read {what}: {e}"));
    let green = last_green(gh, repo, run).unwrap_or_else(|e| {
        noted("the last green run", e);
        None
    });
    let culprits = culprits(gh, repo, green.as_deref(), &run.head).unwrap_or_else(|e| {
        noted("the culprits", e);
        "unknown".to_owned()
    });
    let log = log_lines(gh, repo, first.id).unwrap_or_else(|e| {
        noted("the annotations", e);
        vec![]
    });
    let mut lines = vec![
        format!("Run: {}", run.url),
        format!("Head: {}", run.head),
        format!("Last green: {}", green.as_deref().unwrap_or("none")),
        format!("Culprits: {culprits}"),
    ];
    lines.extend(
        failing
            .iter()
            .take(MAX_FAILING)
            .map(|j| format!("Failing: {}", j.name)),
    );
    lines.extend(log.into_iter().map(|l| format!("Log: {l}")));
    let body = lines.join("\n");
    if let Some(number) = open {
        let comments = format!("repos/{repo}/issues/{number}/comments");
        write(gh, Call::post(comments, json!({ "body": body })))?;
        notes.push(format!("red: commented on issue #{number}"));
        return Ok(notes);
    }
    ensure_label(gh, repo)?;
    let issue = json!({
        "title": format!("{PREFIX}{}", first.name),
        "body": body,
        "labels": [LABEL],
    });
    let made = write(gh, Call::post(format!("repos/{repo}/issues"), issue))?;
    let url = made["html_url"].as_str().unwrap_or_default();
    notes.push(format!("red: opened {url}"));
    Ok(notes)
}

fn green(gh: &mut impl Github, repo: &str, run: &Run) -> Result<Vec<String>, Error> {
    let Some(number) = open_issue(gh, repo)? else {
        return Ok(vec!["green, and no open issue".to_owned()]);
    };
    // Only the newest run speaks for main: a green re-run of an older run, while a newer
    // one is red or still running, leaves the issue open for the newer run to decide.
    let path = format!(
        "repos/{repo}/actions/workflows/{}/runs?branch=main&event=push&per_page=1",
        run.workflow_id
    );
    let newest = array(&get(gh, &path)?, Some("workflow_runs"), &path)?;
    if let Some(id) = newest.first().and_then(|r| r["id"].as_u64())
        && id != run.id
    {
        return Ok(vec![format!(
            "green, but run {id} is newer: issue #{number} is left to it"
        )]);
    }
    let issue = format!("repos/{repo}/issues/{number}");
    let note = format!("Fixed: {} is green at {} ({})", run.name, run.head, run.url);
    write(
        gh,
        Call::post(format!("{issue}/comments"), json!({ "body": note })),
    )?;
    let closed = json!({ "state": "closed", "state_reason": "completed" });
    write(gh, Call::patch(issue, closed))?;
    Ok(vec![format!("green: closed issue #{number}")])
}

/// Create the label when the repository has none.
fn ensure_label(gh: &mut impl Github, repo: &str) -> Result<(), Error> {
    if gh
        .call(&Call::get(format!("repos/{repo}/labels/{LABEL}")))?
        .is_some()
    {
        return Ok(());
    }
    let label = json!({
        "name": LABEL,
        "color": "b60205",
        "description": "The release on main is red",
    });
    write(gh, Call::post(format!("repos/{repo}/labels"), label)).map(|_| ())
}

/// The head of the newest successful push run of the run's workflow on `main` that was
/// created before it and is not at its head.
fn last_green(gh: &mut impl Github, repo: &str, run: &Run) -> Result<Option<String>, Error> {
    let path = format!(
        "repos/{repo}/actions/workflows/{}/runs?branch=main&event=push&status=success&per_page=20",
        run.workflow_id
    );
    let runs = array(&get(gh, &path)?, Some("workflow_runs"), &path)?;
    Ok(runs
        .iter()
        .filter(|r| {
            r["created_at"]
                .as_str()
                .is_some_and(|t| t < run.created_at.as_str())
        })
        .filter_map(|r| r["head_sha"].as_str())
        .find(|sha| *sha != run.head)
        .map(str::to_owned))
}

/// The pull requests of the commits from `green` to `head`, as `#12, #13`, or `unknown`.
fn culprits(
    gh: &mut impl Github,
    repo: &str,
    green: Option<&str>,
    head: &str,
) -> Result<String, Error> {
    let unknown = || "unknown".to_owned();
    let Some(green) = green else {
        return Ok(unknown());
    };
    let path = format!("repos/{repo}/compare/{green}...{head}?per_page=100");
    let Some(reply) = gh.call(&Call::get(path.clone()))? else {
        return Ok(unknown());
    };
    let commits = array(&reply, Some("commits"), &path)?;
    if reply["total_commits"].as_u64().unwrap_or_default() > commits.len() as u64 {
        return Ok(unknown());
    }
    let mut numbers: Vec<String> = vec![];
    for commit in &commits {
        let message = commit["commit"]["message"].as_str().unwrap_or_default();
        let number = pull_number(message.lines().next().unwrap_or_default());
        if let Some(n) = number.filter(|n| !numbers.contains(n)) {
            numbers.push(n);
        }
    }
    // The newest, nearest the red head, when there are more.
    let numbers = &numbers[numbers.len().saturating_sub(MAX_CULPRITS)..];
    Ok(if numbers.is_empty() {
        unknown()
    } else {
        numbers.join(", ")
    })
}

/// `#N` for a subject ending in `(#N)`, as a squash merge writes it.
fn pull_number(subject: &str) -> Option<String> {
    let rest = subject.trim_end().strip_suffix(')')?;
    let digits = &rest[rest.rfind("(#")? + 2..];
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| format!("#{digits}"))
}

/// The first annotations of a job's check run, each on one line.
fn log_lines(gh: &mut impl Github, repo: &str, job_id: u64) -> Result<Vec<String>, Error> {
    let path = format!("repos/{repo}/check-runs/{job_id}/annotations");
    let Some(reply) = gh.call(&Call::get(path.clone()))? else {
        return Ok(vec![]);
    };
    Ok(array(&reply, None, &path)?
        .iter()
        .filter_map(|a| a["message"].as_str())
        .map(one_line)
        .filter(|l| !l.is_empty())
        .take(MAX_LOG)
        .collect())
}

/// `text` with every control character a space, whitespace collapsed, cut at
/// [`MAX_LOG_CHARS`].
fn one_line(text: &str) -> String {
    let spaced: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(MAX_LOG_CHARS).collect()
}

#[cfg(test)]
mod tests;
