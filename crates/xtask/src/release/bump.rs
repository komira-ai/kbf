//! `release bump-internal`: the last step of a kbf release. After `publish`, it opens a pull
//! request in the internal repository that moves its submodule to the released commit, and
//! turns auto-merge on. It is a Rust port of the bump tool of mopm (`tools/bump_internal.zig`)
//! with the same steps and the same refusals.
//!
//! The internal repository admits such a pull request only through its bump guard, which
//! accepts the release bot's pull request when all of these hold: exactly one changed path,
//! the submodule's gitlink; the head branch is `bump/<submodule>-v<semver>`; the new commit
//! descends from the old one; the tag `v<semver>` of the released repository names the new
//! commit. The guard reads the branch, not the title. This verb produces exactly that, and
//! refuses what the guard would refuse before it pushes or opens anything, so a mistake is a
//! red job here and not a stuck pull request.
//!
//! The steps, each one a `gh` or `git` call without a shell:
//!
//! 1. the tag `v<version>` of `--repo` names `--sha`;
//! 2. the pull requests of the internal repository whose head branch begins
//!    `bump/<submodule>-v` (`gh pr list`, open, merged and closed): an open or merged one for
//!    this version or a newer one means there is nothing to open (an open one for this
//!    version without auto-merge gets it). A list that reaches the limit it was asked for
//!    (500) is an error, because a newer bump may be hidden behind it;
//! 3. when the branch `bump/<submodule>-v<version>` already exists (an earlier run stopped
//!    after its push), its gitlink must be `--sha` and the branch is reused. Otherwise: a
//!    shallow clone (no submodule is initialised), the gitlink set with `git update-index
//!    --cacheinfo`, a check that the staged change is that one gitlink and nothing else, the
//!    commit, and a push of the branch (never forced). A gitlink that is already `--sha` ends
//!    the run: there is nothing to open. One that `--sha` is not ahead of (GitHub's compare of
//!    the old commit and `--sha` is not `ahead`) is refused: the submodule never moves
//!    backwards;
//! 4. `gh pr create`, `gh pr merge --auto --merge`, and a check that auto-merge is on;
//! 5. every older open bump pull request is closed in favour of the new one.
//!
//! The token is `GH_TOKEN`, the release bot's installation token. `gh` reads it from its
//! environment; `git` gets it as an HTTP header through `GIT_CONFIG_*` in its environment,
//! without `GH_TOKEN`. It is never an argument of any process: [`Process`] refuses to run a
//! command whose arguments hold it, and hides it from what a command printed.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::tags::Version;
use crate::Error;
use crate::build_commit::full_commit;

/// The branch of the internal repository a bump pull request is based on.
const BASE: &str = "main";

/// How many pull requests one `gh pr list` call is asked for. A list this long may have
/// more behind it.
const LIMIT: usize = 500;

/// The fields of a pull request `gh pr list` is asked for.
const FIELDS: &str = "number,state,headRefName,url,autoMergeRequest";

/// What a version must look like, for the usage error.
const SEMVER: &str = "<major>.<minor>.<patch> (plain numbers, no leading v)";

/// The login of the release bot: the author and the committer of the bump commit.
const BOT_NAME: &str = "komira-release-bot[bot]";

/// The noreply address GitHub gives a bot user, `<user id>+<login>@users.noreply.github.com`.
/// The id is the id of the bot USER, not of the App.
const BOT_EMAIL: &str = "345322945+komira-release-bot[bot]@users.noreply.github.com";

/// What `git` runs under besides the token: no prompt, https only, the bot as author and
/// committer.
const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ALLOW_PROTOCOL", "https"),
    ("GIT_AUTHOR_NAME", BOT_NAME),
    ("GIT_AUTHOR_EMAIL", BOT_EMAIL),
    ("GIT_COMMITTER_NAME", BOT_NAME),
    ("GIT_COMMITTER_EMAIL", BOT_EMAIL),
];

