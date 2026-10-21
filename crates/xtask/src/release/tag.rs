//! `release tag`: the annotated release tag at one commit, made through the GitHub API.
//!
//! Create-only. A tag is made with two POSTs (`git/tags`, then `git/refs`) and no call ever
//! updates, moves or deletes a ref: a tag that exists is read, and it is either exactly
//! what this run would have made or the run fails.
//!
//! In order, the first that fails refuses:
//!
//! 1. `--sha` is a full commit and `--tag` is a release tag (`v<MAJOR>.<MINOR>.<PATCH>`);
//! 2. Rule S ([`tags::superseded`]) and the `--tag` rule (`tag_is_current`), the same two
//!    `publish` runs, before any write: a re-run of an old run tags nothing;
//! 3. when `--tag` exists, it is a re-run. It is reused (`created=false`) when it is
//!    annotated, names exactly this commit, carries `--message-file` as its message and was
//!    made by `--tagger`; otherwise the run fails and nothing is written;
//! 4. else the tag is created (`created=true`).
//!
//! A transient failure (an HTTP 5xx or 429, or a ref that "already exists" right after this
//! run asked for it, which is a lost reply) starts steps 3 and 4 over once. A second one
//! fails the run. The retry reads before it writes, so it never makes a second tag.
//!
//! The identity that tags is the token `gh` is given (`GH_TOKEN`); this verb takes none.
//! The tag ruleset, not this check, is what keeps anyone else from tagging. The tagger a
//! tag object records is what GitHub fills in for the token's identity, and the API docs do
//! not say what that is for an App installation token. [`check_tagger`] therefore compares
//! it with an expected login that is a flag ([`BOT_LOGIN`] by default), not a pinned guess.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::tags::{self, Version};
use super::{append_output, tag_is_current};
use crate::Error;
use crate::build_commit::full_commit;

/// The login GitHub is expected to record as the tagger of a tag the release App made
/// (`komira-release-bot`). Not pinned offline: see the module docs.
pub const BOT_LOGIN: &str = "komira-release-bot[bot]";

