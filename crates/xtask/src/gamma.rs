//! `cargo xtask gamma`: the steps of `.github/workflows/gamma.yml`, which follows a cut
//! release to the one gamma host, our own build farm's frontend (docs/design/kbf-cicd.md,
//! section 5.4; the host side is docs/design/gamma-deploy.md).
//!
//! - `gamma evidence --repo <owner/repo> --sha <sha> --run-id <id> --output <file> [--gh
//!   <program>]` ([`evidence`]): the run `<id>` of the `release` workflow is a push to `main`
//!   that succeeded, at exactly `<sha>`, on the right file, and its `publish` job succeeded.
//!   Prints `released=true`, or `released=false` when the job was skipped or did not succeed
//!   (a push that cut nothing, such as one that changed only docs), and appends it to
//!   `--output`. A fact of the run that contradicts the event is an error, not a skip; so is
//!   a job list that is missing, not whole (its `total_count` is not the number listed) or
//!   has no job named `publish`: those would end the pipeline green with nothing deployed.
//! - `gamma deploy --repo <owner/repo> --sha <sha> --host <h> --user <u> --bundle <tar> --raw
//!   <file> --summary <file> [--tailscale <program>] [--gh <program>]` ([`deploy`]): masks the
//!   host and user in the log, refuses unless `<sha>` is the commit of the highest release tag
//!   (Rule S, [`tags::superseded`], before anything reaches the farm), then runs
//!   [`COMMAND`] on the host through `tailscale ssh` and `sudo` with `<tar>` on its standard
//!   input. The installer's one-line summary goes to `--raw`, which names nodes, and
//!   [`redact`] writes what a public log may show to `--summary`. What `tailscale ssh` writes
//!   to its standard error (`sudo`'s and the installer's messages name the host and its
//!   paths) is dropped and only its size is printed. Fails when the installer's status is
//!   not 0; no message names the host.
//! - `gamma summary --file <summary.json> --output <file>` ([`summary`]): a table of the
//!   redacted summary, appended to `<file>` (on Actions, `GITHUB_STEP_SUMMARY`). A missing or
//!   unreadable summary is said so; it is not an error, so it never hides the deploy's own
//!   failure.
//! - `gamma sudoers-line --user <account>` ([`sudoers_line`]): the line of the host's sudoers
//!   file that lets the account run [`COMMAND`] as root and nothing else.
//! - `gamma check-build --komira-dir <dir> --targets <targets> --buckconfig-file <path> --log
//!   <file> --summary <file>` ([`check_build`]): after the deploy, builds komira's main
//!   (`<dir>`) against the farm just updated, with the farm's normal caching, using the config
//!   in the environment variable [`BUCKCONFIG_ENV`] (a secret: it names the farm). Passes only
//!   for `BUILD SUCCEEDED`, `local: 0` in buck2's last `Commands:` line and exit status 0;
//!   `--no-remote-cache` is refused wherever it could come from. buck2's output goes to
//!   `--log` and is never printed; the `Commands:` line and the verdict are appended to
//!   `--summary` (on Actions, `GITHUB_STEP_SUMMARY`).
//! - `gamma held-steps --repo <owner/repo> --host <h> --user <u> [--tag <tag>]`
//!   ([`held_steps`]): the settings a person makes once (the environment, its variables and
//!   secrets, the Tailscale policy, the host). It prints them and does nothing: it has no
//!   `--apply`.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::Error;
use crate::build_commit::full_commit;
use crate::release::{self, tags};

mod check;
pub use check::{BUCKCONFIG_ENV, CheckBuild, check_build};

/// The workflow whose push run on `main` this follows: `release.yml`, named `release`.
pub const RELEASE_WORKFLOW: &str = "release";

/// Its file, as a run reports it in `path`.
pub const RELEASE_PATH: &str = ".github/workflows/release.yml";

/// The job of that workflow that publishes: a run released a commit only if it succeeded.
pub const PUBLISH_JOB: &str = "publish";

/// The installer on the frontend host: the path the Linux package installs it at, the one
/// program the deploy account may run as root.
pub const INSTALLER: &str = crate::pkg_linux::BINARY_PATH;

/// Where the commit goes in [`COMMAND`].
const COMMIT_SLOT: &str = "<commit>";

/// The command the deploy account runs as root, word by word: the one the workflow sends and
/// the one the sudoers line allows. Both are made from this array, so they cannot differ.
/// `--offer -` and `--bundle -` read the offer and the files from the tar on standard input;
/// no flag that names a path of the host is in it.
pub const COMMAND: [&str; 10] = [
    INSTALLER,
    "deploy",
    "--apply",
    "--json",
    "--commit",
    COMMIT_SLOT,
    "--offer",
    "-",
    "--bundle",
    "-",
];