/// The arguments of `release bump-internal`.
#[derive(clap::Args)]
pub struct BumpArgs {
    /// The released repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`): its tag
    /// `v<version>` must name `--sha`.
    #[arg(long, default_value = "komira-ai/kbf")]
    pub repo: String,
    /// The release's version, `<major>.<minor>.<patch>`: plain numbers, no `v` (the
    /// `version` output of `release plan`).
    #[arg(long)]
    pub version: String,
    /// The released commit, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub sha: String,
    /// The internal repository, `<owner>/<repo>`.
    #[arg(long)]
    pub internal_repo: String,
    /// The submodule: its path in the internal repository, and its name in the branch.
    #[arg(long)]
    pub submodule: String,
    /// Where the internal repository is cloned: a directory that is missing or empty
    /// (default: one of this run's alone, removed after it).
    #[arg(long)]
    pub workdir: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
    /// The `git` program.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
}

/// A `release bump-internal` request whose values are well formed.
pub struct Bump {
    /// The released repository, `<owner>/<repo>`.
    pub repo: String,
    /// The internal repository, `<owner>/<repo>`.
    pub internal_repo: String,
    /// The submodule's path in the internal repository.
    pub submodule: String,
    /// The release's version.
    pub version: Version,
    /// The released commit, a full SHA.
    pub sha: String,
    /// Where the internal repository is cloned.
    pub workdir: PathBuf,
}

impl Bump {
    /// The request `a` asks for, or the usage error naming the flag that is wrong.
    pub fn from_args(a: &BumpArgs) -> Result<Bump, Error> {
        let Some(version) = Version::parse_tag(&format!("v{}", a.version)) else {
            return Err(usage("--version", &a.version, SEMVER));
        };
        full_commit("--sha", &a.sha)?;
        owner_name("--repo", &a.repo)?;
        owner_name("--internal-repo", &a.internal_repo)?;
        let sub = &a.submodule;
        if !is_name(sub) {
            return Err(usage("--submodule", sub, "a submodule name"));
        }
        let workdir = a.workdir.clone().unwrap_or_else(default_workdir);
        Ok(Bump {
            repo: a.repo.clone(),
            internal_repo: a.internal_repo.clone(),
            submodule: a.submodule.clone(),
            version,
            sha: a.sha.clone(),
            workdir,
        })
    }

    /// The tag of the release, `v<version>`.
    pub fn tag(&self) -> String {
        self.version.tag()
    }

    /// The head branch of the pull request: the bump guard's `bump/<submodule>-v<semver>`.
    pub fn branch(&self) -> String {
        format!("bump/{}-v{}", self.submodule, self.version)
    }

    /// The title of the pull request and of its commit.
    pub fn title(&self) -> String {
        format!("chore({}): bump to v{}", self.submodule, self.version)
    }

    /// The body of the pull request, and the rest of the message of its commit.
    pub fn body(&self) -> String {
        let (repo, sub, sha) = (&self.repo, &self.submodule, &self.sha);
        let version = self.version;
        format!(
            "Moves the `{sub}` submodule to the commit of the release v{version} ({sha}).\n\n\
             Release: https://github.com/{repo}/releases/tag/v{version}\n\n\
             Opened by the release pipeline of {repo} after it published the release. \
             Auto-merge fires when the required checks pass, among them the bump guard."
        )
    }
}

/// The usage error for a flag whose value is not what it must be.
fn usage(flag: &str, value: &str, want: &str) -> Error {
    Error::Usage(format!("{flag} {value:?} is not {want}"))
}

/// The directory a run that names none clones into.
fn default_workdir() -> PathBuf {
    std::env::temp_dir().join(format!("xtask-bump-{}", std::process::id()))
}

/// A repository or submodule name: letters, digits, `.`, `_` and `-`, not starting with a dot.
fn is_name(value: &str) -> bool {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    !value.is_empty() && !value.starts_with('.') && value.bytes().all(plain)
}

