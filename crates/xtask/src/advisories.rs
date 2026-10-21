//! `advisories --repo <owner>/<repo>`: the security advisories against kbf's dependencies,
//! kept in one GitHub issue instead of failing a pull request.
//!
//! `cargo deny check advisories` is not part of the pull-request gate (`ci.yml` checks
//! `bans licenses sources`): a new advisory against a crate we already use would fail every
//! open pull request for something none of them did. `.github/workflows/advisories.yml` runs
//! this verb daily and on every push to `main` instead.
//!
//! The verb runs `cargo deny --log-level warn --manifest-path ./Cargo.toml --all-features
//! check advisories --format json` in the repository root and reads what it prints. With
//! `--format json` cargo-deny (0.20.2, the version `ci.yml` pins) writes to **stderr**, one
//! JSON object per line, and nothing to stdout:
//!
//! - `{"type":"diagnostic","fields":{"code":"vulnerability","severity":"error","message":
//!   "<title>","labels":[...],"notes":[...],"graphs":[{"Krate":{"name":"<crate>",
//!   "version":"<version>"},"parents":[...]}],"advisory":{"id":"RUSTSEC-...","package":
//!   "<crate>","title":"<title>","url":"<url or null>", ...}}}`. `code` is `vulnerability`,
//!   `notice`, `unmaintained` or `unsound`. The `advisory` object is the database entry; the
//!   version of the crate in our lock file is only in `graphs`. A yanked crate
//!   (`deny.toml` sets `yanked = "deny"`) has `"code":"yanked"` and no `advisory`: it is
//!   listed too, under the id `yanked`, so moving the check out of the gate loses nothing;
//! - `{"type":"log","fields":{"level":"ERROR","message":"...","timestamp":"..."}}`: cargo-deny's
//!   own log. An `ERROR` one is a tool failure (a database that could not be fetched, a
//!   `deny.toml` that does not parse).
//!
//! cargo-deny exits 1 when it reported an error-level diagnostic, which every advisory is by
//! default, so its exit status is not the verdict: 0 and 1 are both a finished run, any other
//! (or a signal) is a tool failure, and so is exit 1 with no advisory and no `ERROR` log (the
//! issue must not be closed on a run nobody understood).
//!
//! What it does with the advisories found, at `--commit` (default: `HEAD`), through the
//! issue titled [`TITLE`] (the open one; pull requests are not issues here):
//!
//! | advisories | open issue | does |
//! |---|---|---|
//! | none | none | nothing |
//! | none | yes | comments `No advisories as of <commit>` and closes it |
//! | some | none | opens it, labelled [`LABEL`] when the repository has that label |
//! | some | yes | overwrites its body |
//!
//! It exits 0 in every row: the issue is the signal, the workflow run is informational. It
//! exits 1 only on a tool failure and 2 on a usage error. GitHub is spoken to with `gh api`
//! (REST, an argument array, never a shell); a JSON body goes on stdin and `--input -` is on
//! the argument list, because `gh` ignores a piped body without it ([`gh_args`]).

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::{Error, build_commit, git_stdout};

/// The title of the one issue the advisories are kept in.
pub const TITLE: &str = "Security advisories in kbf's dependencies";

/// The label the issue gets when the repository has it.
pub const LABEL: &str = "security";

/// What follows `cargo` for the check: the arguments `ci.yml` gave `cargo deny`, narrowed to
/// the advisories and in JSON.
pub const DENY_ARGS: [&str; 10] = [
    "deny",
    "--log-level",
    "warn",
    "--manifest-path",
    "./Cargo.toml",
    "--all-features",
    "check",
    "advisories",
    "--format",
    "json",
];

/// Issues GitHub lists per page (its maximum).
const PAGE: usize = 100;

/// The arguments of `advisories`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `github.repository`).
    #[arg(long)]
    pub repo: String,
    /// The commit the advisories were found at, a full SHA (default: `HEAD` of the root).
    #[arg(long)]
    pub commit: Option<String>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// One advisory against a crate in our lock file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Advisory {
    pub id: String,
    pub krate: String,
    pub version: String,
    pub title: String,
    pub url: Option<String>,
}

/// What `cargo deny` did: its exit code (none when a signal ended it) and its stderr.
pub struct DenyRun {
    pub code: Option<i32>,
    pub stderr: String,
}

/// The method of a [`Call`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
}

/// One GitHub REST call: a path (with its query) and, for a write, a JSON body.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
}

impl Call {
    fn get(path: String) -> Self {
        Call {
            method: Method::Get,
            path,
            body: None,
        }
    }

    fn post(path: String, body: Value) -> Self {
        Call {
            method: Method::Post,
            path,
            body: Some(body),
        }
    }

    fn patch(path: String, body: Value) -> Self {
        Call {
            method: Method::Patch,
            path,
            body: Some(body),
        }
    }
}