/// The arguments of `tailscale` for the deploy: `ssh <user>@<host> sudo -n` and [`COMMAND`]
/// with `sha` in the commit's place.
pub fn ssh_args(sha: &str, host: &str, user: &str) -> Vec<String> {
    let mut args = vec![
        "ssh".to_owned(),
        format!("{user}@{host}"),
        "sudo".to_owned(),
        "-n".to_owned(),
    ];
    args.extend(COMMAND.iter().map(|arg| {
        let arg = if *arg == COMMIT_SLOT { sha } else { *arg };
        arg.to_owned()
    }));
    args
}

/// [`COMMAND`] as sudoers matches it: the commit is forty `[0-9a-f]` classes, one lowercase
/// hex digit each, and nothing is a wildcard.
pub fn sudoers_command() -> String {
    COMMAND
        .map(|arg| {
            if arg == COMMIT_SLOT {
                "[0-9a-f]".repeat(40)
            } else {
                arg.to_owned()
            }
        })
        .join(" ")
}

/// The sudoers rule for `user` (already known to be a valid account).
fn rule(user: &str) -> String {
    format!("{user} ALL=(root) NOPASSWD: {}", sudoers_command())
}

/// The line of the host's sudoers file for the deploy account `user`.
pub fn sudoers_line(user: &str) -> Result<String, Error> {
    account(user)?;
    Ok(rule(user))
}

/// A deploy account: a lowercase account name, and never `root` (a caller that is root did
/// not come through `sudo`, so the installer would not narrow its flags). The message does
/// not repeat the name.
fn account(user: &str) -> Result<(), Error> {
    let mut chars = user.chars();
    let valid = chars.next().is_some_and(|c| matches!(c, 'a'..='z' | '_'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'));
    if !valid || user == "root" {
        let why = "--user is not a deploy account ([a-z_][a-z0-9_-]*, not root)";
        return Err(Error::Usage(why.to_owned()));
    }
    Ok(())
}

/// A tailnet host name: letters, digits, `.`, `_` and `-`, and not an address. The message
/// does not repeat the name.
fn host_name(host: &str) -> Result<(), Error> {
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    let name = !host.is_empty() && host.bytes().all(plain);
    let address = host.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if name && !address {
        return Ok(());
    }
    Err(Error::Usage("--host is not a tailnet name".to_owned()))
}

/// `<owner>/<name>`, each of letters, digits, `.`, `_` and `-`.
fn repo_name(repo: &str) -> Result<(), Error> {
    let part = |p: &str| {
        let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        !p.is_empty() && p.bytes().all(plain)
    };
    match repo.split_once('/') {
        Some((owner, name)) if part(owner) && part(name) => Ok(()),
        _ => Err(Error::Usage(format!(
            "--repo {repo:?} is not <owner>/<name>"
        ))),
    }
}

/// A Tailscale tag: `tag:` and lowercase letters, digits and `-`.
fn tag_name(tag: &str) -> Result<(), Error> {
    let plain = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-');
    let name = tag.strip_prefix("tag:").unwrap_or_default();
    if !name.is_empty() && name.bytes().all(plain) {
        return Ok(());
    }
    Err(Error::Usage(format!("--tag {tag:?} is not tag:<name>")))
}

/// Appends `text` to the file `path`, making it when it is not there.
fn append(path: &Path, text: &str) -> Result<(), Error> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(Error::spawn(format!("append to {}", path.display())))
}

/// The string under `key` of a JSON object; empty when there is none.
fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

/// What the run says that the event did not: one message for each fact that differs. The
/// path may carry the ref it ran at (`<path>@<ref>`); the ref is the branch fact below.
fn contradictions(run: &Value, sha: &str) -> Vec<String> {
    let file = field(run, "path").split('@').next().unwrap_or_default();
    let facts = [
        ("path", file, RELEASE_PATH),
        ("name", field(run, "name"), RELEASE_WORKFLOW),
        ("event", field(run, "event"), "push"),
        ("head_branch", field(run, "head_branch"), release::MAIN),
        ("head_sha", field(run, "head_sha"), sha),
        ("status", field(run, "status"), "completed"),
        ("conclusion", field(run, "conclusion"), "success"),
    ];
    facts
        .iter()
        .filter(|(_, got, want)| got != want)
        .map(|(what, got, want)| format!("the run's {what} is {got:?}, not {want:?}"))
        .collect()
}

/// The `gamma evidence` verb: whether run `run_id` of the release workflow released `sha`.
pub fn evidence(
    gh: &Path,
    repo: &str,
    sha: &str,
    run_id: u64,
    output: &Path,
) -> Result<Vec<String>, Error> {
    full_commit("--sha", sha)?;
    repo_name(repo)?;
    let path = format!("repos/{repo}/actions/runs/{run_id}");
    let Some(run) = release::api(gh, &path)? else {
        return Err(Error::Failed(format!(
            "run {run_id} of {repo} does not exist"
        )));
    };
    let wrong = contradictions(&run, sha);
    if !wrong.is_empty() {
        return Err(Error::Refused(wrong));
    }
    // A run that succeeded lists its `publish` job, skipped when the push cut nothing. A job
    // that is not there (renamed, or past the first page), a listing that is not whole or no
    // listing is an error and not a skip: a skip is a green run that deploys nothing.
    let Some(listing) = release::api(gh, &format!("{path}/jobs?per_page=100"))? else {
        return Err(Error::Failed(format!("the jobs of run {run_id} are gone")));
    };
    let array = listing["jobs"].as_array();
    let jobs = array.map(Vec::as_slice).unwrap_or_default();
    if listing["total_count"] != jobs.len() {
        let why = format!("run {run_id}: the list of its jobs is not whole");
        return Err(Error::Failed(why));
    }
    let Some(job) = jobs.iter().find(|j| j["name"] == PUBLISH_JOB) else {
        let why = format!("run {run_id} has no job named {PUBLISH_JOB}");
        return Err(Error::Failed(why));
    };
    let released = job["conclusion"] == "success";
    let line = format!("released={released}");
    append(output, &format!("{line}\n"))?;
    Ok(vec![line])
}

/// Runs `program args` with the file `tar` as its standard input and its standard output in
/// `raw`. Returns its exit status (-1 when a signal ended it) and how many bytes it wrote to
/// its standard error, which is dropped: the remote side's messages (`sudo`'s, the
/// installer's usage errors) name the host and its paths, and the log is public.
fn pipe(program: &Path, args: &[String], tar: &Path, raw: &Path) -> Result<(i32, usize), Error> {
    let what = format!("open {}", tar.display());
    let input = fs::File::open(tar).map_err(Error::spawn(what))?;
    let what = format!("create {}", raw.display());
    let output = fs::File::create(raw).map_err(Error::spawn(what))?;
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(output))
        .output()
        .map_err(Error::spawn("tailscale".to_owned()))?;
    Ok((out.status.code().unwrap_or(-1), out.stderr.len()))
}

