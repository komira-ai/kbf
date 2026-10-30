//! `cargo xtask release version-facts`: it runs the `kbf` just built with `--version --json`
//! and nothing else, in an empty environment, and keeps the exact bytes it printed under the
//! name of the `kbf` tarball. `xtask-fake` is the `kbf`; what it prints is in files beside it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use sha2::Digest;
use xtask::package::Platform;
use xtask::version_facts::{Args, emit};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
/// A commit the binary may say it was built from instead.
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const PLATFORM: &str = "linux-x86_64";

/// The exit code, stdout and stderr of a run.
type Ran = (Option<i32>, String, String);

fn ran(out: &Output) -> Ran {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// What a `kbf` of `commit` and `profile` prints for `--version --json`, laid out as no
/// serializer lays it out: a field order that is not sorted, spaces after the colons and a
/// blank line at the end. The installer's parser takes it; a document parsed and written
/// again would not be these bytes.
fn says(commit: &str, profile: &str) -> String {
    format!(
        "{{ \"version\": \"0.1.0\", \"commit\": \"{}\",\n  \"profile\": \"{profile}\", \"protocol\": 1,\n  \"min_protocol\": 1, \"state_schema\": 1, \"meta_log\": 4, \"seq\": 812 }}\n\n",
        &commit[..12]
    )
}

/// A scratch directory with `bin/kbf` (the fake, linked: nothing this test wrote is run, so
/// no file that is run can be open for writing), the files beside it that make it print
/// what it is given, and the name of `dist`, where the facts go.
struct Fx {
    kbf: PathBuf,
    out: PathBuf,
    /// The directory all of it is in; last, so it is removed after the rest.
    root: TestDir,
}

fn fx(name: &str, says: &str) -> Fx {
    let root = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-version-facts"),
        name,
    );
    let bin = root.join("bin");
    std::fs::create_dir(&bin).expect("bin");
    let kbf = bin.join("kbf");
    std::fs::hard_link(FAKE, &kbf).expect("link the fake as kbf");
    std::fs::write(bin.join("kbf.stamp"), &COMMIT[..12]).expect("stamp");
    std::fs::write(bin.join("kbf.json"), says).expect("json");
    let out = root.join("dist");
    Fx { kbf, out, root }
}

/// `release version-facts` on the fixture's `kbf`, run as the job runs it, from a
/// process whose own environment holds a secret that must not reach the binary.
fn run(f: &Fx, commit: &str, platform: &str) -> Ran {
    let out = Command::new(XTASK)
        .args(["release", "version-facts", "--binary"])
        .arg(&f.kbf)
        .args(["--commit", commit, "--platform", platform, "--out"])
        .arg(&f.out)
        .env("KBF_TEST_SECRET", "not for the binary")
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    ran(&out)
}

/// `cargo xtask <args>`.
fn cargo_xtask(args: &[&str]) -> Ran {
    let out = Command::new(XTASK)
        .args(args)
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    ran(&out)
}

/// The names of the files in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir") {
        let entry = entry.expect("entry");
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    names
}