/// The two programs the verb runs, so its decisions are tested without either.
pub trait Tools {
    /// Run `cargo deny` ([`DENY_ARGS`]) in the repository root.
    fn deny(&mut self) -> Result<DenyRun, Error>;
    /// Make `call`: its JSON reply, or `None` for a GET that GitHub answered with 404.
    fn github(&mut self, call: &Call) -> Result<Option<Value>, Error>;
}

/// The arguments of `gh` for `call`. A body is read from stdin only with `--input -`:
/// without it `gh` ignores what is piped to it and sends an empty request.
pub fn gh_args(call: &Call) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    match call.method {
        Method::Get => {}
        Method::Post => args.extend(["-X".to_owned(), "POST".to_owned()]),
        Method::Patch => args.extend(["-X".to_owned(), "PATCH".to_owned()]),
    }
    args.push(call.path.clone());
    if call.body.is_some() {
        args.extend(["--input".to_owned(), "-".to_owned()]);
    }
    args
}

/// The real programs: `cargo deny` and `gh api`.
pub struct Process<'a> {
    pub cargo: &'a OsStr,
    pub gh: &'a Path,
    pub root: &'a Path,
}

impl Tools for Process<'_> {
    fn deny(&mut self) -> Result<DenyRun, Error> {
        let out = Command::new(self.cargo)
            .args(DENY_ARGS)
            .current_dir(self.root)
            .stdin(Stdio::null())
            .output()
            .map_err(Error::spawn("cargo deny".to_owned()))?;
        Ok(DenyRun {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn github(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        let what = self.gh.display().to_string();
        let mut command = Command::new(self.gh);
        command.args(gh_args(call));
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.stdin(if call.body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().map_err(Error::spawn(what.clone()))?;
        if let (Some(body), Some(mut stdin)) = (&call.body, child.stdin.take()) {
            // A write that fails means `gh` is gone; its exit status says why.
            let _ = stdin.write_all(body.to_string().as_bytes());
        }
        let out = child.wait_with_output().map_err(Error::spawn(what))?;
        let err = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            if call.method == Method::Get && err.contains("(HTTP 404)") {
                return Ok(None);
            }
            return Err(Error::Failed(format!(
                "gh api {}: {}",
                call.path,
                err.trim_end()
            )));
        }
        if out.stdout.is_empty() {
            return Ok(Some(Value::Null));
        }
        serde_json::from_slice(&out.stdout)
            .map(Some)
            .map_err(|e| Error::Failed(format!("gh api {}: not JSON: {e}", call.path)))
    }
}

/// `advisories`: the lines to print when it finished.
pub fn run(args: &Args, root: &Path, cargo: &OsStr) -> Result<Vec<String>, Error> {
    check_repo(&args.repo)?;
    let commit = match &args.commit {
        Some(commit) => {
            build_commit::full_commit("--commit", commit)?;
            commit.clone()
        }
        None => String::from_utf8_lossy(&git_stdout(root, &["rev-parse", "HEAD"])?)
            .trim()
            .to_owned(),
    };
    let mut tools = Process {
        cargo,
        gh: &args.gh,
        root,
    };
    sync(&mut tools, &args.repo, &commit)
}

/// `repo` is `<owner>/<repo>` in the characters GitHub allows, so it is one path segment
/// pair of every URL built from it.
fn check_repo(repo: &str) -> Result<(), Error> {
    let valid = |s: &str| {
        !matches!(s, "" | "." | "..")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match repo.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(()),
        _ => Err(Error::Usage(format!(
            "--repo {repo:?} is not <owner>/<repo>"
        ))),
    }
}

/// Run the check and bring the issue to match it.
pub fn sync(tools: &mut impl Tools, repo: &str, commit: &str) -> Result<Vec<String>, Error> {
    let found = parse(&tools.deny()?)?;
    let open = open_issue(tools, repo)?;
    match (found.is_empty(), open) {
        (true, None) => Ok(vec!["no advisories, and no open issue".to_owned()]),
        (true, Some(number)) => {
            let issue = format!("repos/{repo}/issues/{number}");
            let note = format!("No advisories as of {commit}");
            write(
                tools,
                Call::post(format!("{issue}/comments"), json!({ "body": note })),
            )?;
            let closed = json!({ "state": "closed", "state_reason": "completed" });
            write(tools, Call::patch(issue, closed))?;
            Ok(vec![format!("no advisories: closed issue #{number}")])
        }
        (false, None) => {
            let labels: Vec<&str> = tools
                .github(&Call::get(format!("repos/{repo}/labels/{LABEL}")))?
                .map_or(vec![], |_| vec![LABEL]);
            let body = json!({ "title": TITLE, "body": render(&found, commit), "labels": labels });
            let made = write(tools, Call::post(format!("repos/{repo}/issues"), body))?;
            let url = made["html_url"].as_str().unwrap_or_default();
            Ok(vec![format!("{} advisories: opened {url}", found.len())])
        }
        (false, Some(number)) => {
            let body = json!({ "body": render(&found, commit) });
            write(
                tools,
                Call::patch(format!("repos/{repo}/issues/{number}"), body),
            )?;
            Ok(vec![format!(
                "{} advisories: updated issue #{number}",
                found.len()
            )])
        }
    }
}

/// A write, whose reply must exist (only a GET is answered with `None`).
fn write(tools: &mut impl Tools, call: Call) -> Result<Value, Error> {
    let path = call.path.clone();
    tools
        .github(&call)?
        .ok_or_else(|| Error::Failed(format!("gh api {path}: no reply")))
}

/// The number of the open issue titled [`TITLE`], if there is one. A pull request is not
/// one, though GitHub lists them with the issues.
fn open_issue(tools: &mut impl Tools, repo: &str) -> Result<Option<u64>, Error> {
    for page in 1.. {
        let path = format!("repos/{repo}/issues?state=open&per_page={PAGE}&page={page}");
        let reply = tools
            .github(&Call::get(path.clone()))?
            .ok_or_else(|| Error::Failed(format!("gh api {path}: not found")))?;
        let Some(items) = reply.as_array() else {
            return Err(Error::Failed(format!("gh api {path}: not an array")));
        };
        let found = items
            .iter()
            .find(|i| i["title"] == TITLE && i.get("pull_request").is_none());
        if let Some(issue) = found {
            return issue["number"]
                .as_u64()
                .map(Some)
                .ok_or_else(|| Error::Failed(format!("gh api {path}: an issue with no number")));
        }
        if items.len() < PAGE {
            break;
        }
    }
    Ok(None)
}

/// The advisories in what `cargo deny` printed, sorted and without repeats.
pub fn parse(run: &DenyRun) -> Result<Vec<Advisory>, Error> {
    if !matches!(run.code, Some(0 | 1)) {
        let how = run.code.map_or("was ended by a signal".to_owned(), |c| {
            format!("exited {c}")
        });
        return Err(Error::Failed(format!("cargo deny {how}")));
    }
    let mut found = BTreeSet::new();
    for (n, line) in run.stderr.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = |why: String| Error::Failed(format!("cargo deny output, line {}: {why}", n + 1));
        let value: Value = serde_json::from_str(line).map_err(|e| bad(e.to_string()))?;
        let fields = &value["fields"];
        match value["type"].as_str() {
            Some("log") if fields["level"] == "ERROR" => {
                let message = fields["message"].as_str().unwrap_or_default();
                return Err(Error::Failed(format!("cargo deny: {message}")));
            }
            Some("diagnostic") if !fields.is_object() => return Err(bad("no `fields`".to_owned())),
            Some("diagnostic") => found.extend(advisory_of(fields).map_err(bad)?),
            Some(_) => {}
            None => return Err(bad("no `type`".to_owned())),
        }
    }
    if found.is_empty() && run.code == Some(1) {
        return Err(Error::Failed(
            "cargo deny exited 1 and reported no advisory".to_owned(),
        ));
    }
    Ok(found.into_iter().collect())
}

/// The string at `key` of `object`, or why it is not there.
fn string(object: &Value, key: &str) -> Result<String, String> {
    object[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("no string `{key}`"))
}

/// The crate and version of the first node of a diagnostic's `graphs`.
fn graph_krate(fields: &Value) -> Option<(String, String)> {
    let krate = &fields["graphs"][0]["Krate"];
    Some((
        krate["name"].as_str()?.to_owned(),
        krate["version"].as_str()?.to_owned(),
    ))
}

/// The advisory a diagnostic reports: none for a diagnostic that is not one.
fn advisory_of(fields: &Value) -> Result<Option<Advisory>, String> {
    let graph = graph_krate(fields);
    if let Some(entry) = fields.get("advisory") {
        let version = graph.map_or("unknown".to_owned(), |(_, version)| version);
        return Ok(Some(Advisory {
            id: string(entry, "id")?,
            krate: string(entry, "package")?,
            version,
            title: string(entry, "title")?,
            url: entry["url"].as_str().map(str::to_owned),
        }));
    }
    if fields["code"] != "yanked" {
        return Ok(None);
    }
    let (krate, version) = graph.ok_or("a yanked crate with no graph")?;
    Ok(Some(Advisory {
        id: "yanked".to_owned(),
        krate,
        version,
        title: string(fields, "message")?,
        url: None,
    }))
}

/// A Markdown table cell: one line, no `|`.
fn cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// The body of the issue: what is found at `commit`, as a table.
pub fn render(found: &[Advisory], commit: &str) -> String {
    let mut body = format!(
        "`cargo deny check advisories` reports these against the dependencies in `Cargo.lock` \
         at commit {commit}.\n\n| Advisory | Crate | Version | Title | Link |\n|---|---|---|---|---|\n"
    );
    for a in found {
        let link = a.url.as_deref().unwrap_or("");
        body.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            cell(&a.id),
            cell(&a.krate),
            cell(&a.version),
            cell(&a.title),
            cell(link)
        ));
    }
    body.push_str(
        "\nThe advisories workflow rewrites this issue on every push to `main` and daily, and \
         closes it when none remain. Advisories do not block pull requests.\n",
    );
    body
}

#[cfg(test)]
mod tests;