/// What `gamma deploy` is given.
pub struct Deploy<'a> {
    /// The `gh` program, through which Rule S reads the release tags.
    pub gh: &'a Path,
    /// The `tailscale` program.
    pub tailscale: &'a Path,
    /// The repository, `<owner>/<name>`.
    pub repo: &'a str,
    /// The commit that was released.
    pub sha: &'a str,
    /// The frontend host's tailnet name.
    pub host: &'a str,
    /// The deploy account on it.
    pub user: &'a str,
    /// The tar `release stage-offer` wrote: `offer.json` and the release's files.
    pub bundle: &'a Path,
    /// Where the installer's own summary goes. It names nodes: it is never uploaded.
    pub raw: &'a Path,
    /// Where the redacted summary goes.
    pub summary: &'a Path,
}

/// The `gamma deploy` verb: the lines to print when the installer exited 0.
pub fn deploy(d: &Deploy<'_>) -> Result<Vec<String>, Error> {
    full_commit("--sha", d.sha)?;
    repo_name(d.repo)?;
    host_name(d.host)?;
    account(d.user)?;
    // Workflow commands: from here on the log shows neither name. It cannot undo what GitHub
    // and cargo printed of this step before (its environment, the command line), and the log
    // is public.
    println!("::add-mask::{}", d.host);
    println!("::add-mask::{}", d.user);
    // Rule S for the one write to the farm: only the commit of the highest release tag.
    let standing = tags::superseded(d.gh, d.repo, d.sha)?;
    if !matches!(standing, tags::Standing::Is(_)) {
        let why = format!("{} is not the highest release ({standing:?})", d.sha);
        return Err(Error::Refused(vec![format!("not deployed: {why}")]));
    }
    let (status, unseen) = pipe(
        d.tailscale,
        &ssh_args(d.sha, d.host, d.user),
        d.bundle,
        d.raw,
    )?;
    if unseen > 0 {
        eprintln!("tailscale: {unseen} bytes of standard error are not shown");
    }
    // The installer's summary names nodes; only what `redact` keeps leaves this step.
    let raw = fs::read(d.raw).unwrap_or_default();
    let redacted = redact(&String::from_utf8_lossy(&raw));
    let what = format!("write {}", d.summary.display());
    fs::write(d.summary, redacted).map_err(Error::spawn(what))?;
    if status != 0 {
        let why = format!("kbf-installer deploy exited {status}");
        return Err(Error::Failed(why));
    }
    Ok(vec![format!("{}: kbf-installer deploy exited 0", d.sha)])
}