#[test]
fn it_keeps_the_exact_bytes_under_the_name_of_the_tarball_and_lists_them() {
    // Catches: facts written in another layout than the binary printed (a document parsed
    // and written again), under a name that is not the stem of the `kbf` tarball `package`
    // writes, or with no line in `SHA256SUMS.<platform>` (the sums job would refuse the
    // file), and a line that does not survive the `package` of the tarball after it, the
    // assembly of the sums and their check. Catches: the printed bytes replaced by a
    // re-serialisation; the name misspelled; the sums line not written.
    let said = says(COMMIT, "release");
    let f = fx("exact", &said);
    let (code, printed, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(0), "{err}");
    // The workflow packages `kbf` next: the tarball names the stem the facts must have.
    let sbom = f.root.join("kbf.cdx.json");
    std::fs::write(&sbom, "{}").expect("sbom");
    let package = [
        "package",
        "--binary",
        s(&f.kbf),
        "--sbom",
        s(&sbom),
        "--platform",
        PLATFORM,
        "--commit",
        COMMIT,
        "--out",
        s(&f.out),
    ];
    let (code, _, err) = cargo_xtask(&package);
    assert_eq!(code, Some(0), "{err}");
    let listed = names(&f.out);
    let tarball = listed.iter().find(|n| n.ends_with(".tar.gz"));
    let tarball = tarball.expect("the tarball package wrote");
    let stem = tarball.strip_suffix(".tar.gz").expect("a tarball");
    let facts = format!("{stem}.version.json");
    assert!(listed.contains(&facts), "{listed:?}");
    let kept = std::fs::read(f.out.join(&facts)).expect("facts");
    assert_eq!(kept, said.as_bytes());
    let sha = sha_of(said.as_bytes());
    assert_eq!(printed, format!("recorded {facts}:\n{sha}  {facts}\n"));
    let sums = f.out.join("SHA256SUMS.linux-x86_64");
    let part = std::fs::read_to_string(&sums).expect("the platform's sums");
    assert!(part.starts_with(&format!("{sha}  {facts}\n")), "{part}");
    let (code, _, err) = cargo_xtask(&["sums", "assemble", s(&f.out)]);
    assert_eq!(code, Some(0), "{err}");
    let (code, checked, err) = cargo_xtask(&["verify-assets", "sums", s(&f.out)]);
    assert_eq!(code, Some(0), "{err}");
    let want = "sums: 3 files match; the flipped copy does not\n";
    assert_eq!(checked, want);
}

/// Runs `release version-facts` on a `kbf` that prints `says`: it must be refused, with a
/// reason holding `why`, and leave nothing (not the directory, so no sums line either).
fn refuses(name: &str, says: &str, why: &str) {
    let f = fx(name, says);
    let (code, out, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{name}: {out}{err}");
    assert!(err.contains(why), "{name}: {err}");
    assert!(!f.out.exists(), "{name}: left {}", f.out.display());
}

#[test]
fn it_refuses_what_is_no_release_build_of_the_commit_and_writes_nothing() {
    // Catches: the answer of another commit's build or of a debug build, an answer that is
    // no JSON object, one without a field every build reports, and one over the cap, each
    // taken as the facts of this build; and a refusal that leaves a file, the directory or a
    // sums line behind. A field this xtask does not know is a newer build's and is kept (the
    // installer's reader ignores it; `binary_tests` pins that), so it is no refusal here.
    // Catches: the commit not compared; the profile not compared; a required field made
    // optional; the cap dropped.
    let ok = says(COMMIT, "release");
    let other = says(OTHER, "release");
    let debug = says(COMMIT, "debug");
    let missing = ok.replace("\"state_schema\": 1, ", "");
    let long = ok.replace("0.1.0", &"9".repeat(5000));
    refuses("commit", &other, "it says commit badbad012345");
    refuses("profile", &debug, "it says profile \"debug\"");
    refuses("not-json", "meta_log=3\n", "no JSON object");
    refuses("array", "[3]\n", "no JSON object");
    refuses("empty", "", "no JSON object");
    refuses("missing", &missing, "missing field");
    refuses("long", &long, "more than 4096 bytes");
}

#[test]
fn a_binary_that_exits_with_an_error_is_refused_whatever_it_printed() {
    // Catches: the exit status ignored because the output reads as facts. The fake prints
    // valid facts of the commit and then exits 1, so only the status can refuse it. Catches:
    // the status check dropped.
    let f = fx("fails", &says(COMMIT, "release"));
    std::fs::write(f.kbf.with_file_name("kbf.mode"), "fail").expect("mode");
    let (code, out, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains("it did not exit 0"), "{err}");
    assert!(!f.out.exists());
}

#[test]
fn a_binary_that_does_not_answer_is_killed_when_the_time_is_up() {
    // Catches: no time limit (the job would wait for the runner's limit), and a limit
    // that waits for the binary to end on its own. The fake sleeps for two minutes, so a
    // limit of nothing is the only way the run ends in time. Catches: the timeout of the
    // run ignored.
    let f = fx("hangs", &says(COMMIT, "release"));
    std::fs::write(f.kbf.with_file_name("kbf.mode"), "hang").expect("mode");
    let args = Args {
        binary: f.kbf.clone(),
        commit: COMMIT.to_owned(),
        platform: Platform::LinuxX86_64,
        out: f.out.clone(),
    };
    let refusal = emit(&args, Duration::ZERO).expect_err("a binary that hangs");
    let why = refusal.to_string();
    assert!(why.contains("did not answer in 0ns"), "{why}");
    assert!(!f.out.exists());
}

#[test]
fn the_binary_runs_with_an_empty_environment() {
    // Catches: the runner's environment (its tokens) handed to a binary the job has not yet
    // vouched for. The fake writes `ran` and then one line per variable it was given into
    // the file `kbf.env`, which holds nothing until it runs; this process has a secret of
    // its own. Catches: `env` filled from the caller's.
    let f = fx("env", &says(COMMIT, "release"));
    let log = f.kbf.with_file_name("kbf.env");
    std::fs::write(&log, "").expect("marker");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(0), "{err}");
    let seen = std::fs::read_to_string(&log).expect("env log");
    // Under coverage the instrumented child's own profiling runtime sets LLVM_PROFILE
    // variables; they are not inherited, so only they are ignored.
    let seen: Vec<&str> = seen
        .lines()
        .filter(|l| !l.starts_with("__LLVM_PROFILE") && !l.starts_with("LLVM_PROFILE"))
        .collect();
    assert_eq!(seen, ["ran"]);
    assert!(
        !seen.iter().any(|l| l.contains("KBF_TEST_SECRET")),
        "the caller's secret reached the binary: {seen:?}"
    );
}

