//! `cargo xtask release farm-version`: the version text the farm is told to converge on is
//! `<version>+g<12 hex digits of the commit>`, read from the facts of the release's own build
//! (fetched with `gh release download`, here a fake `gh` that copies a directory), and
//! anything that is not the file `SHA256SUMS` lists, is not the release build of the commit,
//! or does not make that form, ends the verb with exit 1, no `version=` line and no output.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "3aa7c93274c5f00dfeed0123456789abcdef0123";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const NAME: &str = "kbf-3aa7c93274c5f00dfeed0123456789abcdef0123-linux-x86_64.version.json";

fn facts(version: &str, commit: &str, full: Option<&str>, profile: &str) -> String {
    let full = full.map_or(String::new(), |f| format!("\"commit_full\": \"{f}\", "));
    format!(
        "{{\"commit\": \"{}\", {full}\"version\": \"{version}\", \"profile\": \"{profile}\", \"protocol\": 1, \"min_protocol\": 1, \"state_schema\": 1, \"seq\": 7}}\n",
        &commit[..12]
    )
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A scratch directory with `gh/gh` (the fake, whose `release download` copies
/// `gh/releases/0.0.13/` into `--dir` and records its arguments in `gh/gh-args`) and `out`.
struct Fx {
    root: TestDir,
}

impl Fx {
    fn new(name: &str) -> Self {
        let root = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-farm-version"),
            name,
        );
        let release = root.path().join("gh/releases/0.0.13");
        std::fs::create_dir_all(&release).expect("release");
        std::fs::hard_link(FAKE, root.path().join("gh/gh")).expect("link the fake as gh");
        Self { root }
    }

    /// The release holds `facts` (under the facts' name) and a `SHA256SUMS` listing `listed`.
    fn release(&self, facts: &str, listed: &str) {
        let dir = self.root.path().join("gh/releases/0.0.13");
        std::fs::write(dir.join(NAME), facts).expect("facts");
        let sums = format!("{}  {NAME}\n", sha_of(listed.as_bytes()));
        std::fs::write(dir.join("SHA256SUMS"), sums).expect("sums");
    }

    fn out(&self) -> PathBuf {
        self.root.path().join("out")
    }

    fn run(&self, commit: &str) -> (Output, String) {
        let ran = Command::new(XTASK)
            .args([
                "release",
                "farm-version",
                "--repo",
                "o/r",
                "--tag",
                "0.0.13",
            ])
            .args(["--commit", commit, "--gh"])
            .arg(self.root.path().join("gh/gh"))
            .arg("--output")
            .arg(self.out())
            .output()
            .expect("run");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let says = format!("{}{}", text(&ran.stdout), text(&ran.stderr));
        (ran, says)
    }
}

#[test]
fn the_version_is_the_builds_own_version_and_commit_in_the_heartbeat_form() {
    // Proves: the facts of the release build of the commit, listed by SHA256SUMS, give
    // `<version>+g<12 hex>` (not the tag the release is downloaded by, which the facts do not
    // hold), printed and appended to `--output` after what is already there; and the release
    // is asked of `gh` by tag and repository with exactly the two files. It would catch a
    // verb that returns the tag, the 40-digit commit or a text without the `g`.
    let fx = Fx::new("ok");
    let text = facts("0.1.0", COMMIT, Some(COMMIT), "release");
    fx.release(&text, &text);
    std::fs::write(fx.out(), "tag=0.0.13\n").expect("write");
    let (ran, says) = fx.run(COMMIT);
    assert_eq!(ran.status.code(), Some(0), "{says}");
    assert!(says.contains("version=0.1.0+g3aa7c93274c5\n"), "{says}");
    assert_eq!(
        std::fs::read_to_string(fx.out()).expect("read"),
        "tag=0.0.13\nversion=0.1.0+g3aa7c93274c5\n"
    );
    let argv = std::fs::read_to_string(fx.root.path().join("gh/gh-args")).expect("argv");
    assert!(
        argv.starts_with("release download 0.0.13 --repo o/r --dir "),
        "{argv}"
    );
    assert!(
        argv.contains(&format!("--pattern SHA256SUMS --pattern {NAME}")),
        "{argv}"
    );
    // A build that does not state its full commit is judged by its 12 digits.
    let text = facts("0.2.0-rc.1", COMMIT, None, "release");
    fx.release(&text, &text);
    let (_, says) = fx.run(COMMIT);
    assert!(says.contains("version=0.2.0-rc.1+g3aa7c93274c5"), "{says}");
}

#[test]
fn facts_that_are_not_the_release_build_of_the_commit_are_refused_with_no_output() {
    // Proves: another commit's facts, a debug build, a full commit that disagrees with
    // `--commit`, a version that makes no heartbeat form, bytes that are no facts, facts
    // that SHA256SUMS does not list (other bytes than it lists) and a release without the
    // facts each end with exit 1, no `version=` line and nothing written to `--output`.
    let fx = Fx::new("refused");
    let good = facts("0.1.0", COMMIT, None, "release");
    let cases = [
        (facts("0.1.0", OTHER, None, "release"), None),
        (facts("0.1.0", COMMIT, None, "debug"), None),
        (facts("0.1.0", COMMIT, Some(OTHER), "release"), None),
        (facts("next", COMMIT, None, "release"), None),
        (facts("0.0.13+g3aa7c93274c5", COMMIT, None, "release"), None),
        ("not json".to_owned(), None),
        (String::new(), None),
        // Listed bytes differ from the bytes downloaded.
        (good.clone(), Some(format!("{good} "))),
    ];
    for (text, listed) in cases {
        fx.release(&text, listed.as_deref().unwrap_or(&text));
        let (ran, says) = fx.run(COMMIT);
        assert_eq!(ran.status.code(), Some(1), "{text}: {says}");
        assert!(!says.contains("version="), "{text}: {says}");
        assert!(!fx.out().exists(), "{text}: wrote the output");
    }
    // The release holds no facts: the download of the pattern found nothing to copy.
    let dir = fx.root.path().join("gh/releases/0.0.13");
    std::fs::remove_file(dir.join(NAME)).expect("remove");
    let (ran, says) = fx.run(COMMIT);
    assert_eq!(ran.status.code(), Some(1), "{says}");
    assert!(!fx.out().exists());
    // A short commit is a usage error, as it is for every verb that takes `--commit`.
    let (ran, says) = fx.run("3aa7c93");
    assert_eq!(ran.status.code(), Some(2), "{says}");
}

impl Fx {
    /// The release holds `facts` and exactly these bytes as `SHA256SUMS`.
    fn release_with_sums(&self, facts: &str, sums: &[u8]) {
        let dir = self.root.path().join("gh/releases/0.0.13");
        std::fs::write(dir.join(NAME), facts).expect("facts");
        std::fs::write(dir.join("SHA256SUMS"), sums).expect("sums");
    }

    /// `run`, with `extra` arguments and environment variables added.
    fn run_with(&self, extra: &[&str], env: &[(&str, &str)]) -> (Output, String) {
        let ran = Command::new(XTASK)
            .args([
                "release",
                "farm-version",
                "--repo",
                "o/r",
                "--tag",
                "0.0.13",
            ])
            .args(["--commit", COMMIT, "--gh"])
            .arg(self.root.path().join("gh/gh"))
            .args(extra)
            .envs(env.iter().copied())
            .output()
            .expect("run");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let says = format!("{}{}", text(&ran.stdout), text(&ran.stderr));
        (ran, says)
    }

    fn refused(&self, ran: &Output, says: &str, why: &str) {
        assert_eq!(ran.status.code(), Some(1), "{says}");
        assert!(says.contains(why), "wanted {why:?} in {says}");
        assert!(!says.contains("version="), "{says}");
        assert!(!self.out().exists(), "wrote the output");
    }
}

#[test]
fn a_sums_file_that_does_not_list_the_facts_exactly_once_is_refused() {
    // Proves: SHA256SUMS naming other files only, or listing the facts twice (even with
    // the right digest), does not vouch for the facts. It would catch a verb that accepts
    // facts nothing lists, or the first of several lines.
    let fx = Fx::new("listing");
    let text = facts("0.1.0", COMMIT, None, "release");
    let digest = sha_of(text.as_bytes());
    let other = format!("{digest}  something-else.tar.gz\n");
    fx.release_with_sums(&text, other.as_bytes());
    let (ran, says) = fx.run(COMMIT);
    fx.refused(
        &ran,
        &says,
        "is not the file SHA256SUMS lists (listed 0 time(s))",
    );
    let twice = format!("{digest}  {NAME}\n{digest}  {NAME}\n");
    fx.release_with_sums(&text, twice.as_bytes());
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, "(listed 2 time(s))");
}

