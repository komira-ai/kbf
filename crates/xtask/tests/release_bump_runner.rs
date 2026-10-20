//! The real runner and the binary of `cargo xtask release bump-internal`: `Process` against
//! the fake `gh` of this crate and the real `git`, and `xtask` against the fake `gh`. The
//! verb itself, against a recorded world, is tested in `release_bump.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};
use xtask::Error;
use xtask::release::bump::{Process, Tools, gitlink_at, staged_one_gitlink};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const INTERNAL: &str = "example-org/internal";
/// The commit a submodule is at.
const OLD: &str = "1111111111111111111111111111111111111111";
/// The released commit.
const NEW: &str = "2222222222222222222222222222222222222222";
/// Another commit.
const OTHER: &str = "3333333333333333333333333333333333333333";
const TOKEN: &str = "ghs_s3cr3t_t0ken";
/// What the fake `gh` is asked when the tag is looked up, and nothing else follows.
const ASKED: &str = "api repos/komira-ai/kbf/commits/v1.2.3\n";

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-bump"),
        name,
    )
}

fn process_with(gh: PathBuf) -> Process {
    Process::new(gh, PathBuf::from("git"), TOKEN.to_owned())
}

/// A `Process` whose `gh` is never started.
fn process() -> Process {
    process_with(PathBuf::from("gh"))
}

fn args_of(cmd: &Command) -> Vec<String> {
    let mut seen = Vec::new();
    for arg in cmd.get_args() {
        seen.push(arg.to_string_lossy().into_owned());
    }
    seen
}

/// The environment `cmd` is given: `None` for a variable it removes.
fn envs_of(cmd: &Command) -> BTreeMap<String, Option<String>> {
    let mut seen = BTreeMap::new();
    for (key, value) in cmd.get_envs() {
        let value = value.map(|v| v.to_string_lossy().into_owned());
        seen.insert(key.to_string_lossy().into_owned(), value);
    }
    seen
}

#[test]
fn a_call_with_the_token_in_its_arguments_is_refused_before_it_runs() {
    // Catches: the token passed as an argument, where every process on the host can read it
    // (the programs are never started: `gh` need not even be installed on the test host).
    let p = process();
    let leak = format!("repos/x?access_token={TOKEN}");
    for ran in [p.gh(&["api", &leak]), p.git(&["clone", &leak])] {
        let Err(Error::Failed(why)) = ran else {
            panic!("a call with the token in its arguments ran");
        };
        assert!(why.contains("the token is in its arguments"), "{why}");
        assert!(!why.contains(TOKEN), "{why}");
    }
}

#[test]
fn gh_gets_the_token_in_its_environment_and_not_in_its_arguments() {
    // Catches: the token moved from `gh`'s environment into its argument list.
    let cmd = process().gh_command(&["api", "repos/x"]);
    assert_eq!(cmd.get_program(), "gh");
    assert_eq!(args_of(&cmd), ["api", "repos/x"]);
    let env = envs_of(&cmd);
    assert_eq!(env.get("GH_TOKEN"), Some(&Some(TOKEN.to_owned())));
    assert_eq!(env.len(), 1);
}

