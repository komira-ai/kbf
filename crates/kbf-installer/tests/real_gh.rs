//! The offline attestation check against the real, pinned `gh` and a real attestation.
//!
//! The fixture is the build provenance of an earlier build of this repository's artifacts
//! workflow on `refs/heads/feature/arch-v2` (commit `0f4a3825...`): the bundle `gh`
//! downloaded for it (`testdata/fixture_provenance.jsonl`), the SBOM that attestation
//! lists as a subject (`testdata/fixture_subject.cdx.json.gz`, compressed so the
//! repository's text lints, which would read the build paths inside it, skip it), and the
//! Sigstore trusted root of the day (`testdata/trusted_root.jsonl`). Nothing is fetched:
//! the verifier runs `gh` through a proxy that refuses every connection, so a pass is an
//! offline verdict.
//!
//! What this shows that the fake `gh` of the unit tests cannot: that the argument list
//! the verifier builds is one the real `gh` accepts and verifies against, that it is the
//! pinned workflow's attestation that passes, and that one changed byte, another commit
//! or another branch, repository or workflow does not.
//!
//! Ignored in the plain test run: the pinned `gh` is not in the repository. CI fetches it
//! (`cargo xtask fetch`, pinned by sha256) and runs these with `--ignored
//! --test-threads=1`, naming the binary in `KBF_TEST_GH`; run without it, each test fails
//! rather than passes. The CI step also fails if no test ran: the last of them,
//! `zz_all_the_real_gh_tests_ran`, fails unless the four before it finished, and
//! `the_real_gh_tests_are_present` (not ignored, so every job runs it) counts the ignored
//! tests in this file.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;
use kbf_installer::CommitId;
use kbf_installer::attest::{SourceRef, Tools, Verifier};
use kbf_installer::run::{RunAs, SystemRunner};

/// The commit the fixture attests, and the branch it was built on.
const COMMIT: &str = "0f4a3825305cc79733ef098eae9725948eb5d6db";
const REF: &str = "refs/heads/feature/arch-v2";
/// The version of `gh` CI pins.
const GH_VERSION: &str = "2.102.0";
/// How many real-`gh` tests finished, read by `zz_all_the_real_gh_tests_ran`.
static FINISHED: AtomicUsize = AtomicUsize::new(0);
/// The real-`gh` tests that run before `zz_all_the_real_gh_tests_ran`.
const REAL_TESTS: usize = 4;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

/// The pinned `gh` CI fetched.
fn gh() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_GH")
        .expect("KBF_TEST_GH names the pinned gh (CI fetches it with cargo xtask fetch)");
    PathBuf::from(path)
}

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = kbf_host::testkit::scratch(&format!("installer-real-gh-{name}"));
    fs::create_dir_all(dir.join("home")).expect("home");
    dir
}

/// The fixture's subject, unpacked into `dir`.
fn subject(dir: &Path) -> PathBuf {
    let mut bytes = Vec::new();
    let file = fs::File::open(testdata("fixture_subject.cdx.json.gz")).expect("fixture");
    flate2::read::GzDecoder::new(file)
        .read_to_end(&mut bytes)
        .expect("a gzip");
    let path = dir.join("subject.cdx.json");
    fs::write(&path, bytes).expect("subject");
    path
}

fn host_path(path: &Path) -> HostPath {
    HostPath::new(path.to_str().expect("utf-8")).expect("an absolute path")
}

/// A host whose answer is that `gh`, the trusted root and every directory above them are
/// root's: in this test they are the CI user's, and the trust check has its own tests.
fn trusting(paths: &[&Path]) -> FakeExec {
    let mut exec = FakeExec::new(0);
    let root = Owner { uid: 0, gid: 0 };
    for path in paths {
        exec.entries.insert(
            host_path(path),
            Entry::File {
                perms: Perms::new(0o755),
                owner: root,
                bytes: Vec::new(),
            },
        );
        for dir in path.ancestors().skip(1) {
            if dir != Path::new("/") {
                exec.entries.insert(
                    host_path(dir),
                    Entry::Dir {
                        perms: Perms::new(0o755),
                        owner: root,
                    },
                );
            }
        }
    }
    exec
}

/// The verifier for the fixture and the real `gh`, with `refs` as the allowed branches;
/// `change` edits it (another repository, workflow).
fn verify(
    dir: &Path,
    file: &Path,
    commit: &str,
    refs: &[&str],
    change: impl FnOnce(&mut Verifier),
) -> Result<kbf_installer::attest::Verified, kbf_installer::refusal::Refusal> {
    let mut verifier = Verifier {
        root: Root::host(),
        gh: host_path(&gh()),
        trusted_root: host_path(&testdata("trusted_root.jsonl")),
        // The fixture bundle was attested under the repository's former name.
        signers: vec![kbf_installer::attest::Signer::former()],
        issuer: kbf_installer::attest::ISSUER.to_owned(),
        source_refs: refs
            .iter()
            .map(|r| SourceRef::new(r).expect("ref"))
            .collect(),
        run_as: RunAs {
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
        },
        timeout: Duration::from_secs(120),
    };
    change(&mut verifier);
    let trust = trusting(&[
        Path::new(verifier.gh.as_str()),
        Path::new(verifier.trusted_root.as_str()),
    ]);
    let tools = Tools {
        trust: &trust,
        runner: &SystemRunner,
    };
    verifier.verify(
        &tools,
        &dir.join("home"),
        file,
        &testdata("fixture_provenance.jsonl"),
        &CommitId::new(commit).expect("commit"),
    )
}

