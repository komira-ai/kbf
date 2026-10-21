//! The bundle `release stage-offer` hands a deploy ([`build_bundle`]), its caps and platform
//! lists held to the installer's, and the run of `kbf --version --json` the verb makes
//! ([`version_json`]). Both are called directly: a binary that never answers or floods its
//! pipe needs a short wait, which the verb does not take as a flag.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kbf_installer::deploy::bundle as installer;
use kbf_installer::deploy::offer as installer_offer;
use kbf_testkit::dir::TestDir;
use kbf_types::ReleasePlatform;
use xtask::Error;
use xtask::release::stage_offer::{
    MAX_ATTESTATION, MAX_FACTS, MAX_OFFER, MAX_SUMS, MAX_TARBALL, build_bundle, version_json,
};

mod support;
use support::{BUSY_WAIT, output_unbusy};
#[path = "support/release_fake.rs"]
mod release_fake;
use release_fake::{entries_of, identity, kbf_bytes};

const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OFFER: &[u8] = b"the offer\n";
/// A wait no test expects to reach.
const LONG: Duration = Duration::from_secs(30);

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-stage-offer-bundle");
    TestDir::under(root, name)
}

/// The release files `build_bundle` reads: `SHA256SUMS`, and for each platform its version
/// facts, and for each component of it the tarball and the bundle beside it, each holding
/// bytes that name it.
fn release_dir(name: &str) -> TestDir {
    let dir = scratch(name);
    std::fs::write(dir.join("SHA256SUMS"), b"the sums\n").expect("sums");
    for platform in ReleasePlatform::ALL {
        let facts = format!("kbf-{SHA}-{platform}.version.json");
        std::fs::write(dir.join(&facts), format!("{facts} bytes")).expect("facts");
        for component in platform.components() {
            let tarball = format!("{component}-{SHA}-{platform}.tar.gz");
            std::fs::write(dir.join(&tarball), format!("{tarball} bytes")).expect("tarball");
            let bundle = dir.join(format!("{tarball}.sigstore.json"));
            std::fs::write(bundle, format!("{tarball} bundle")).expect("bundle");
        }
    }
    dir
}

/// The names of the entries of a bundle of every platform, in order.
fn expected_names() -> Vec<String> {
    let mut names = vec!["SHA256SUMS".to_owned(), "offer.json".to_owned()];
    for platform in ReleasePlatform::ALL {
        names.push(format!("kbf-{SHA}-{platform}.version.json"));
        for component in platform.components() {
            let tarball = format!("{component}-{SHA}-{platform}.tar.gz");
            names.push(tarball.clone());
            names.push(format!("{tarball}.attestation"));
        }
    }
    names
}

#[test]
fn the_caps_and_the_platform_and_component_lists_are_the_installers() {
    // Catches: a cap that differs from the installer's reader (a bundle the reader refuses,
    // found at a deploy), a platform the installer's store does not know, or a platform it
    // knows that a release is never staged for, and a component it does not take.
    // Mutant it catches: `MAX_FACTS` here set to a value other than the installer's.
    assert_eq!(MAX_TARBALL, installer::MAX_TARBALL);
    assert_eq!(MAX_ATTESTATION, installer::MAX_ATTESTATION);
    assert_eq!(MAX_FACTS, installer::MAX_FACTS);
    assert_eq!(MAX_SUMS, installer::MAX_SUMS);
    assert_eq!(MAX_OFFER, installer_offer::MAX_OFFER);
    let all = ReleasePlatform::ALL;
    let mut platforms: Vec<&str> = all.map(ReleasePlatform::as_str).to_vec();
    platforms.sort_unstable();
    let mut known = installer::PLATFORMS.to_vec();
    known.sort_unstable();
    assert_eq!(platforms, known);
    let mut components: Vec<&str> = Vec::new();
    for platform in all {
        components.extend(platform.components());
    }
    // Every component a release stages is one the installer takes. Not the reverse: the
    // installer may take one (`kbf-daemon`) that no platform of the release stages yet.
    for component in components {
        assert!(installer::COMPONENTS.contains(&component), "{component}");
    }
}

