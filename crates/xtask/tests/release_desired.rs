//! `cargo xtask release desired`, run as the built binary with `xtask-fake` as `gh` (its
//! `gh-api.json` holds what GitHub answers, its `releases/<tag>` directory what `gh release
//! download` fetches): the exit statuses (0 for a deliberate no-set, 1 for a refusal, 2 for flags
//! that do not fit), what reaches stdout and stderr, and the lines appended to the output file.
//! The real `gh` and `curl` are never started and nothing touches the network; the apt index is
//! covered by the unit tests of the verb, which fake the CDN.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-desired"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("gh dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake as gh");
    Fx { root, _own: own }
}

impl Fx {
    /// What GitHub answers: `(path, answer)`.
    fn github(&self, answers: &[(&str, Value)]) {
        let map: serde_json::Map<String, Value> = answers
            .iter()
            .map(|(path, answer)| ((*path).to_owned(), answer.clone()))
            .collect();
        std::fs::write(
            self.root.join("gh/gh-api.json"),
            Value::Object(map).to_string(),
        )
        .expect("gh-api.json");
    }

    fn out(&self) -> PathBuf {
        self.root.join("out")
    }

    fn desired(&self, flags: &[&str]) -> Output {
        Command::new(XTASK)
            .args(["release", "desired", "--repo", "o/r", "--dir"])
            .arg(self.root.join("d"))
            .arg("--output")
            .arg(self.out())
            .arg("--gh")
            .arg(self.root.join("gh/gh"))
            .arg("--curl")
            .arg(self.root.join("no-curl"))
            .args(flags)
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run xtask")
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("utf-8")
}

/// Asserts that a run whose `publish` job was skipped exits 0, prints why nothing is set and
/// `set=false`, and appends `set=false` to the output file: most release runs publish nothing.
#[test]
fn a_run_that_published_nothing_exits_zero_with_set_false() {
    let fx = fx("none");
    fx.github(&[(
        "repos/o/r/actions/runs/42/jobs?per_page=100&filter=latest",
        json!({"total_count": 1, "jobs": [{"name": "publish", "conclusion": "skipped"}]}),
    )]);
    let out = fx.desired(&["--version", "", "--after-run", "42"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(out.stderr));
    assert_eq!(
        text(out.stdout),
        "run 42 of o/r did not publish a release (publish did not succeed): nothing is set\nset=false\n"
    );
    assert_eq!(
        std::fs::read_to_string(fx.out()).expect("output"),
        "set=false\n"
    );
}

/// Asserts that both flags empty, and both given, exit 2 with the usage reason on stderr and
/// nothing on stdout or in the output file.
#[test]
fn both_and_neither_of_version_and_after_run_exit_two() {
    let fx = fx("usage");
    for flags in [
        ["--version", "", "--after-run", ""],
        ["--version", "0.0.66", "--after-run", "9"],
    ] {
        let out = fx.desired(&flags);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(out.stdout, b"");
        assert_eq!(
            text(out.stderr),
            "xtask: give one of --version and --after-run, not both and not neither\n"
        );
        assert!(!fx.out().exists());
    }
}

/// Asserts that a release `gh` cannot find is a failure, status 1, naming the call, and that
/// nothing is appended to the output file (the coordinator is never called).
#[test]
fn a_release_that_cannot_be_read_exits_one_and_sets_nothing() {
    let fx = fx("missing");
    fx.github(&[]);
    let out = fx.desired(&["--version", "0.0.66", "--after-run", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        text(out.stderr),
        "xtask: gh api repos/o/r/releases/tags/v0.0.66: not found\n"
    );
    assert!(!fx.out().exists());
}

/// Asserts that a draft release named by `--version` exits 1 even though `gh` finds it.
#[test]
fn a_draft_release_exits_one() {
    let fx = fx("draft");
    fx.github(&[(
        "repos/o/r/releases/tags/v0.0.66",
        json!({"draft": true, "assets": []}),
    )]);
    let out = fx.desired(&["--version", "0.0.66", "--after-run", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(out.stderr),
        "xtask: release v0.0.66 of o/r is a draft or a pre-release, not a released version\n"
    );
}

/// Asserts that a release without `SHA256SUMS` exits 1 with the refusal naming it: the stable
/// index has nothing to be held to.
#[test]
fn a_release_without_sums_exits_one_naming_them() {
    let fx = fx("nosums");
    fx.github(&[(
        "repos/o/r/releases/tags/v0.0.66",
        json!({"draft": false, "prerelease": false, "assets": [{"name": "kbf-native.deb"}]}),
    )]);
    let out = fx.desired(&["--version", "0.0.66", "--after-run", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        text(out.stderr),
        "xtask: release v0.0.66 has no SHA256SUMS: there are no digests to hold the stable suite to\n"
    );
}

/// Asserts the path through the download: the release's `SHA256SUMS`, fetched by `gh release
/// download` into `--dir`, that lists neither native `.deb` of the tag's commit (only another
/// file) exits 1 with one refusal line per `.deb`, and writes no output.
#[test]
fn sums_without_the_native_debs_of_the_tags_commit_exit_one() {
    let fx = fx("sums");
    fx.github(&[
        (
            "repos/o/r/releases/tags/v0.0.66",
            json!({"draft": false, "prerelease": false, "assets": [{"name": "SHA256SUMS"}]}),
        ),
        (
            "repos/o/r/commits/refs/tags/v0.0.66",
            json!({ "sha": COMMIT }),
        ),
    ]);
    let release = fx.root.join("gh/releases/v0.0.66");
    std::fs::create_dir_all(&release).expect("release");
    std::fs::write(
        release.join("SHA256SUMS"),
        format!("{DIGEST}  kbf-{COMMIT}-linux-x86_64.tar.gz\n"),
    )
    .expect("sums");
    let out = fx.desired(&["--version", "0.0.66", "--after-run", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        text(out.stderr),
        format!(
            "xtask: SHA256SUMS of release v0.0.66 lists no kbf-native-{COMMIT}-linux-x86_64.deb\n\
             SHA256SUMS of release v0.0.66 lists no kbf-native-{COMMIT}-linux-arm64.deb\n"
        )
    );
    assert!(!fx.out().exists());
}
