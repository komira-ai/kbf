//! `cargo xtask release plan` as the release workflow runs it: a temp git repository for
//! the push (`VERSION`, the files it changed), and `xtask-fake` as `gh`, whose
//! `gh-api.json` holds the release tags, the releases and the compares each test writes.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};
use xtask::release::tags::{Base, Version};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const ZERO: &str = "0000000000000000000000000000000000000000";
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/release-plan.txt");
const VERSION_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../VERSION");

/// One test's world: a git repository and a fake `gh` in a directory of its own.
struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// A repository whose one commit holds `VERSION` as `version`, and the fake as `gh`.
fn fx(name: &str, version: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-plan"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("gh dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake as gh");
    std::fs::create_dir_all(root.join("repo")).expect("repo dir");
    let fx = Fx { root, _own: own };
    git(
        &fx.repo(),
        &["init", "-q", "-b", "main", "--object-format=sha1"],
    );
    fx.commit(&[("VERSION", Some(version))]);
    fx
}

impl Fx {
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    /// Commits `files`, `(path, Some(contents))` written and `(path, None)` removed, and
    /// returns the new commit; no file at all makes an empty commit.
    fn commit(&self, files: &[(&str, Option<&str>)]) -> String {
        for (path, contents) in files {
            let at = self.repo().join(path);
            match contents {
                Some(text) => {
                    std::fs::create_dir_all(at.parent().expect("a parent")).expect("dirs");
                    std::fs::write(&at, text).expect("write");
                }
                None => std::fs::remove_file(&at).expect("remove"),
            }
        }
        git(&self.repo(), &["add", "-A"]);
        git(
            &self.repo(),
            &["commit", "-q", "--allow-empty", "-m", "change"],
        );
        git(&self.repo(), &["rev-parse", "HEAD"])
    }

    /// GitHub's answers: the release tags `tags`, the releases `drafts` (a draft names a
    /// tag that need not exist), and `compares`, `(tag, commit, status)`.
    fn github(&self, tags: &[&str], releases: &[&str], compares: &[(&str, &str, &str)]) {
        let refs: Vec<Value> = tags
            .iter()
            .map(|t| json!({ "ref": format!("refs/tags/{t}") }))
            .collect();
        let releases: Vec<Value> = releases.iter().map(|t| json!({ "tag_name": t })).collect();
        let mut map = serde_json::Map::new();
        map.insert(refs_path(1), Value::Array(refs));
        map.insert(releases_path(1), Value::Array(releases));
        for (tag, commit, status) in compares {
            map.insert(
                format!("repos/{REPO}/compare/{tag}...{commit}"),
                json!({ "status": status }),
            );
        }
        self.write_answers(map);
    }

    fn write_answers(&self, map: serde_json::Map<String, Value>) {
        std::fs::write(
            self.root.join("gh/gh-api.json"),
            Value::Object(map).to_string(),
        )
        .expect("gh-api.json");
    }

    /// What the fake gh was asked, one line per command.
    fn gh_args(&self) -> String {
        std::fs::read_to_string(self.root.join("gh/gh-args")).unwrap_or_default()
    }

    /// `release plan` for `sha` pushed over `before`, with `extra` flags.
    fn plan(&self, sha: &str, before: &str, extra: &[&str]) -> (Option<i32>, String, String) {
        let repo = self.repo();
        let gh = self.root.join("gh/gh");
        let out = Command::new(XTASK)
            .args([
                "release", "plan", "--repo", REPO, "--sha", sha, "--before", before,
            ])
            .arg("--repo-dir")
            .arg(&repo)
            .arg("--gh")
            .arg(&gh)
            .args(extra)
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    }

    /// The pushed commit is the head and the push is a code change over `before`.
    fn plan_head(&self) -> (Option<i32>, String, String) {
        let head = git(&self.repo(), &["rev-parse", "HEAD"]);
        self.plan(&head, ZERO, &[])
    }
}

fn refs_path(page: u32) -> String {
    format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100&page={page}")
}