#[test]
fn the_bundle_is_the_sums_the_offer_and_each_platforms_facts_and_tarballs_in_that_order() {
    // Catches: SHA256SUMS or the offer left out or out of place, a platform's version facts
    // left out, changed or taken from another platform's file (each file's bytes name it), a
    // tarball without its attestation or with another's (each bundle's bytes name its own
    // tarball), an SBOM or a bundle under its release name in the tar, a header that holds a
    // time, an owner or a mode that can differ from run to run, and an entry order that
    // follows the directory's.
    // Mutant it catches: a platform's facts entry left out of `build_bundle`, or put after
    // its tarballs.
    let dir = release_dir("layout");
    let bytes = build_bundle(&dir, SHA, &ReleasePlatform::ALL, OFFER).expect("a bundle");
    let entries = entries_of(&bytes);
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, expected_names());
    for e in &entries {
        let header = (e.mode, e.uid, e.gid, e.mtime);
        assert_eq!(header, (0o644, 0, 0, 0), "{}", e.name);
        let want: Vec<u8> = match e.name.as_str() {
            "SHA256SUMS" => b"the sums\n".to_vec(),
            "offer.json" => OFFER.to_vec(),
            name => match name.strip_suffix(".attestation") {
                Some(tarball) => format!("{tarball} bundle").into_bytes(),
                None => format!("{name} bytes").into_bytes(),
            },
        };
        assert_eq!(e.bytes, want, "{}", e.name);
    }
    // The same inputs make the same bytes.
    let again = build_bundle(&dir, SHA, &ReleasePlatform::ALL, OFFER).expect("a bundle");
    assert_eq!(bytes, again);
}

#[test]
fn only_the_platforms_given_are_in_the_bundle() {
    // Catches: every platform packed whatever the offer found complete: a platform the offer
    // left out has files the release may not hold, and the host would be handed them.
    // Mutant it catches: the facts of every platform read, not those of the platforms given.
    let dir = release_dir("subset");
    let linux = [ReleasePlatform::LinuxArm64];
    let bytes = build_bundle(&dir, SHA, &linux, OFFER).expect("a bundle");
    let entries = entries_of(&bytes);
    let names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
    let facts = format!("kbf-{SHA}-linux-arm64.version.json");
    let tarball = format!("kbf-{SHA}-linux-arm64.tar.gz");
    let attestation = format!("{tarball}.attestation");
    let want = [
        "SHA256SUMS",
        "offer.json",
        facts.as_str(),
        tarball.as_str(),
        attestation.as_str(),
    ];
    assert_eq!(names, want);
}

/// Sets the length of the file `path`: a hole when it grows, so no disk is used.
fn set_len(path: &Path, len: u64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open");
    file.set_len(len).expect("set_len");
}

/// `build_bundle` over a release in which `file` is `len` bytes, with an offer of `offer`
/// bytes.
fn built(name: &str, file: &str, len: u64, offer: usize) -> Result<Vec<u8>, Error> {
    let dir = release_dir(name);
    set_len(&dir.join(file), len);
    let body = vec![b'o'; offer];
    build_bundle(&dir, SHA, &ReleasePlatform::ALL, &body)
}

/// The message of the `Failed` that `made` is.
fn refused(made: Result<Vec<u8>, Error>) -> String {
    match made {
        Err(Error::Failed(why)) => why,
        Err(other) => panic!("another error: {other}"),
        Ok(_) => panic!("a bundle was made"),
    }
}