/// `value` is `<owner>/<repo>`: two names joined by one slash.
fn owner_name(flag: &str, value: &str) -> Result<(), Error> {
    let both = value
        .split_once('/')
        .is_some_and(|(owner, repo)| is_name(owner) && is_name(repo));
    if both {
        return Ok(());
    }
    Err(usage(flag, value, "<owner>/<repo>"))
}

/// What a program printed, and whether it succeeded.
pub struct Ran {
    /// The exit status was 0.
    pub ok: bool,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// `gh` and `git`, behind a seam so tests run against a recorded world.
pub trait Tools {
    /// Runs `gh <args>`.
    fn gh(&self, args: &[&str]) -> Result<Ran, Error>;
    /// Runs `git <args>`.
    fn git(&self, args: &[&str]) -> Result<Ran, Error>;
}

/// `gh` and `git` as programs. The token is `gh`'s `GH_TOKEN`; `git` gets it as an HTTP
/// header through `GIT_CONFIG_*` in its environment, without `GH_TOKEN`. It is never an
/// argument: a call whose arguments hold it is refused, and what a program printed is
/// returned with it hidden.
pub struct Process {
    gh_program: PathBuf,
    git_program: PathBuf,
    token: String,
}

impl Process {
    /// `gh` and `git` as these programs, with the release bot's installation `token`.
    pub fn new(gh: PathBuf, git: PathBuf, token: String) -> Process {
        Process {
            gh_program: gh,
            git_program: git,
            token,
        }
    }

    /// The command `gh <args>`: the token in its environment.
    pub fn gh_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.gh_program);
        cmd.args(args).env("GH_TOKEN", &self.token);
        cmd
    }

    /// The command `git <args>`: no `GH_TOKEN` or `GITHUB_TOKEN`; the token as the HTTP
    /// header `git` sends to github.com; no prompt; https only; the bot as author and
    /// committer.
    pub fn git_command(&self, args: &[&str]) -> Command {
        let credential = format!("x-access-token:{}", self.token);
        let header = format!("AUTHORIZATION: basic {}", STANDARD.encode(credential));
        let mut cmd = Command::new(&self.git_program);
        cmd.args(args);
        cmd.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
        cmd.env("GIT_CONFIG_COUNT", "1");
        cmd.env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader");
        cmd.env("GIT_CONFIG_VALUE_0", header);
        cmd.envs(GIT_ENV);
        cmd
    }

    /// Runs `cmd`, which is `program` with `args`; refuses first when an argument holds the
    /// token.
    fn run(&self, program: &Path, mut cmd: Command, args: &[&str]) -> Result<Ran, Error> {
        let name = program.display().to_string();
        let token = self.token.as_str();
        if args.iter().any(|a| a.contains(token)) {
            let why = format!("refusing to run {name}: the token is in its arguments");
            return Err(Error::Failed(why));
        }
        let out = cmd.output().map_err(Error::spawn(name))?;
        let scrub = |b: &[u8]| String::from_utf8_lossy(b).replace(token, "***");
        Ok(Ran {
            ok: out.status.success(),
            stdout: scrub(&out.stdout),
            stderr: scrub(&out.stderr),
        })
    }
}

impl Tools for Process {
    fn gh(&self, args: &[&str]) -> Result<Ran, Error> {
        self.run(&self.gh_program, self.gh_command(args), args)
    }

    fn git(&self, args: &[&str]) -> Result<Ran, Error> {
        self.run(&self.git_program, self.git_command(args), args)
    }
}

/// Runs `gh <args>` (at least two): its standard output, or the error holding its stderr.
fn gh_must(tools: &dyn Tools, args: &[&str]) -> Result<String, Error> {
    let ran = tools.gh(args)?;
    if ran.ok {
        return Ok(ran.stdout);
    }
    let what = args[..2].join(" ");
    let why = ran.stderr.trim_end();
    Err(Error::Failed(format!("gh {what}: {why}")))
}