fn releases_path(page: u32) -> String {
    format!("repos/{REPO}/releases?per_page=100&page={page}")
}

/// The line `key=` of a plan's output.
fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in {out}"))
}

#[test]
fn the_patch_is_one_above_the_highest_numerically_not_as_text() {
    // Catches: a string compare of N, which puts 9 above 10 and hands out 10 again.
    let fx = fx("numeric", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(
        &["v0.0.2", "v0.0.9", "v0.0.10"],
        &[],
        &[("v0.0.10", &sha, "ahead")],
    );
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.11", "{out}");
    assert_eq!(value(&out, "tag"), "v0.0.11");
}

#[test]
fn nine_is_followed_by_ten() {
    // Catches: a patch that stops at a digit (9 -> 9 or 90), or reads the highest wrong.
    let fx = fx("nine-ten", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&["v0.0.8", "v0.0.9"], &[], &[("v0.0.9", &sha, "ahead")]);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.10", "{out}");
}

#[test]
fn the_first_release_of_a_base_is_patch_one() {
    // Catches: a first release numbered 0, or a repository with no tag refused.
    let fx = fx("first", "0.0\n");
    fx.github(&[], &[], &[]);
    let (code, out, err) = fx.plan_head();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.1", "{out}");
    assert_eq!(value(&out, "reused"), "false");
}

#[test]
fn tags_of_another_base_do_not_count() {
    // Catches: a patch taken from the highest tag whatever its base (0.1.7 -> 0.2.8).
    let fx = fx("other-base", "0.2\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(
        &["v0.1.7", "v0.1.3"],
        &["v0.1.9"],
        &[("v0.1.7", &sha, "ahead")],
    );
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.2.1", "{out}");
}

#[test]
fn a_higher_base_than_version_stops_the_run() {
    // Catches: a base that may go backwards. The compare says `ahead`, so only the
    // base check can refuse here.
    let fx = fx("backwards", "0.1\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&["v0.1.4", "v0.2.1"], &[], &[("v0.2.1", &sha, "ahead")]);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.contains("VERSION is 0.1 but a release of 0.2 exists: the base never goes backwards"),
        "{err}"
    );
    assert_eq!(out, "");
}

#[test]
fn a_draft_of_a_higher_base_stops_the_run_and_a_draft_takes_its_number() {
    // Catches: drafts ignored, for the base and for the patch.
    let fx = fx("drafts", "0.1\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&["v0.1.2"], &["v0.2.0"], &[("v0.1.2", &sha, "ahead")]);
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("never goes backwards"), "{err}");

    fx.github(&["v0.1.2"], &["v0.1.4"], &[("v0.1.2", &sha, "ahead")]);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.1.5", "{out}");
}

#[test]
fn suffixed_and_odd_tags_are_not_counted() {
    // Catches: a `-rc1` or `-probe` tag read as a release (it would be the highest and
    // number the next one 8), and a tag that is not three plain numbers read as one.
    let fx = fx("suffix", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(
        &[
            "v0.0.1",
            "v0.0.3",
            "v0.0.5-rc1",
            "v0.0.7-probe",
            "v0.0.9+build",
            "v0.0.08",
            "v0.0.+9",
            "v0.0",
            "v0.0.1.2",
            "v0..4",
            "v0.0.99999999999999999999",
            "0.0.50",
            "vlatest",
        ],
        &["v0.0.6-rc2", "nightly"],
        &[("v0.0.3", &sha, "ahead")],
    );
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.4", "{out}");
}

#[test]
fn a_rerun_of_the_tagged_commit_reports_its_version_again() {
    // Catches: a re-run that numbers its own commit the next version (two releases of
    // one commit), or one that reuses a number a newer draft has taken.
    let fx = fx("reuse", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&["v0.0.1", "v0.0.2"], &[], &[("v0.0.2", &sha, "identical")]);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.2", "{out}");
    assert_eq!(value(&out, "reused"), "true");

    fx.github(
        &["v0.0.1", "v0.0.2"],
        &["v0.0.3"],
        &[("v0.0.2", &sha, "identical")],
    );
    let (_, out, _) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(value(&out, "version"), "0.0.4", "{out}");
    assert_eq!(value(&out, "reused"), "false");
}

#[test]
fn a_bumped_base_does_not_reuse_the_old_base_tag() {
    // Catches: a reuse that ignores the base: the commit the highest tag names, when
    // VERSION already names a newer base, is 0.1.1, not the old tag again.
    let fx = fx("bump", "0.1\n");
    let sha = git(&fx.repo(), &["rev-parse", "HEAD"]);
    fx.github(&["v0.0.5"], &[], &[("v0.0.5", &sha, "identical")]);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.1.1", "{out}");
    assert_eq!(value(&out, "reused"), "false");

    // And with a draft of the new base: the old tag is the highest of all, not of the base.
    fx.github(&["v0.0.5"], &["v0.1.1"], &[("v0.0.5", &sha, "identical")]);
    let (_, out, _) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(value(&out, "version"), "0.1.2", "{out}");
    assert_eq!(value(&out, "reused"), "false");
}

#[test]
fn a_commit_behind_or_beside_the_highest_tag_is_superseded() {
    // Catches: the ancestor check accepting `behind` (the commit is an ancestor of a
    // newer release: an old run), `diverged`, or a compare GitHub cannot answer.
    let fx = fx("superseded", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    for status in ["behind", "diverged", "weird"] {
        fx.github(&["v0.0.4"], &[], &[("v0.0.4", &sha, status)]);
        let (code, out, err) = fx.plan(&sha, ZERO, &[]);
        assert_eq!(code, Some(1), "{status}: {out}");
        assert!(
            err.contains(&format!(
                "superseded: the highest release tag is v0.0.4 and {sha} is not at or after it \
                 (compare says {status})"
            )),
            "{status}: {err}"
        );
        assert_eq!(out, "", "{status}");
    }
    fx.github(&["v0.0.4"], &[], &[]);
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("compare says nothing"), "{err}");
}

#[test]
fn the_freeze_refuses_before_anything_is_asked_of_github() {
    // Catches: a freeze that is ignored, one that still lets the run call GitHub, or
    // one that an empty variable (unset on Actions) trips.
    let fx = fx("freeze", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&[], &[], &[]);
    let (code, out, err) = fx.plan(&sha, ZERO, &["--freeze", "incident 42"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        err.contains("frozen: RELEASE_FREEZE is \"incident 42\": nothing is released"),
        "{err}"
    );
    assert_eq!(fx.gh_args(), "");
    let (code, _, err) = fx.plan(&sha, ZERO, &["--freeze", ""]);
    assert_eq!(code, Some(0), "{err}");
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_version_file_that_is_not_major_dot_minor_is_refused() {
    // Catches: a lax VERSION: a `v`, a patch, spaces, a missing or doubled line end,
    // a leading zero, a sign, an empty file.
    for (i, bad) in [
        "0.0", "v0.0\n", "0.0.1\n", "01.0\n", "0.0\n\n", " 0.0\n", "", "\n", "0.\n", ".1\n",
        "+1.0\n", "0.x\n", "0.0\r\n",
    ]
    .into_iter()
    .enumerate()
    {
        let fx = fx(&format!("bad-version-{i}"), bad);
        fx.github(&[], &[], &[]);
        let (code, out, err) = fx.plan_head();
        assert_eq!(code, Some(1), "{bad:?}: {out}");
        assert!(err.contains("VERSION at "), "{bad:?}: {err}");
        assert!(
            err.contains("expected MAJOR.MINOR and one line end"),
            "{err}"
        );
    }
}

#[test]
fn a_commit_with_no_version_file_fails_the_run() {
    let fx = fx("no-version", "0.0\n");
    let sha = fx.commit(&[("VERSION", None)]);
    fx.github(&[], &[], &[]);
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("VERSION"), "{err}");
}

#[test]
fn version_comes_from_the_commit_pushed() {
    // Catches: a VERSION read from the checkout's head or working tree: a later commit
    // (or an edit) changes the version of an earlier push that is planned again.
    let fx = fx("tree", "0.3\n");
    let sha = git(&fx.repo(), &["rev-parse", "HEAD"]);
    fx.commit(&[("VERSION", Some("0.4\n"))]);
    std::fs::write(fx.repo().join("VERSION"), "9.9\n").expect("edit");
    fx.github(&[], &[], &[]);
    let (_, out, _) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(value(&out, "version"), "0.3.1", "{out}");
}

#[test]
fn tag_lists_are_read_page_after_page() {
    // Catches: only the first 100 tags read (the highest hidden on page 2), and a short
    // page that is followed by one more request.
    let fx = fx("pages", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    let page1: Vec<Value> = (1..=100)
        .map(|n| json!({ "ref": format!("refs/tags/v0.0.{n}") }))
        .collect();
    let mut map = serde_json::Map::new();
    map.insert(refs_path(1), Value::Array(page1));
    map.insert(refs_path(2), json!([{ "ref": "refs/tags/v0.0.101" }]));
    map.insert(releases_path(1), json!([]));
    map.insert(
        format!("repos/{REPO}/compare/v0.0.101...{sha}"),
        json!({ "status": "ahead" }),
    );
    fx.write_answers(map);
    let (code, out, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "0.0.102", "{out}");
    let asked = fx.gh_args();
    assert_eq!(asked.matches("matching-refs").count(), 4, "{asked}");
    assert!(!asked.contains("page=3"), "{asked}");
}

#[test]
fn a_list_github_does_not_give_fails_the_run_rather_than_reading_as_empty() {
    // Catches: a failed or odd tag list read as "no tags", which lets any commit pass.
    let fx = fx("no-list", "0.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.write_answers(serde_json::Map::new());
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("no list of entries"), "{err}");

    let mut map = serde_json::Map::new();
    map.insert(refs_path(1), json!({ "message": "not a list" }));
    fx.write_answers(map);
    let (code, _, err) = fx.plan(&sha, ZERO, &[]);
    assert_eq!(code, Some(1));
    assert!(err.contains("no list of entries"), "{err}");
}

/// A change to a repository (`VERSION` is there already), and the class it must have.
type Case = (&'static [(&'static str, &'static str)], [bool; 3]);

#[test]
fn the_push_is_classified_by_the_files_it_changed() {
    // Catches: a mixed push read as docs, a path that ships read as one that does not
    // (or the reverse), an anchored directory matched at any depth, a rename that hides
    // its old path, a path git would quote read wrong.
    let fx = fx("classify", "0.0\n");
    fx.github(&[], &[], &[]);
    // (files written; [code, site, pipeline])
    let cases: &[Case] = &[
        (&[("README.md", "x")], [false, false, false]),
        (&[("crates/a/README.md", "x")], [false, false, false]),
        (&[("docs/a.txt", "x")], [false, true, false]),
        (&[("docs/b.md", "x")], [false, true, false]),
        (&[("site/index.html", "x")], [false, true, false]),
        (&[(".github/CODEOWNERS", "x")], [false, false, false]),
        (&[(".github/workflows/ci.yml", "x")], [false, false, true]),
        (&[("infra/site/main.ts", "x")], [false, false, true]),
        (&[("infra/other/main.ts", "x")], [false, false, false]),
        (&[("crates/a/src/lib.rs", "x")], [true, false, false]),
        (&[("src/docs/x.rs", "x")], [true, false, false]),
        (&[("src/site/x.rs", "x")], [true, false, false]),
        (&[("VERSION", "0.1\n")], [true, false, false]),
        (&[("crates/xtask/src/release.rs", "x")], [true, false, true]),
        (
            &[("crates/xtask/src/release/plan.rs", "x")],
            [true, false, true],
        ),
        (&[("crates/xtask/src/lib.rs", "x")], [true, false, false]),
        (
            &[("crates/xtask/tests/release.rs", "x")],
            [true, false, false],
        ),
        (&[("notes.md.txt", "x")], [true, false, false]),
        (
            &[("README.md", "y"), ("crates/a/src/lib.rs", "y")],
            [true, false, false],
        ),
        (
            &[("README.md", "z"), ("docs/a.txt", "z")],
            [false, true, false],
        ),
        (
            &[("docs/a.txt", "z"), ("site/index.html", "z")],
            [false, true, false],
        ),
        (
            &[
                (".github/workflows/ci.yml", "y"),
                ("crates/a/src/lib.rs", "z"),
            ],
            [true, false, true],
        ),
        (&[("src/my file.rs", "x")], [true, false, false]),
        (&[("docs/\u{e9}t\u{e9}.txt", "x")], [false, true, false]),
        (&[("src/\u{e9}t\u{e9}.rs", "x")], [true, false, false]),
    ];
    for (i, (files, [code, site, pipeline])) in cases.iter().enumerate() {
        let before = git(&fx.repo(), &["rev-parse", "HEAD"]);
        // Each case writes contents no earlier case did, so every file it names changes.
        let unique = i.to_string();
        let files: Vec<(&str, Option<&str>)> = files
            .iter()
            .map(|(p, t)| (*p, Some(if *p == "VERSION" { *t } else { unique.as_str() })))
            .collect();
        let sha = fx.commit(&files);
        let (status, out, err) = fx.plan(&sha, &before, &[]);
        assert_eq!(status, Some(0), "{files:?}: {err}");
        let got = [
            value(&out, "code"),
            value(&out, "site"),
            value(&out, "pipeline"),
        ];
        let want = [code, site, pipeline].map(|b| b.to_string());
        assert_eq!(got.map(str::to_owned), want, "{files:?}");
    }
}

#[test]
fn a_move_from_code_to_docs_is_still_code() {
    // Catches: a rename listed as its new path only, which reads as docs.
    let fx = fx("rename", "0.0\n");
    fx.github(&[], &[], &[]);
    let before = fx.commit(&[("src/big.rs", Some("fn main() {}\n".repeat(20).as_str()))]);
    std::fs::create_dir_all(fx.repo().join("docs")).expect("docs");
    git(&fx.repo(), &["mv", "src/big.rs", "docs/big.rs"]);
    git(&fx.repo(), &["commit", "-q", "-m", "move"]);
    let sha = git(&fx.repo(), &["rev-parse", "HEAD"]);
    let (code, out, err) = fx.plan(&sha, &before, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "code"), "true", "{out}");
    assert_eq!(value(&out, "site"), "true");
}

#[test]
fn a_push_that_changed_no_file_is_neither_code_nor_site() {
    // Catches: an empty diff read as code (a version burned on nothing).
    let fx = fx("empty", "0.0\n");
    fx.github(&[], &[], &[]);
    let before = git(&fx.repo(), &["rev-parse", "HEAD"]);
    let sha = fx.commit(&[]);
    let (code, out, err) = fx.plan(&sha, &before, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        [
            value(&out, "code"),
            value(&out, "site"),
            value(&out, "pipeline")
        ],
        ["false"; 3]
    );
}

#[test]
fn a_first_push_is_code_and_site_but_no_pipeline_change() {
    // Catches: an all-zero `before` diffed (an error), read as docs, or as a change to
    // the pipeline that would block the first release. The last commit alone is docs,
    // so only the rule for a zero `before` makes it code.
    let fx = fx("zero", "0.0\n");
    fx.github(&[], &[], &[]);
    fx.commit(&[("README.md", Some("x"))]);
    let (code, out, err) = fx.plan_head();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        [
            value(&out, "code"),
            value(&out, "site"),
            value(&out, "pipeline")
        ],
        ["true", "true", "false"]
    );
}

#[test]
fn a_before_the_checkout_does_not_hold_is_code_site_and_pipeline() {
    // Catches: a forced push (or a shallow checkout) failing the run, or read as docs,
    // or as a change that cannot have touched the pipeline.
    let fx = fx("lost", "0.0\n");
    fx.github(&[], &[], &[]);
    let sha = fx.commit(&[("README.md", Some("x"))]);
    let (code, out, err) = fx.plan(&sha, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        [
            value(&out, "code"),
            value(&out, "site"),
            value(&out, "pipeline")
        ],
        ["true", "true", "true"]
    );
}

#[test]
fn a_sha_or_before_that_is_not_a_commit_is_a_usage_error() {
    let fx = fx("usage", "0.0\n");
    fx.github(&[], &[], &[]);
    let sha = git(&fx.repo(), &["rev-parse", "HEAD"]);
    let (code, _, err) = fx.plan("abc", ZERO, &[]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--sha"), "{err}");
    for before in ["xyz", "", "ABCDEF123456"] {
        let (code, _, err) = fx.plan(&sha, before, &[]);
        assert_eq!(code, Some(2), "{before:?}: {err}");
        assert!(err.contains("--before"), "{err}");
    }
    assert_eq!(fx.gh_args(), "");
}

#[test]
fn output_is_appended_to_the_file_and_printed_as_the_golden_has_it() {
    // Catches: a changed output (the workflow reads these keys), an output that
    // replaces the file's earlier lines, and one that differs from what is printed.
    let fx = fx("golden", "0.1\n");
    let before = git(&fx.repo(), &["rev-parse", "HEAD"]);
    let sha = fx.commit(&[("crates/a/src/lib.rs", Some("x"))]);
    fx.github(
        &["v0.1.1", "v0.1.2", "v0.0.9"],
        &[],
        &[("v0.1.2", &sha, "ahead")],
    );
    let file = fx.root.join("github-output");
    std::fs::write(&file, "earlier=line\n").expect("output");
    let (code, out, err) = fx.plan(&sha, &before, &["--output", file.to_str().expect("utf-8")]);
    assert_eq!(code, Some(0), "{err}");
    let golden = std::fs::read_to_string(GOLDEN).expect("golden file");
    assert_eq!(out, golden);
    assert_eq!(
        std::fs::read_to_string(&file).expect("output"),
        format!("earlier=line\n{golden}")
    );
}

#[test]
fn an_output_file_that_cannot_be_written_fails_the_run() {
    let fx = fx("bad-output", "0.0\n");
    fx.github(&[], &[], &[]);
    let missing = fx.root.join("no-such-dir/out");
    let head = git(&fx.repo(), &["rev-parse", "HEAD"]);
    let (code, _, err) = fx.plan(&head, ZERO, &["--output", missing.to_str().expect("utf-8")]);
    assert_eq!(code, Some(1));
    assert!(err.contains("append to"), "{err}");
}

#[test]
fn versions_and_tags_parse_as_three_plain_numbers() {
    // Catches: a tag spelling that two readers could number differently.
    let v = |t: &str| Version::parse_tag(t).map(|v| v.to_string());
    assert_eq!(v("v0.0.1").as_deref(), Some("0.0.1"));
    assert_eq!(v("v10.20.30").as_deref(), Some("10.20.30"));
    for bad in [
        "v1.2",
        "1.2.3",
        "v1.2.3.4",
        "v1.2.3-rc",
        "v1.02.3",
        "v1..3",
        "v1.2.-3",
        "V1.2.3",
        "",
    ] {
        assert_eq!(v(bad), None, "{bad}");
    }
    assert!(Version::parse_tag("v0.0.9") < Version::parse_tag("v0.0.10"));
    assert!(Version::parse_tag("v0.9.0") < Version::parse_tag("v0.10.0"));
    assert!(Version::parse_tag("v9.0.0") < Version::parse_tag("v10.0.0"));
    assert_eq!(
        Version::parse_tag("v1.2.3").map(|v| v.tag()).as_deref(),
        Some("v1.2.3")
    );
}

#[test]
fn the_repository_version_file_has_the_shape_the_pipeline_reads() {
    // Catches: a VERSION edit the pipeline would refuse at the first release.
    let text = std::fs::read_to_string(VERSION_FILE).expect("VERSION at the repository root");
    assert!(Base::parse_file(&text).is_some(), "VERSION is {text:?}");
}