#[test]
fn git_gets_the_token_as_a_header_in_its_environment_only() {
    // Catches: the token in git's arguments or a remote url, left in `GH_TOKEN` for the
    // programs git starts, or a git that prompts, speaks a protocol but https, or commits as
    // someone but the release bot.
    let cmd = process().git_command(&["status"]);
    assert_eq!(cmd.get_program(), "git");
    assert_eq!(args_of(&cmd), ["status"]);
    let env = envs_of(&cmd);
    let credential = STANDARD.encode(format!("x-access-token:{TOKEN}"));
    let header = format!("AUTHORIZATION: basic {credential}");
    let set = |key: &str| env.get(key).cloned().flatten();
    assert_eq!(env.get("GH_TOKEN"), Some(&None));
    assert_eq!(env.get("GITHUB_TOKEN"), Some(&None));
    assert_eq!(set("GIT_CONFIG_COUNT").as_deref(), Some("1"));
    let key = "http.https://github.com/.extraheader";
    assert_eq!(set("GIT_CONFIG_KEY_0").as_deref(), Some(key));
    assert_eq!(set("GIT_CONFIG_VALUE_0"), Some(header));
    assert_eq!(set("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(set("GIT_ALLOW_PROTOCOL").as_deref(), Some("https"));
    let name = "komira-release-bot[bot]";
    let email = "345322945+komira-release-bot[bot]@users.noreply.github.com";
    assert_eq!(set("GIT_AUTHOR_NAME").as_deref(), Some(name));
    assert_eq!(set("GIT_COMMITTER_NAME").as_deref(), Some(name));
    assert_eq!(set("GIT_AUTHOR_EMAIL").as_deref(), Some(email));
    assert_eq!(set("GIT_COMMITTER_EMAIL").as_deref(), Some(email));
    assert!(env.values().flatten().all(|v| v != TOKEN));
}

/// Puts the fake `gh` at `dir/gh/gh`, with `answers` as GitHub's answers (the fake's
/// `gh-api.json`): the path of the program.
fn fake_gh(dir: &Path, answers: &Value) -> PathBuf {
    std::fs::create_dir_all(dir.join("gh")).expect("gh dir");
    std::fs::hard_link(FAKE, dir.join("gh/gh")).expect("link the fake as gh");
    std::fs::write(dir.join("gh/gh-api.json"), answers.to_string()).expect("answers");
    dir.join("gh/gh")
}

/// What the fake `gh` under `dir` was asked, one line per command.
fn gh_args(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("gh/gh-args")).unwrap_or_default()
}

#[test]
fn the_runner_returns_what_the_program_printed_with_the_token_hidden() {
    // Catches: a token a program echoes (in a url, in an error) reaching the job log, a
    // 404 read as success, and a program that cannot be started read as an answer.
    let own = scratch("runner");
    let answers = json!({ "echo": format!("seen {TOKEN}") });
    let p = process_with(fake_gh(&own, &answers));
    let echoed = p.gh(&["api", "echo"]).expect("the fake runs");
    assert!(echoed.ok);
    assert_eq!(echoed.stdout, "\"seen ***\"\n");
    let missing = p.gh(&["api", "nothing"]).expect("the fake runs");
    assert!(!missing.ok);
    let stderr = missing.stderr;
    assert!(stderr.contains("(HTTP 404)"), "{stderr}");
    let absent = process_with(own.join("none/gh"));
    let spawned = absent.gh(&["api", "x"]);
    assert!(matches!(spawned, Err(Error::Spawn { .. })));
}

#[test]
fn the_git_checks_read_what_real_git_prints() {
    // Catches: a parser of `git ls-tree` or `git diff --raw -z` that fits the fake's text
    // and not git's: a gitlink read from a file, a staged change of two paths read as one.
    let own = scratch("real-git");
    let dir = own.to_str().expect("a UTF-8 path");
    let p = process();
    let git = |args: &[&str]| {
        let ran = p.git(args).expect("git runs");
        assert!(ran.ok, "git {args:?}: {}", ran.stderr);
    };
    git(&["init", "-q", "-b", "main", dir]);
    std::fs::write(own.join("README"), "one\n").expect("README");
    git(&["-C", dir, "add", "README"]);
    let old = format!("160000,{OLD},kbf");
    let mut add = vec!["-C", dir, "update-index", "--add"];
    add.extend(["--cacheinfo", &old]);
    git(&add);
    let mut commit = vec!["-C", dir, "-c", "commit.gpgsign=false"];
    commit.extend(["commit", "-q", "-m", "base"]);
    git(&commit);
    assert_eq!(gitlink_at(&p, dir, "kbf").expect("a gitlink"), OLD);
    for path in ["README", "nothing"] {
        let got = gitlink_at(&p, dir, path);
        let Err(Error::Refused(why)) = got else {
            panic!("{path}: expected a refusal");
        };
        let start = format!("{path} is not a gitlink at HEAD");
        assert!(why[0].starts_with(&start), "{why:?}");
    }
    let new = format!("160000,{NEW},kbf");
    let mut set = vec!["-C", dir, "update-index"];
    set.extend(["--cacheinfo", &new]);
    git(&set);
    staged_one_gitlink(&p, dir, "kbf").expect("one modified gitlink");
    std::fs::write(own.join("README"), "two\n").expect("README");
    git(&["-C", dir, "add", "README"]);
    let got = staged_one_gitlink(&p, dir, "kbf");
    let Err(Error::Refused(why)) = got else {
        panic!("a second staged path was accepted");
    };
    let want = "not exactly one modified gitlink at kbf";
    assert!(why[0].contains(want), "{why:?}");
}

/// `xtask release bump-internal` with `gh` as the program: the exit code, stdout and
/// stderr. `token` is `GH_TOKEN`, or unset.
fn xtask(
    gh: &Path,
    version: &str,
    extra: &[&str],
    token: Option<&str>,
) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(XTASK);
    cmd.args(["release", "bump-internal", "--version", version]);
    cmd.args(["--sha", NEW, "--internal-repo", INTERNAL]);
    cmd.args(["--submodule", "kbf"]);
    cmd.arg("--gh").arg(gh);
    cmd.args(extra);
    cmd.env_remove("GH_TOKEN").env_remove("XTASK_FAKE_DEFECT");
    if let Some(token) = token {
        cmd.env("GH_TOKEN", token);
    }
    let out = cmd.output().expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn without_gh_token_the_binary_fails_before_any_call() {
    // Catches: a run that goes on without the release bot's token, or reads it from a flag.
    let own = scratch("no-token");
    let gh = fake_gh(&own, &json!({}));
    let (code, _, stderr) = xtask(&gh, "1.2.3", &[], None);
    assert_eq!(code, Some(1));
    let want = "xtask: GH_TOKEN is not set (the installation token of the release bot)\n";
    assert_eq!(stderr, want);
    assert_eq!(gh_args(&own), "");
}

#[test]
fn a_malformed_version_is_a_usage_error_before_any_call() {
    // Catches: a version with a leading `v` accepted, which builds the branch `bump/kbf-vv1`.
    let own = scratch("usage");
    let gh = fake_gh(&own, &json!({}));
    let (code, _, stderr) = xtask(&gh, "v1.2.3", &[], Some(TOKEN));
    assert_eq!(code, Some(2));
    let start = "xtask: --version \"v1.2.3\" is not <major>";
    assert!(stderr.starts_with(start), "{stderr}");
    assert_eq!(gh_args(&own), "");
}

#[test]
fn a_missing_tag_is_refused_by_the_binary_and_the_token_stays_out_of_argv() {
    // Catches: the token in an argument of any `gh` call the binary makes (the fake records
    // each argv), and a tag that is not looked up in the kbf repository by default.
    let own = scratch("missing-tag");
    let gh = fake_gh(&own, &json!({}));
    let (code, stdout, stderr) = xtask(&gh, "1.2.3", &[], Some(TOKEN));
    assert_eq!(code, Some(1));
    let want = "xtask: the tag v1.2.3 does not exist in komira-ai/kbf\n";
    assert_eq!(stderr, want);
    assert_eq!(gh_args(&own), ASKED);
    for seen in [stdout, stderr, gh_args(&own)] {
        assert!(!seen.contains(TOKEN), "{seen}");
    }
}

#[test]
fn a_tag_on_another_commit_is_refused_by_the_binary_with_a_workdir_given() {
    // Catches: a tag check that the binary skips, and the workdir flag not reaching the run.
    let answers = json!({ "repos/komira-ai/kbf/commits/v1.2.3": { "sha": OTHER } });
    let own = scratch("other-tag");
    let gh = fake_gh(&own, &answers);
    let clone = own.join("clone");
    let clone = clone.to_str().expect("a UTF-8 path");
    let (code, _, stderr) = xtask(&gh, "1.2.3", &["--workdir", clone], Some(TOKEN));
    assert_eq!(code, Some(1));
    let want = format!("names {OTHER:?}, not {NEW}");
    assert!(stderr.contains(&want), "{stderr}");
    assert_eq!(gh_args(&own), ASKED);
}