/// Runs `git <args>`: its standard output, or the error holding its stderr.
fn git_must(tools: &dyn Tools, args: &[&str]) -> Result<String, Error> {
    let ran = tools.git(args)?;
    if ran.ok {
        return Ok(ran.stdout);
    }
    let what = args.join(" ");
    let why = ran.stderr.trim_end();
    Err(Error::Failed(format!("git {what}: {why}")))
}

/// Runs `git -C <dir> <args>`.
fn git_in(tools: &dyn Tools, dir: &str, args: &[&str]) -> Result<String, Error> {
    let mut all = vec!["-C", dir];
    all.extend_from_slice(args);
    git_must(tools, &all)
}

/// Whether `gh api` failed because nothing is there: a 404, or the 422 the commits API
/// answers for a ref that names no commit (a tag that does not exist).
fn is_missing(stderr: &str) -> bool {
    stderr.contains("(HTTP 404)") || stderr.contains("No commit found")
}

/// `gh api <path>`: the JSON it printed, or `None` when nothing is there.
fn api(tools: &dyn Tools, path: &str) -> Result<Option<Value>, Error> {
    let ran = tools.gh(&["api", path])?;
    if !ran.ok {
        if is_missing(&ran.stderr) {
            return Ok(None);
        }
        let why = ran.stderr.trim_end();
        return Err(Error::Failed(format!("gh api {path}: {why}")));
    }
    let json = serde_json::from_str(&ran.stdout);
    json.map(Some)
        .map_err(|e| Error::Failed(format!("gh api {path}: not JSON: {e}")))
}

/// A bump pull request of the internal repository.
struct Pr {
    number: u64,
    url: String,
    version: Version,
    open: bool,
    auto_merge: bool,
}

/// The version a bump branch `<prefix><version>` names, or none for any other branch.
fn branch_version(prefix: &str, head: &str) -> Option<Version> {
    let version = head.strip_prefix(prefix)?;
    Version::parse_tag(&format!("v{version}"))
}

/// The open and merged bump pull requests of the submodule: the pull requests whose head
/// branch begins `bump/<submodule>-v` (the search), at most [`LIMIT`] of them.
fn pull_requests(tools: &dyn Tools, b: &Bump) -> Result<Vec<Pr>, Error> {
    let repo = b.internal_repo.as_str();
    let prefix = format!("bump/{}-v", b.submodule);
    let search = format!("head:{prefix}");
    let limit = LIMIT.to_string();
    let mut args = vec!["pr", "list", "-R", repo, "--state", "all"];
    args.extend(["--search", &search, "--limit", &limit]);
    args.extend(["--json", FIELDS]);
    let printed = gh_must(tools, &args)?;
    let Ok(Value::Array(listed)) = serde_json::from_str::<Value>(&printed) else {
        let why = format!("gh pr list printed no JSON list: {printed:?}");
        return Err(Error::Failed(why));
    };
    if listed.len() >= LIMIT {
        let why = format!(
            "gh pr list returned {LIMIT} pull requests, the most it is asked for: more may be \
             hidden, and a hidden newer bump would not be seen"
        );
        return Err(Error::Failed(why));
    }
    let mut found = Vec::new();
    for entry in &listed {
        let head = entry["headRefName"].as_str().unwrap_or_default();
        let Some(version) = branch_version(&prefix, head) else {
            continue;
        };
        let open = match entry["state"].as_str() {
            Some("OPEN") => true,
            Some("MERGED") => false,
            _ => continue,
        };
        let (number, url) = (entry["number"].as_u64(), entry["url"].as_str());
        let (Some(number), Some(url)) = (number, url) else {
            let why = format!("gh pr list printed an entry without number or url: {entry}");
            return Err(Error::Failed(why));
        };
        found.push(Pr {
            number,
            url: url.to_owned(),
            version,
            open,
            auto_merge: !entry["autoMergeRequest"].is_null(),
        });
    }
    Ok(found)
}