#[test]
fn a_commit_that_is_not_full_or_a_platform_that_is_none_is_refused_before_the_binary_runs() {
    // Catches: a short or upper-case commit taken (the name of the facts would not be the
    // tarball's), a platform typo taken (a file nobody looks for), and the binary run
    // before either is checked (the marker would hold what the fake writes).
    let f = fx("usage", &says(COMMIT, "release"));
    let log = f.kbf.with_file_name("kbf.env");
    std::fs::write(&log, "").expect("marker");
    let upper = COMMIT.to_uppercase();
    for commit in ["abc", upper.as_str()] {
        let (code, _, err) = run(&f, commit, PLATFORM);
        assert_eq!(code, Some(2), "{commit}: {err}");
    }
    let (code, _, err) = run(&f, COMMIT, "freebsd-x86_64");
    assert_eq!(code, Some(2), "{err}");
    assert!(!f.out.exists());
    assert_eq!(std::fs::read_to_string(&log).expect("env log"), "");
}

#[test]
fn a_binary_that_is_missing_or_cannot_be_started_is_refused() {
    // Catches: a path that does not name a file read as success, and a file that is not
    // executable reported as an answer. Nothing is written for either.
    let f = fx("unstartable", &says(COMMIT, "release"));
    std::fs::remove_file(&f.kbf).expect("remove the link");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("find "), "{err}");
    std::fs::write(&f.kbf, "not a program").expect("a plain file");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("run "), "{err}");
    assert!(err.contains("Permission denied"), "{err}");
    assert!(!f.out.exists());
}

#[test]
fn an_out_that_is_a_file_is_refused_and_left_alone() {
    // Catches: a directory that cannot be made taken as made (the facts would go nowhere
    // or the error be lost), and the file in the way touched.
    let f = fx("out-is-a-file", &says(COMMIT, "release"));
    std::fs::write(&f.out, "in the way").expect("a file");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("create "), "{err}");
    let kept = std::fs::read_to_string(&f.out).expect("file");
    assert_eq!(kept, "in the way");
}

#[test]
fn it_replaces_nothing_and_lists_nothing_twice() {
    // Catches: a second run that replaces the facts or lists them twice; a file in the way
    // that the sums do not list replaced. Catches: `create_new` replaced by a plain write;
    // the check of the sums file dropped (the first half would then name the write).
    let f = fx("twice", &says(COMMIT, "release"));
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(0), "{err}");
    let sums = f.out.join("SHA256SUMS.linux-x86_64");
    let first = std::fs::read_to_string(&sums).expect("sums");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("already lists"), "{err}");
    assert_eq!(std::fs::read_to_string(&sums).expect("sums"), first);
    std::fs::remove_file(&sums).expect("remove the sums");
    let (code, _, err) = run(&f, COMMIT, PLATFORM);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("write "), "{err}");
    assert!(!sums.exists(), "a refused run wrote the sums");
}