/// A value of the installer's summary that is a short word (lowercase letters, digits, `_`
/// and `-`): a result, a reason code, a step name, a state. Anything else is dropped.
fn word(v: &Value) -> Value {
    let Some(s) = v.as_str() else {
        return Value::Null;
    };
    let plain = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-');
    if s.is_empty() || s.len() > 64 || !s.bytes().all(plain) {
        return Value::Null;
    }
    Value::String(s.to_owned())
}

/// The installer's summary reduced to what a public log may show, written out from an
/// allow-list and not by removing what is known to be sensitive: the commit, the result, the
/// exit status and the reason code, the names of the steps and whether each passed, the
/// platform, state and seconds of each node, the post checks, and the offer's numbers. A
/// node's id (which may be a host name), `detail`, a node's `refusal`, a step's `reason` and
/// any field this function does not know are not in it. Text that is not a JSON document
/// gives the empty string.
pub fn redact(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return String::new();
    };
    let steps: Vec<Value> = v["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| json!({ "name": word(&s["name"]), "ok": s["ok"].as_bool() }))
        .collect();
    let nodes: Vec<Value> = v["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|n| {
            json!({
                "platform": word(&n["platform"]),
                "state": word(&n["state"]),
                "secs": n["secs"].as_u64(),
            })
        })
        .collect();
    let (post, offer) = (&v["post"], &v["offer"]);
    json!({
        "commit": word(&v["commit"]),
        "result": word(&v["result"]),
        "exit": v["exit"].as_i64(),
        "reason": word(&v["reason"]),
        "steps": steps,
        "nodes": nodes,
        "post": {
            "members_active": post["members_active"].as_bool(),
            "nodes_on_commit": post["nodes_on_commit"].as_bool(),
            "readyz": post["readyz"].as_bool(),
            "cas_smoke": word(&post["cas_smoke"]),
        },
        "offer": {
            "reversible": offer["reversible"].as_bool(),
            "host_reversible": offer["host_reversible"].as_bool(),
            "meta_log": offer["meta_log"].as_u64(),
            "previous_meta_log": offer["previous_meta_log"].as_u64(),
        },
    })
    .to_string()
}

/// A JSON value as a table cell: a string or a number as written, else `?`.
fn show(cell: &Value) -> String {
    match cell {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => "?".to_owned(),
    }
}

/// The markdown of a redacted summary, or why it cannot be shown. It names no node and no
/// host.
pub fn table(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return "### gamma deploy\n\nno summary was printed\n".to_owned();
    };
    // A deploy that succeeded has no reason code: say nothing, not a question mark.
    let reason = v["reason"].as_str();
    let reason = reason.map_or_else(String::new, |r| format!(" {r}"));
    let mut out = format!(
        "### gamma deploy of `{}`\n\nresult: {} (exit {}){reason}\n\n",
        show(&v["commit"]),
        show(&v["result"]),
        show(&v["exit"])
    );
    out.push_str("| platform | state | seconds |\n|---|---|---|\n");
    for n in v["nodes"].as_array().into_iter().flatten() {
        let row = ["platform", "state", "secs"].map(|key| show(&n[key]));
        out.push_str(&format!("| {} |\n", row.join(" | ")));
    }
    let steps: Vec<String> = v["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let ok = s["ok"].as_bool() == Some(true);
            format!("{}={}", show(&s["name"]), if ok { "ok" } else { "FAILED" })
        })
        .collect();
    out.push_str(&format!("\nsteps: {}\n", steps.join(" ")));
    out
}

/// The `gamma summary` verb: the table of `file` appended to `output`.
pub fn summary(file: &Path, output: &Path) -> Result<Vec<String>, Error> {
    let md = table(&fs::read_to_string(file).unwrap_or_default());
    append(output, &md)?;
    Ok(vec![format!("wrote {} bytes", md.len())])
}

/// The tag the gamma host carries: a Tailscale SSH rule's destination is a tag (or the
/// user's own devices), not a device name.
pub const HOST_TAG: &str = "tag:kbf-gamma-host";