/// The bump guard's last rule, checked first: the tag `v<version>` of the released
/// repository names the commit.
fn tag_names_sha(tools: &dyn Tools, b: &Bump) -> Result<(), Error> {
    let (repo, sha, tag) = (b.repo.as_str(), b.sha.as_str(), b.tag());
    let found = api(tools, &format!("repos/{repo}/commits/{tag}"))?;
    let Some(found) = found else {
        let why = format!("the tag {tag} does not exist in {repo}");
        return Err(Error::Refused(vec![why]));
    };
    let named = found["sha"].as_str().unwrap_or_default();
    if named != sha {
        let why = format!("the tag {tag} of {repo} names {named:?}, not {sha}");
        return Err(Error::Refused(vec![why]));
    }
    Ok(())
}

/// Turns auto-merge on for the pull request `pr` (a number or a url), and checks that it
/// is on.
fn enable_auto_merge(tools: &dyn Tools, b: &Bump, pr: &str) -> Result<(), Error> {
    let repo = b.internal_repo.as_str();
    let mut merge = vec!["pr", "merge", pr, "-R", repo];
    merge.extend(["--auto", "--merge"]);
    gh_must(tools, &merge)?;
    let mut view = vec!["pr", "view", pr, "-R", repo];
    view.extend(["--json", "autoMergeRequest"]);
    let printed = gh_must(tools, &view)?;
    let viewed: Result<Value, _> = serde_json::from_str(&printed);
    if viewed.is_ok_and(|v| v["autoMergeRequest"].is_object()) {
        return Ok(());
    }
    let why = format!("auto-merge is not on for {pr} after `gh pr merge --auto --merge`");
    Err(Error::Failed(why))
}

/// A bump for this version or a newer one exists: there is nothing to open, except the
/// auto-merge of an open pull request for this very version that lacks it.
fn cover(tools: &dyn Tools, b: &Bump, p: &Pr) -> Result<String, Error> {
    let (tag, url) = (b.tag(), &p.url);
    if p.version == b.version && p.open && !p.auto_merge {
        enable_auto_merge(tools, b, &p.number.to_string())?;
        let line = format!("{url} for {tag} was open without auto-merge: turned it on");
        return Ok(line);
    }
    let state = if p.open { "open" } else { "merged" };
    let line = format!(
        "a bump pull request for {tag} or newer exists ({url}, {state}): nothing to open"
    );
    Ok(line)
}

/// The commit of the submodule's gitlink on the bump branch of the internal repository, or
/// none when the branch does not exist.
fn branch_gitlink(tools: &dyn Tools, b: &Bump) -> Result<Option<String>, Error> {
    let (repo, sub) = (b.internal_repo.as_str(), b.submodule.as_str());
    let branch = b.branch();
    let ref_path = format!("repos/{repo}/git/ref/heads/{branch}");
    if api(tools, &ref_path)?.is_none() {
        return Ok(None);
    }
    let path = format!("repos/{repo}/contents/{sub}?ref={branch}");
    let held = api(tools, &path)?;
    let sha = held
        .as_ref()
        .filter(|v| v["type"] == "submodule")
        .and_then(|v| v["sha"].as_str());
    if let Some(sha) = sha {
        return Ok(Some(sha.to_owned()));
    }
    let why = format!(
        "the branch {branch} of {repo} holds no gitlink at {sub}: delete the branch by hand"
    );
    Err(Error::Refused(vec![why]))
}

/// The commit of the gitlink at `path` at HEAD of the clone in `dir`: mode 160000, type
/// commit. Anything else at that path, a file, a directory or nothing, is refused.
pub fn gitlink_at(tools: &dyn Tools, dir: &str, path: &str) -> Result<String, Error> {
    let printed = git_in(tools, dir, &["ls-tree", "HEAD", "--", path])?;
    // `<mode> SP <type> SP <sha> TAB <path>`
    let meta = printed.split('\t').next().unwrap_or_default();
    let fields: Vec<&str> = meta.split(' ').collect();
    match fields.as_slice() {
        ["160000", "commit", sha] => Ok((*sha).to_owned()),
        _ => {
            let shown = printed.trim_end();
            let why = format!("{path} is not a gitlink at HEAD (git ls-tree says {shown:?})");
            Err(Error::Refused(vec![why]))
        }
    }
}