/// The arguments of `release tag`.
#[derive(clap::Args)]
pub struct TagArgs {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The commit to tag, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub sha: String,
    /// The tag to make, `v<MAJOR>.<MINOR>.<PATCH>` (`release plan`'s `tag=`).
    #[arg(long)]
    pub tag: String,
    /// The tag's message, the Beta record `release verdict-write` wrote.
    #[arg(long)]
    pub message_file: PathBuf,
    /// The login the tagger of a tag that already exists must have.
    #[arg(long, default_value = BOT_LOGIN)]
    pub tagger: String,
    /// Append the lines to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// Why one try of the tag did not finish.
enum Step {
    /// Try again, once: a failure that says nothing about the tag.
    Retry(String),
    /// Stop: the run fails with this.
    Stop(Error),
}

impl Step {
    /// The error a step that will not be tried again is.
    fn fail(self) -> Error {
        match self {
            Step::Retry(why) => Error::Failed(format!("{why} (transient)")),
            Step::Stop(e) => e,
        }
    }
}

impl From<Error> for Step {
    fn from(e: Error) -> Self {
        Step::Stop(e)
    }
}

/// `gh api <path>`: the JSON it printed, or `None` for a GET that GitHub answered with
/// 404. With a `body` the call is a POST of it.
fn call(gh: &Path, path: &str, body: Option<&Value>) -> Result<Option<Value>, Step> {
    let mut command = Command::new(gh);
    command.arg("api").arg(path);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if body.is_some() {
        command.args(["-X", "POST", "--input", "-"]);
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(Error::spawn(gh.display().to_string()))?;
    if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
        // A write that fails means `gh` is gone; its exit status says why.
        let _ = stdin.write_all(body.to_string().as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if body.is_none() && err.contains("(HTTP 404)") {
            return Ok(None);
        }
        let why = format!("gh api {path}: {}", err.trim_end());
        let lost_reply = body.is_some() && err.contains("Reference already exists");
        if err.contains("(HTTP 5") || err.contains("(HTTP 429") || lost_reply {
            return Err(Step::Retry(why));
        }
        return Err(Step::Stop(Error::Failed(why)));
    }
    serde_json::from_slice(&out.stdout)
        .map(Some)
        .map_err(|e| Step::Stop(Error::Failed(format!("gh api {path}: not JSON: {e}"))))
}

/// An annotated tag as the API shows it.
struct Held {
    /// What the ref points at: a `tag` object, or a `commit` (a lightweight tag).
    ref_kind: String,
    /// The tag object's name.
    name: String,
    /// The kind of object it names (`commit` for a release).
    kind: String,
    /// The object it names.
    object: String,
    /// Its message, verbatim.
    message: String,
    /// The login the API records as its tagger.
    tagger: String,
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// The tag `tag` of `repo`: none when there is no such ref, and a `Held` with no name or
/// message when the ref is a lightweight tag.
fn held(gh: &Path, repo: &str, tag: &str) -> Result<Option<Held>, Step> {
    let Some(r) = call(gh, &format!("repos/{repo}/git/ref/tags/{tag}"), None)? else {
        return Ok(None);
    };
    let ref_kind = text(&r["object"]["type"]);
    let oid = text(&r["object"]["sha"]);
    if ref_kind != "tag" {
        return Ok(Some(Held {
            ref_kind,
            name: String::new(),
            kind: String::new(),
            object: oid,
            message: String::new(),
            tagger: String::new(),
        }));
    }
    let Some(t) = call(gh, &format!("repos/{repo}/git/tags/{oid}"), None)? else {
        return Err(Error::Failed(format!(
            "ref tags/{tag} points at tag object {oid}, which GitHub does not have"
        ))
        .into());
    };
    Ok(Some(Held {
        ref_kind,
        name: text(&t["tag"]),
        kind: text(&t["object"]["type"]),
        object: text(&t["object"]["sha"]),
        message: text(&t["message"]),
        tagger: text(&t["tagger"]["name"]),
    }))
}

/// Git ends every tag message with one line end whatever it was given, so messages are
/// compared without their trailing line ends.
fn same_message(a: &str, b: &str) -> bool {
    a.trim_end_matches('\n') == b.trim_end_matches('\n')
}

impl Held {
    /// Why this tag is not the one `release tag` would make for `sha`, `tag` and `message`.
    fn problems(&self, tag: &str, sha: &str, message: &str, tagger: &str) -> Vec<String> {
        if self.ref_kind != "tag" {
            return vec![format!(
                "tag {tag} exists and is lightweight (a ref to {}): it is never replaced",
                self.object
            )];
        }
        let mut problems = Vec::new();
        if self.kind != "commit" || self.object != sha {
            problems.push(format!(
                "tag {tag} exists and names {} {}, not commit {sha}: it is never moved",
                self.kind, self.object
            ));
        }
        if self.name != tag {
            problems.push(format!("tag object is named {:?}, not {tag}", self.name));
        }
        if !same_message(&self.message, message) {
            problems.push(format!(
                "tag {tag} exists with another message than --message-file: it is never replaced"
            ));
        }
        problems.extend(self.tagger_problem(tagger));
        problems
    }

    fn tagger_problem(&self, expected: &str) -> Option<String> {
        (self.tagger != expected).then(|| {
            format!(
                "the tagger of the tag is {:?}, not {expected:?}: not made by the release bot",
                self.tagger
            )
        })
    }
}

/// Reads the tag `tag` of `repo` and requires its tagger to be `expected` (a login such as
/// [`BOT_LOGIN`]). A tag that does not exist, or is lightweight (it has no tagger), fails.
pub fn check_tagger(gh: &Path, repo: &str, tag: &str, expected: &str) -> Result<(), Error> {
    let found = held(gh, repo, tag).map_err(Step::fail)?;
    let problem = match found {
        None => Some(format!("tag {tag} does not exist")),
        Some(h) if h.ref_kind != "tag" => Some(format!("tag {tag} is lightweight: no tagger")),
        Some(h) => h.tagger_problem(expected),
    };
    problem.map_or(Ok(()), |p| Err(Error::Refused(vec![p])))
}

/// What one run makes.
struct Want<'a> {
    gh: &'a Path,
    repo: &'a str,
    sha: &'a str,
    tag: &'a str,
    message: &'a str,
    tagger: &'a str,
}

/// One try: reuse the tag when it exists as it should, else make it. Whether it was made.
fn once(w: &Want) -> Result<bool, Step> {
    if let Some(found) = held(w.gh, w.repo, w.tag)? {
        let problems = found.problems(w.tag, w.sha, w.message, w.tagger);
        if problems.is_empty() {
            return Ok(false);
        }
        return Err(Error::Refused(problems).into());
    }
    let repo = w.repo;
    let object = json!({ "tag": w.tag, "message": w.message, "object": w.sha, "type": "commit" });
    let made = call(w.gh, &format!("repos/{repo}/git/tags"), Some(&object))?;
    let oid = made.as_ref().and_then(|m| m["sha"].as_str());
    let Some(oid) = oid else {
        return Err(Error::Failed("git/tags answered with no sha".to_owned()).into());
    };
    let create = json!({ "ref": format!("refs/tags/{}", w.tag), "sha": oid });
    call(w.gh, &format!("repos/{repo}/git/refs"), Some(&create))?;
    Ok(true)
}

/// Runs [`once`], and once more after a failure worth a retry.
fn settle(w: &Want) -> Result<bool, Error> {
    match once(w) {
        Err(Step::Retry(_)) => once(w).map_err(Step::fail),
        done => done.map_err(Step::fail),
    }
}

/// Whether `repo` is `<owner>/<name>`, each part of `[A-Za-z0-9._-]` and neither `.` nor `..`.
pub(super) fn is_repo(repo: &str) -> bool {
    let part = |p: &str| {
        !matches!(p, "" | "." | "..")
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| part(owner) && part(name))
}

/// The `release tag` verb: the lines to print.
pub fn run(a: &TagArgs) -> Result<Vec<String>, Error> {
    if !is_repo(&a.repo) {
        return Err(Error::Usage(format!(
            "--repo {:?} is not <owner>/<name>",
            a.repo
        )));
    }
    full_commit("--sha", &a.sha)?;
    if Version::parse_tag(&a.tag).is_none() {
        return Err(Error::Usage(format!(
            "--tag {:?} is not a release tag (v<MAJOR>.<MINOR>.<PATCH>)",
            a.tag
        )));
    }
    let message = std::fs::read_to_string(&a.message_file)
        .map_err(Error::spawn(format!("read {}", a.message_file.display())))?;
    if message.trim().is_empty() {
        return Err(Error::Usage(format!(
            "--message-file {} is empty: a release tag carries its Beta record",
            a.message_file.display()
        )));
    }
    // Rule S, before the first write: an old run tags nothing.
    let standing = tags::superseded(&a.gh, &a.repo, &a.sha)?;
    tag_is_current(&a.tag, &standing)?;
    let want = Want {
        gh: &a.gh,
        repo: &a.repo,
        sha: &a.sha,
        tag: &a.tag,
        message: &message,
        tagger: &a.tagger,
    };
    let created = settle(&want)?;
    let lines = vec![format!("tag={}", a.tag), format!("created={created}")];
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}