#[test]
fn a_sums_file_that_is_not_sums_is_refused_with_its_problem() {
    // Proves: a SHA256SUMS line that is not `<sha256>  <name>` ends the verb and names
    // the line, and bytes that are not UTF-8 are refused as such.
    let fx = Fx::new("sums-bad");
    let text = facts("0.1.0", COMMIT, None, "release");
    fx.release_with_sums(&text, b"garbage\n");
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, "SHA256SUMS: line 1:");
    fx.release_with_sums(&text, &[0xff, 0xfe, b'\n']);
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, "SHA256SUMS is not UTF-8");
}

#[test]
fn files_over_their_caps_and_a_missing_sums_file_are_refused() {
    // Proves: facts larger than the cap a build can print, a SHA256SUMS over 1 MiB, and a
    // release with no SHA256SUMS are each refused before anything is parsed or written.
    let fx = Fx::new("caps");
    let dir = fx.root.path().join("gh/releases/0.0.13");
    let big = "x".repeat(kbf_installer::binary::VERSION_CAP + 1);
    fx.release_with_sums(&big, b"");
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, "bytes");
    let text = facts("0.1.0", COMMIT, None, "release");
    fx.release_with_sums(&text, "a".repeat((1 << 20) + 1).as_bytes());
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, &format!("is over {} bytes", 1 << 20));
    std::fs::remove_file(dir.join("SHA256SUMS")).expect("remove");
    let (ran, says) = fx.run(COMMIT);
    fx.refused(&ran, &says, "read ");
}