/// Whether `meta` is the `--raw` line of a gitlink that stays a gitlink and was modified.
fn is_gitlink_edit(meta: &str) -> bool {
    meta.starts_with(":160000 160000 ") && meta.ends_with(" M")
}

/// Refuses unless the staged change in the clone in `dir` is exactly one path, `path`, a
/// gitlink before and after, modified.
pub fn staged_one_gitlink(tools: &dyn Tools, dir: &str, path: &str) -> Result<(), Error> {
    let mut args = vec!["diff", "--cached", "--raw", "-z", "--no-renames"];
    args.extend(["--ignore-submodules=none", "HEAD"]);
    let raw = git_in(tools, dir, &args)?;
    let fields: Vec<&str> = raw.split_terminator('\0').collect();
    let one = match fields.as_slice() {
        [meta, found] => *found == path && is_gitlink_edit(meta),
        _ => false,
    };
    if one {
        return Ok(());
    }
    let shown = raw.replace('\0', " ");
    let why = format!(
        "the staged change is not exactly one modified gitlink at {path} (git diff says \
         {shown:?}): nothing is committed"
    );
    Err(Error::Refused(vec![why]))
}

/// Clones the internal repository, sets the gitlink, commits and pushes the branch. False
/// when the gitlink is already the commit: there is nothing to push.
fn push_bump(tools: &dyn Tools, b: &Bump) -> Result<bool, Error> {
    let (repo, sha, sub) = (b.repo.as_str(), b.sha.as_str(), b.submodule.as_str());
    let dir = b.workdir.display().to_string();
    let url = format!("https://github.com/{}.git", b.internal_repo);
    // No submodule is initialised: a clone without `--recurse-submodules` leaves the gitlink.
    let mut clone = vec!["clone", "--quiet", "--depth", "1", "--no-tags"];
    clone.extend(["--single-branch", "--branch", BASE, &url, &dir]);
    git_must(tools, &clone)?;
    let old = gitlink_at(tools, &dir, sub)?;
    if old == sha {
        return Ok(false);
    }
    // Forward only, as the guard demands: `sha` descends from the old commit.
    let compare = format!("repos/{repo}/compare/{old}...{sha}");
    let compared = api(tools, &compare)?;
    let status = compared.as_ref().and_then(|v| v["status"].as_str());
    if status != Some("ahead") {
        let said = status.unwrap_or("nothing");
        let why = format!(
            "{sha} is not ahead of {old} in {repo} (compare says {said}): the submodule \
             never moves backwards"
        );
        return Err(Error::Refused(vec![why]));
    }
    let cacheinfo = format!("160000,{sha},{sub}");
    git_in(tools, &dir, &["update-index", "--cacheinfo", &cacheinfo])?;
    staged_one_gitlink(tools, &dir, sub)?;
    let (title, body) = (b.title(), b.body());
    let mut commit = vec!["-c", "commit.gpgsign=false", "commit", "--quiet"];
    commit.extend(["-m", &title, "-m", &body]);
    git_in(tools, &dir, &commit)?;
    let committed = gitlink_at(tools, &dir, sub)?;
    if committed != sha {
        let why = format!("after the commit the gitlink of {sub} is {committed}, not {sha}");
        return Err(Error::Failed(why));
    }
    let refspec = format!("HEAD:refs/heads/{}", b.branch());
    git_in(tools, &dir, &["push", "--quiet", "origin", &refspec])?;
    Ok(true)
}