/// Catches `gh` being a different release than the one pinned (the argument list and the
/// verification are what that release does).
#[test]
#[ignore = "needs the pinned gh: KBF_TEST_GH"]
fn the_gh_under_test_is_the_pinned_release() {
    let out = std::process::Command::new(gh())
        .arg("--version")
        .output()
        .expect("gh runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(&format!("gh version {GH_VERSION} ")),
        "the pinned gh is {GH_VERSION}: {text}"
    );
    FINISHED.fetch_add(1, Ordering::SeqCst);
}

/// Catches an argument list the real `gh` does not accept, or a verification that does not
/// reach the attestation: the SBOM, with its bundle and the pinned repository, workflow,
/// issuer, ref, commit and predicate, is attested, offline, on hosted runners.
#[test]
#[ignore = "needs the pinned gh: KBF_TEST_GH"]
fn the_real_gh_accepts_the_real_attestation() {
    let dir = scratch("good");
    let file = subject(&dir);
    let verified = verify(&dir, &file, COMMIT, &[REF], |_| {}).expect("attested");
    assert_eq!(verified.source_ref.as_str(), REF);
    // The allowed refs are tried in order: the one the build was on is found second.
    let verified =
        verify(&dir, &file, COMMIT, &["refs/heads/main", REF], |_| {}).expect("attested");
    assert_eq!(verified.source_ref.as_str(), REF);
    FINISHED.fetch_add(1, Ordering::SeqCst);
}

/// Catches a verification that accepts what it should not. One changed byte of the
/// subject, another commit, another branch (and a pull request's merge ref), another
/// repository and another workflow are each refused, with `gh` as the judge.
///
/// Mutants seen red: `--source-digest` dropped (the other commit is accepted);
/// `--source-ref` dropped (the other branch is accepted).
#[test]
#[ignore = "needs the pinned gh: KBF_TEST_GH"]
fn the_real_gh_refuses_what_the_attestation_does_not_cover() {
    let dir = scratch("bad");
    let file = subject(&dir);
    let mut changed = fs::read(&file).expect("subject");
    changed.push(b'x');
    let changed_file = dir.join("changed.cdx.json");
    fs::write(&changed_file, changed).expect("changed");

    let code = |r: Result<_, kbf_installer::refusal::Refusal>| match r {
        Ok(_) => "accepted".to_owned(),
        Err(e) => e.code().to_owned(),
    };
    assert_eq!(
        code(verify(&dir, &changed_file, COMMIT, &[REF], |_| {})),
        "not-attested"
    );
    let other_commit = "4b41accf336d9529cf53b1aed31d7a30cf96e744";
    assert_eq!(
        code(verify(&dir, &file, other_commit, &[REF], |_| {})),
        "not-attested"
    );
    assert_eq!(
        code(verify(&dir, &file, COMMIT, &["refs/heads/main"], |_| {})),
        "not-attested"
    );
    assert_eq!(
        code(verify(
            &dir,
            &file,
            COMMIT,
            &["refs/heads/feature/other"],
            |_| {}
        )),
        "not-attested"
    );
    assert_eq!(
        code(verify(&dir, &file, COMMIT, &[REF], |v| {
            v.signers[0].repo = "komira-ai/komira".to_owned();
        })),
        "not-attested"
    );
    assert_eq!(
        code(verify(&dir, &file, COMMIT, &[REF], |v| {
            v.signers[0].workflow =
                "komira-ai/komira-build-farm/.github/workflows/ci.yml".to_owned();
        })),
        "not-attested"
    );
    // A file that is not a subject of the attestation (the bundle itself).
    assert_eq!(
        code(verify(
            &dir,
            &testdata("trusted_root.jsonl"),
            COMMIT,
            &[REF],
            |_| {}
        )),
        "not-attested"
    );
    FINISHED.fetch_add(1, Ordering::SeqCst);
}

/// Catches an empty or a missing trusted root being read as acceptance: `gh` has nothing
/// to verify the certificate chain against, and the verdict is a refusal.
#[test]
#[ignore = "needs the pinned gh: KBF_TEST_GH"]
fn the_real_gh_refuses_an_empty_or_missing_trusted_root() {
    let dir = scratch("root");
    let file = subject(&dir);
    let empty = dir.join("empty_root.jsonl");
    fs::write(&empty, b"").expect("empty");
    let missing = dir.join("none.jsonl");
    for root in [empty, missing] {
        let result = verify(&dir, &file, COMMIT, &[REF], |v| {
            v.trusted_root = host_path(&root);
        });
        assert_eq!(result.expect_err("refused").code(), "not-attested");
    }
    FINISHED.fetch_add(1, Ordering::SeqCst);
}

/// Fails the `--ignored` run unless the four real-`gh` tests ran before it (it sorts last,
/// and the run is single-threaded): a rename, a filter or an edit that leaves the step
/// running nothing is red, not green.
#[test]
#[ignore = "needs the pinned gh: KBF_TEST_GH"]
fn zz_all_the_real_gh_tests_ran() {
    assert_eq!(
        FINISHED.load(Ordering::SeqCst),
        REAL_TESTS,
        "the real-gh tests that finished before this one"
    );
}

/// Counts the ignored tests in this file, so that the CI step that runs them with
/// `--ignored` cannot pass on zero: an edit that removes or un-ignores them fails here, in
/// the plain run.
#[test]
fn the_real_gh_tests_are_present() {
    let source = include_str!("real_gh.rs");
    let marker = concat!("#[", "ignore");
    let ignored = source
        .lines()
        .filter(|l| l.trim_start().starts_with(marker))
        .count();
    assert_eq!(ignored, REAL_TESTS + 1, "the ignored tests of this file");
}