#[test]
fn a_failing_or_missing_gh_ends_the_verb_with_its_message() {
    // Proves: a `gh release download` that exits non-zero is reported with gh's own words,
    // and a `gh` program that cannot be started is reported by its path; neither writes.
    let fx = Fx::new("gh-fails");
    let text = facts("0.1.0", COMMIT, None, "release");
    fx.release(&text, &text);
    let (ran, says) = fx.run_with(
        &["--output", fx.out().to_str().expect("utf-8")],
        &[("XTASK_FAKE_DEFECT", "release-fails")],
    );
    fx.refused(
        &ran,
        &says,
        "gh release download: xtask-fake: release failed",
    );
    let (ran, says) = Command::new(XTASK)
        .args([
            "release",
            "farm-version",
            "--repo",
            "o/r",
            "--tag",
            "0.0.13",
        ])
        .args(["--commit", COMMIT, "--gh", "/nonexistent/gh"])
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stderr).into_owned();
            (o, s)
        })
        .expect("run");
    assert_eq!(ran.status.code(), Some(1), "{says}");
    assert!(says.contains("/nonexistent/gh"), "{says}");
}

#[test]
fn the_version_is_printed_without_an_output_file_and_a_scratch_that_cannot_be_made_is_refused() {
    // Proves: without `--output` the line is printed and nothing else is written; with a
    // temporary directory that does not exist the verb says it cannot create its
    // directory and prints nothing.
    let fx = Fx::new("no-output");
    let text = facts("0.1.0", COMMIT, None, "release");
    fx.release(&text, &text);
    let (ran, says) = fx.run_with(&[], &[]);
    assert_eq!(ran.status.code(), Some(0), "{says}");
    assert!(says.contains("version=0.1.0+g3aa7c93274c5"), "{says}");
    assert!(!fx.out().exists());
    let (ran, says) = fx.run_with(&[], &[("TMPDIR", "/nonexistent/tmp")]);
    fx.refused(&ran, &says, "create ");
}

#[test]
fn an_output_file_that_cannot_be_appended_to_is_refused() {
    // Proves: when `--output` is a directory the append fails, and the verb exits 1 saying
    // so instead of printing a version no later step will read.
    let fx = Fx::new("output-fails");
    let text = facts("0.1.0", COMMIT, None, "release");
    fx.release(&text, &text);
    let dir = fx.root.path().join("a-directory");
    std::fs::create_dir(&dir).expect("dir");
    let (ran, says) = fx.run_with(&["--output", dir.to_str().expect("utf-8")], &[]);
    assert_eq!(ran.status.code(), Some(1), "{says}");
    assert!(says.contains("append to "), "{says}");
}
