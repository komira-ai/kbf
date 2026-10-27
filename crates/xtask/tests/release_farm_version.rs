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