/// Opens the pull request of the bump branch: its url.
fn create_pr(tools: &dyn Tools, b: &Bump) -> Result<String, Error> {
    let repo = b.internal_repo.as_str();
    let (branch, title, body) = (b.branch(), b.title(), b.body());
    let mut args = vec!["pr", "create", "-R", repo, "--base", BASE];
    args.extend(["--head", &branch, "--title", &title, "--body", &body]);
    let printed = gh_must(tools, &args)?;
    // The url is the last line of what `gh pr create` prints.
    let url = printed.lines().next_back().unwrap_or_default().trim();
    if !url.starts_with("https://") {
        let why = format!("gh pr create printed {printed:?}, not a pull request url");
        return Err(Error::Failed(why));
    }
    Ok(url.to_owned())
}

/// No bump pull request covers this version: reuse the branch an earlier run pushed (it must
/// hold this commit), or push one, then open the pull request and turn auto-merge on.
fn open(tools: &dyn Tools, b: &Bump) -> Result<String, Error> {
    let (sub, sha, tag) = (b.submodule.as_str(), b.sha.as_str(), b.tag());
    if let Some(held) = branch_gitlink(tools, b)? {
        if held != sha {
            let (branch, repo) = (b.branch(), b.internal_repo.as_str());
            let why = format!(
                "the branch {branch} of {repo} holds {sub} at {held}, not {sha}: delete the \
                 branch by hand"
            );
            return Err(Error::Refused(vec![why]));
        }
    } else if !push_bump(tools, b)? {
        let line = format!("{sub} is already at {sha}: nothing to open");
        return Ok(line);
    }
    let url = create_pr(tools, b)?;
    enable_auto_merge(tools, b, &url)?;
    let line = format!("opened {url} with auto-merge: {sub} to {tag}");
    Ok(line)
}

/// Closes the older open bump pull request `old` in favour of this one, and its branch.
fn close_older(tools: &dyn Tools, b: &Bump, old: &Pr) -> Result<(), Error> {
    let repo = b.internal_repo.as_str();
    let (number, version) = (old.number.to_string(), b.version);
    let note = format!("Superseded by the bump to v{version}.");
    let mut args = vec!["pr", "close", number.as_str(), "-R", repo];
    args.extend(["--comment", &note, "--delete-branch"]);
    gh_must(tools, &args)?;
    Ok(())
}

/// The bump itself: the lines to print. Every refusal comes before the first write.
pub fn apply(tools: &dyn Tools, b: &Bump) -> Result<Vec<String>, Error> {
    let (repo, sha, tag) = (b.repo.as_str(), b.sha.as_str(), b.tag());
    tag_names_sha(tools, b)?;
    let mut said = vec![format!("tag {tag} of {repo} names {sha}")];
    let listed = pull_requests(tools, b)?;
    let mine = b.version;
    let covered = listed
        .iter()
        .filter(|p| p.version >= mine)
        .max_by_key(|p| (p.version, p.open));
    let line = match covered {
        Some(p) => cover(tools, b, p)?,
        None => open(tools, b)?,
    };
    said.push(line);
    for old in listed.iter().filter(|p| p.open && p.version < mine) {
        close_older(tools, b, old)?;
        said.push(format!("closed {} (older than {tag})", old.url));
    }
    Ok(said)
}

/// The `release bump-internal` verb: the lines to print. The token is `GH_TOKEN`.
pub fn run(a: &BumpArgs) -> Result<Vec<String>, Error> {
    let bump = Bump::from_args(a)?;
    let token = std::env::var("GH_TOKEN").unwrap_or_default();
    if token.is_empty() {
        let why = "GH_TOKEN is not set (the installation token of the release bot)";
        return Err(Error::Failed(why.to_owned()));
    }
    let tools = Process::new(a.gh.clone(), a.git.clone(), token);
    let done = apply(&tools, &bump);
    if a.workdir.is_none() {
        // The clone this run made for itself is of no use after it, whatever its result.
        let _ = std::fs::remove_dir_all(&bump.workdir);
    }
    done
}