#[test]
fn a_file_larger_than_the_installer_takes_is_refused_and_one_at_the_cap_is_not() {
    // Catches: no cap (a bundle the installer's reader refuses after the job has run its
    // checks), a cap off by one either way, and a tarball read before its size is judged
    // (the one here is a hole of 512 MiB and a byte).
    // Mutant it catches: the facts cap unchecked, or checked with `>=` in place of `>`.
    let sums = "SHA256SUMS";
    built("sums-at", sums, MAX_SUMS, 1).expect("SHA256SUMS at the cap");
    let over = MAX_SUMS + 1;
    let why = refused(built("sums-over", sums, over, 1));
    assert!(why.contains(sums), "{why}");

    let attestation = format!("kbf-{SHA}-linux-x86_64.tar.gz.sigstore.json");
    built("att-at", &attestation, MAX_ATTESTATION, 1).expect("an attestation at the cap");
    let over = MAX_ATTESTATION + 1;
    let why = refused(built("att-over", &attestation, over, 1));
    assert!(why.contains(&attestation), "{why}");

    let tarball = format!("kbf-{SHA}-linux-x86_64.tar.gz");
    let over = MAX_TARBALL + 1;
    let why = refused(built("tar-over", &tarball, over, 1));
    assert!(why.contains(&tarball), "{why}");

    let facts = format!("kbf-{SHA}-linux-x86_64.version.json");
    built("facts-at", &facts, MAX_FACTS, 1).expect("facts at the cap");
    let over = MAX_FACTS + 1;
    let why = refused(built("facts-over", &facts, over, 1));
    assert!(why.contains(&facts), "{why}");

    let cap = MAX_OFFER as usize;
    built("offer-at", sums, 9, cap).expect("an offer at the cap");
    let why = refused(built("offer-over", sums, 9, cap + 1));
    assert!(why.contains("offer.json"), "{why}");
}

#[test]
fn a_file_the_release_lacks_is_an_error_that_names_it() {
    // Catches: a missing attestation left out of the bundle instead of failing it (the
    // installer would take a tarball with none and refuse it at the host), and missing
    // version facts left out (the store would keep the platform and never offer it).
    // Mutant it catches: a missing facts file skipped in place of failing the bundle.
    let gone = [
        format!("kbf-{SHA}-linux-arm64.tar.gz.sigstore.json"),
        format!("kbf-{SHA}-linux-arm64.version.json"),
    ];
    for (n, name) in gone.iter().enumerate() {
        let dir = release_dir(&format!("missing-{n}"));
        std::fs::remove_file(dir.join(name)).expect("remove");
        let made = build_bundle(&dir, SHA, &ReleasePlatform::ALL, OFFER);
        let why = made.expect_err("a refusal").to_string();
        assert!(why.contains(name), "{why}");
    }
}

/// A `kbf` in `dir` that answers `json` to `--version --json`, free to run.
fn kbf_in(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("kbf");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&path)
        .expect("kbf");
    let bytes = kbf_bytes("c0ffee012345", json);
    file.write_all(&bytes).expect("write kbf");
    drop(file);
    // A file just written meets ETXTBSY if another thread forked while it was open: run it
    // once, waiting for that to pass.
    let probe = output_unbusy(Command::new(&path).arg("--version"), BUSY_WAIT).expect("run");
    assert!(probe.status.success());
    path
}

/// What `version_json` made of a `kbf` that answers `json`.
struct Asked {
    result: Result<(), Error>,
    /// The file it was to write.
    out: PathBuf,
    /// Last field: the directory is removed with it.
    dir: TestDir,
}

fn asked(name: &str, json: &str, wait: Duration) -> Asked {
    let dir = scratch(name);
    let kbf = kbf_in(&dir, json);
    let out = dir.join("version.json");
    let result = version_json(&kbf, &out, wait);
    Asked { result, out, dir }
}

#[test]
fn the_identity_a_kbf_prints_is_written_to_the_file() {
    // Catches: the answer not written, or written changed.
    let a = asked("identity", &identity(SHA, 4), LONG);
    a.result.expect("the identity");
    let got = std::fs::read_to_string(&a.out).expect("out");
    assert_eq!(got, identity(SHA, 4));
}