/// What a person does once, as lines to print. `{repo}`, `{host}`, `{user}`, `{tag}`,
/// `{host_tag}` and `{sudoers}` are replaced by [`held_steps`].
const HELD: &[&str] = &[
    "# gamma: what a person runs, once. This command prints the steps and changes nothing.",
    "# Each GitHub step needs admin on {repo}. Every name below comes from the flags.",
    "",
    "# 1. GitHub: the environment (deployments from main only), then the variables and the secrets",
    "gh api -X PUT repos/{repo}/environments/gamma -F 'deployment_branch_policy[protected_branches]=false' -F 'deployment_branch_policy[custom_branch_policies]=true'",
    "gh api -X POST repos/{repo}/environments/gamma/deployment-branch-policies -f name=main -f type=branch",
    "gh variable set GAMMA_HOST --repo {repo} --body {host}",
    "gh variable set GAMMA_USER --repo {repo} --body {user}",
    "gh variable set TS_TAG --repo {repo} --body {tag}",
    "gh variable set GAMMA_CHECK_TARGETS --repo {repo}",
    "gh secret set TS_OAUTH_CLIENT_ID --env gamma --repo {repo}",
    "gh secret set TS_OAUTH_SECRET --env gamma --repo {repo}",
    "gh secret set GAMMA_FARM_BUCKCONFIG --env gamma --repo {repo}",
    "# The secrets are asked for on the terminal: no value is on a command line.",
    "# GAMMA_CHECK_TARGETS, asked for the same way, is the komira targets the check job builds after a deploy, separated by spaces. It is a variable, so it is public.",
    "# GAMMA_FARM_BUCKCONFIG is the text of the buckconfig that points buck2 at the farm. It is a secret and not a variable because it names the farm and this repository's logs are public (GitHub masks it line by line).",
    "# While GAMMA_HOST, GAMMA_USER or TS_TAG is unset the deploy job is skipped (a green run), and so is the check job that follows it. To stop gamma, delete one: gh variable delete GAMMA_HOST --repo {repo}",
    "# GitHub prints a variable's value in the log of any step that has it in its environment, and this repository's logs are public: use the short tailnet name for the host, never an address.",
    "",
    r#"# 2. Tailscale: an OAuth client with the scope "Auth Keys: Write" only, restricted to {tag}; its id and secret are the two secrets of step 1."#,
    r#"# Add to the policy file (merge, do not replace). An SSH rule's "dst" is a tag, not a device name, so the host carries {host_tag} (step 3b):"#,
    "{",
    r#"  "tagOwners": { "{tag}": ["autogroup:admin"], "{host_tag}": ["autogroup:admin"] },"#,
    r#"  "grants": [{ "src": ["{tag}"], "dst": ["{host_tag}"], "ip": ["22"] }],"#,
    r#"  "ssh": [{ "action": "accept", "src": ["{tag}"], "dst": ["{host_tag}"], "users": ["{user}"] }]"#,
    "}",
    "# The check job builds on the farm over the tailnet with the same identity ({tag}): also grant it the farm's build port in that policy, and nothing else.",
    "",
    "# 3. The frontend host, as root there",
    "# a. The account {user} exists. It has no password: it only receives the ssh command.",
    "# b. Give the host its tag, which the rules of step 2 name: tailscale set --advertise-tags={host_tag}",
    "# c. Run: visudo -f /etc/sudoers.d/kbf-gamma, and put this one line in it. It has no wildcard; only the commit varies, as forty hex digits.",
    "{sudoers}",
    "# d. kbf-installer is set up with --deploy-stage gamma, which writes /etc/kbf-installer/deploy.json (the API address, the token file and the stage): the workflow names none of them.",
    "",
    "# 4. Repository settings: the Actions policy allows tailscale/github-action.",
];

/// The `gamma held-steps` verb: [`HELD`] with the names filled in. Every name is checked
/// first; nothing is run.
pub fn held_steps(repo: &str, host: &str, user: &str, tag: &str) -> Result<Vec<String>, Error> {
    repo_name(repo)?;
    host_name(host)?;
    account(user)?;
    tag_name(tag)?;
    if tag == HOST_TAG {
        let why = format!("--tag is the host's own tag {HOST_TAG}");
        return Err(Error::Usage(why));
    }
    let sudoers = rule(user);
    let names = [
        ("{repo}", repo),
        ("{host}", host),
        ("{user}", user),
        ("{tag}", tag),
        ("{host_tag}", HOST_TAG),
        ("{sudoers}", sudoers.as_str()),
    ];
    let fill = |line: &str| {
        let swap = |text: String, (key, value): &(&str, &str)| text.replace(key, value);
        names.iter().fold(line.to_owned(), swap)
    };
    Ok(HELD.iter().map(|line| fill(line)).collect())
}

#[cfg(test)]
mod tests;