#[test]
fn the_binary_runs_with_no_environment_and_in_its_own_directory() {
    // Catches: the job's environment (the token the job holds among it) handed to a binary
    // it runs, and a binary run in the job's working directory. The control is the same
    // binary started the way a test starts one, which sees what it is given.
    let dir = scratch("env");
    let kbf = kbf_in(&dir, "env\n");
    let mut control = Command::new(&kbf);
    control.args(["--version", "--json"]);
    control.env("CANARY", "1");
    let seen = output_unbusy(&mut control, BUSY_WAIT).expect("run the control");
    let seen = String::from_utf8_lossy(&seen.stdout).into_owned();
    assert!(seen.contains("CANARY=1"), "{seen}");
    let out = dir.join("version.json");
    version_json(&kbf, &out, LONG).expect("the answer");
    let home = dir.canonicalize().expect("the directory");
    let want = format!("cwd={}\n", home.display());
    let got = std::fs::read_to_string(&out).expect("out");
    // Under llvm-cov the instrumented child's own profiling runtime sets `__LLVM_PROFILE_*`
    // variables in it; they are not inherited, so only the runtime's own are let through.
    let kept: String = got
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("__LLVM_PROFILE") && !l.starts_with("LLVM_PROFILE"))
        .collect();
    assert_eq!(kept, want, "{got}");
    // The control's canary must not have reached the binary.
    assert!(!got.contains("CANARY"), "{got}");
}

#[test]
fn a_binary_that_fails_is_an_error_and_writes_nothing() {
    // Catches: a failed run read as an answer, and a file left behind.
    let a = asked("fails", "fail\n", LONG);
    let why = a.result.expect_err("a failure").to_string();
    assert!(why.contains("exit status"), "{why}");
    assert!(!a.out.exists());
    assert!(a.dir.exists());
}

#[test]
fn a_binary_that_never_answers_is_killed_after_the_wait() {
    // Catches: no wait limit (the job would hang with the credentials it holds), and an
    // answer taken from a run that was cut short.
    let start = Instant::now();
    let a = asked("hangs", "hang\n", Duration::from_millis(300));
    let why = a.result.expect_err("a timeout").to_string();
    assert!(why.contains("no answer within"), "{why}");
    assert!(start.elapsed() < LONG, "waited {:?}", start.elapsed());
    assert!(!a.out.exists());
    assert!(a.dir.exists());
}

#[test]
fn a_process_left_holding_the_output_does_not_hold_the_run() {
    // Catches: a join on the reader of the output, which a process the binary left running
    // (it exits 0 at once, and the process holds the pipe for 3 s) holds for as long as it
    // lives: the run must end with an error within the wait, not with the process.
    let start = Instant::now();
    let a = asked("orphan", "orphan\n", Duration::from_millis(300));
    let waited = start.elapsed();
    let why = a.result.expect_err("an open output").to_string();
    assert!(why.contains("its output did not end"), "{why}");
    assert!(waited < Duration::from_secs(2), "{waited:?}");
    assert!(!a.out.exists());
}

#[test]
fn an_answer_over_the_cap_is_an_error_and_one_at_the_cap_is_not() {
    // Catches: no cap (a binary could fill the job's memory), a cap off by one either way:
    // the cap is 8192 bytes, and the answers here are 8192 and 8193.
    let at = format!("{}\n", "x".repeat(8191));
    let a = asked("at-cap", &at, LONG);
    a.result.expect("an answer at the cap");
    assert_eq!(std::fs::read_to_string(&a.out).expect("out"), at);
    let over = format!("{}\n", "x".repeat(8192));
    let a = asked("over-cap", &over, LONG);
    let why = a.result.expect_err("over the cap").to_string();
    assert!(why.contains("more than 8192 bytes"), "{why}");
    assert!(!a.out.exists());
    assert!(a.dir.exists());
}

#[test]
fn a_binary_that_is_not_there_or_a_file_that_cannot_be_written_is_an_error() {
    // Catches: a missing binary or an unwritable out reported as an answer.
    let dir = scratch("missing-binary");
    let nope = dir.join("nope");
    let out = dir.join("version.json");
    let failed = version_json(&nope, &out, LONG);
    let why = failed.expect_err("no binary").to_string();
    assert!(why.contains("nope"), "{why}");
    let kbf = kbf_in(&dir, &identity(SHA, 4));
    let no_dir = dir.join("no-such-dir/version.json");
    let failed = version_json(&kbf, &no_dir, LONG);
    let why = failed.expect_err("no file").to_string();
    assert!(why.contains("write "), "{why}");
}
