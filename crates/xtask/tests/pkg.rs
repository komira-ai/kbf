//! `cargo xtask pkg build|cask` run as CI runs them, with `xtask-fake` copied
//! as `kbf-installer`, the stub of the real tool, as the binary to package.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;
use kbf_testkit::dir::TestDir;
use support::{output_unbusy, output_unbusy_with};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const PLIST: &str = "../../packaging/macos/com.komira.kbf-installer.plist";
const URL: &str = "https://github.com/komira-ai/kbf/releases/download/v1/x";

/// An empty directory of its own for one test, holding the stub as `kbf-installer`.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pkg"),
        name,
    );
    std::fs::copy(FAKE, dir.join("kbf-installer")).expect("the stub");
    dir
}

fn plist() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(PLIST)
}

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out: Output = Command::new(XTASK).args(args).output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn pkg_build(dir: &Path, out: &Path, extra: &[&str]) -> (Option<i32>, String, String) {
    let binary = dir.join("kbf-installer");
    let plist = plist();
    let mut args = vec![
        "pkg",
        "build",
        "--binary",
        s(&binary),
        "--launchd-plist",
        s(&plist),
        "--version",
        "0.1.0",
        "--out",
        s(out),
    ];
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn the_stub_prints_its_arguments_on_both_outputs_and_exits_0() {
    let dir = scratch("stub");
    let out = output_unbusy(Command::new(dir.join("kbf-installer")).args(["serve", "--version"]))
        .expect("run the stub");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"kbf-installer-stub serve --version\n");
    assert_eq!(out.stderr, b"kbf-installer-stub serve --version\n");
}

/// The stub with a descriptor open for writing on it, as a forked child leaves it:
/// running it is `ETXTBSY` until the descriptor is closed.
fn held_open(name: &str) -> (PathBuf, std::fs::File, TestDir) {
    let own = scratch(name);
    let stub = own.join("kbf-installer");
    let held = std::fs::OpenOptions::new()
        .append(true)
        .open(&stub)
        .expect("open the stub for writing");
    (stub, held, own)
}

#[test]
fn a_stub_held_open_for_writing_is_busy_to_a_plain_run() {
    // Catches: the tests below passing on a file that is not busy at all, which
    // would prove nothing about the retry.
    let (stub, _held, _own) = held_open("busy-plain");
    let err = Command::new(&stub).output().expect_err("the stub is busy");
    assert_eq!(err.kind(), io::ErrorKind::ExecutableFileBusy, "{err}");
}

#[test]
fn a_busy_stub_runs_once_the_descriptor_closes() {
    // Catches: a run that gives up on the first ETXTBSY (the coverage job's failure), and
    // a run that returns before the stub was free. The descriptor is closed by the
    // first pause between two tries, so the run must pause, try again and then succeed.
    let (stub, held, _own) = held_open("busy-closes");
    let mut held = Some(held);
    let mut pauses = 0;
    let out = output_unbusy_with(Command::new(&stub).arg("late"), || {
        pauses += 1;
        drop(held.take());
    })
    .expect("run once free");
    assert_eq!(pauses, 1, "one pause, then the free stub ran");
    assert_eq!(out.stdout, b"kbf-installer-stub late\n");
}

#[test]
fn an_error_that_is_not_busy_is_returned_at_once() {
    // Catches: a retry on any error (a missing program would be started again and
    // again), which only the busy case may do.
    let _missing_own = scratch("not-busy");
    let missing = _missing_own.join("no-such-program");
    let mut pauses = 0;
    let err = output_unbusy_with(&mut Command::new(&missing), || pauses += 1)
        .expect_err("no such program");
    assert_eq!(pauses, 0, "retried a program that is not busy");
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

#[test]
fn pkg_build_writes_the_package_and_names_its_sum_and_is_the_same_twice() {
    let a = scratch("build-a");
    let b = scratch("build-b");
    let trust = a.join("trusted_root.jsonl");
    std::fs::write(&trust, b"{}\n").unwrap();
    let (code, stdout, stderr) = pkg_build(&a, &a.join("out"), &["--trust", s(&trust)]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""), "{stdout}");
    let name = "kbf-installer-0.1.0-macos-arm64.pkg";
    let pkg = std::fs::read(a.join("out").join(name)).unwrap();
    assert_eq!(&pkg[..4], b"xar!");
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some(format!("packaged {name}:").as_str()));
    let sum = lines.next().unwrap();
    assert!(
        sum.ends_with(&format!("  {name}")) && sum.len() == 64 + 2 + name.len(),
        "{sum}"
    );
    // Another directory and copy of the stub, the same trust file: the same bytes.
    let trust_b = b.join("trusted_root.jsonl");
    std::fs::write(&trust_b, b"{}\n").unwrap();
    let (code, _, stderr) = pkg_build(&b, &b.join("out"), &["--trust", s(&trust_b)]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(pkg, std::fs::read(b.join("out").join(name)).unwrap());
    // It will not replace the package.
    let (code, _, stderr) = pkg_build(&a, &a.join("out"), &["--trust", s(&trust)]);
    assert_eq!(code, Some(1));
    assert!(stderr.starts_with("xtask: write "), "{stderr}");
}

#[test]
fn pkg_build_is_a_usage_error_for_a_bad_version_and_a_refusal_for_an_ungated_job() {
    let dir = scratch("refuse");
    let out = dir.join("out");
    let binary = dir.join("kbf-installer");
    let good = plist();
    let args = |plist: &Path, version: &'static str| {
        run(&[
            "pkg",
            "build",
            "--binary",
            s(&binary),
            "--launchd-plist",
            s(plist),
            "--version",
            version,
            "--out",
            s(&out),
        ])
    };
    let (code, _, stderr) = args(&good, "1 2");
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("is not a version"), "{stderr}");
    let ungated = dir.join("job.plist");
    let text = std::fs::read_to_string(&good).unwrap();
    std::fs::write(
        &ungated,
        text.replace(
            "<key>RunAtLoad</key>\n\t<false/>",
            "<key>RunAtLoad</key>\n\t<true/>",
        ),
    )
    .unwrap();
    let (code, _, stderr) = args(&ungated, "1");
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("does not say the job is not started at load"),
        "{stderr}"
    );
    assert!(!out.exists());
}

#[test]
fn pkg_cask_writes_a_file_naming_the_sum_of_what_it_installs() {
    let dir = scratch("brew");
    let name = "kbf-installer-1.0.0-macos-arm64.pkg";
    std::fs::write(dir.join(name), b"pkg bytes").unwrap();
    let cask = dir.join("cask.rb");
    let pkg = dir.join(name);
    let (code, stdout, stderr) = run(&[
        "pkg",
        "cask",
        "--pkg",
        s(&pkg),
        "--version",
        "1.0.0",
        "--url",
        URL,
        "--out",
        s(&cask),
    ]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""), "{stdout}");
    let text = std::fs::read_to_string(&cask).unwrap();
    assert!(
        text.contains(&format!("sha256 \"{}\"", sha256_of(b"pkg bytes"))),
        "{text}"
    );
    // A name that is not the version's package.
    let (code, _, stderr) = run(&[
        "pkg",
        "cask",
        "--pkg",
        s(&pkg),
        "--version",
        "2.0.0",
        "--url",
        URL,
        "--out",
        s(&dir.join("c2.rb")),
    ]);
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("is not named kbf-installer-2.0.0-macos-arm64.pkg"),
        "{stderr}"
    );
}

fn sha256_of(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const MAIN: &str = "refs/heads/main";

#[test]
fn pkg_build_takes_the_version_from_the_binary_and_pkg_macos_asset_makes_the_asset() {
    // Catches: the steps of the macOS artifacts job not fitting each other: `pkg-trust`'s
    // directory (gh 0755, root, conf) not the `--trust` files `pkg build` wants, `--version-of`
    // ignored or given with `--version`, a package whose binary is not the tarball's,
    // or one attested for another branch than it trusts, accepted as the asset; and
    // `--verify-only` writing, or accepting a file not named as the asset.
    let dir = scratch("macos-asset");
    // The fake as `kbf-installer`, which prints `kbf-installer 0.1.0+<stamp>`.
    let binary = dir.join("named").join("kbf-installer");
    std::fs::create_dir(dir.join("named")).unwrap();
    std::fs::hard_link(FAKE, &binary).unwrap();
    let xtask = |args: &[&str]| {
        let out = Command::new(XTASK)
            .args(args)
            .env("XTASK_FAKE_INSTALLER_VERSION", &COMMIT[..12])
            .output()
            .expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    };
    // The trust directory as CI makes it.
    let (gh, root) = (dir.join("gh"), dir.join("root.jsonl"));
    std::fs::write(&gh, b"the pinned gh").unwrap();
    std::fs::write(&root, b"{\"mediaType\":\"trustedroot\"}\n").unwrap();
    let trust = dir.join("trust");
    let (code, _, stderr) = xtask(&[
        "pkg-trust",
        "--gh",
        s(&gh),
        "--trusted-root",
        s(&root),
        "--source-ref",
        MAIN,
        "--out",
        s(&trust),
    ]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let plist = plist();
    let out = dir.join("out");
    let build = |extra: &[&str]| {
        let mut args = vec![
            "pkg",
            "build",
            "--binary",
            s(&binary),
            "--launchd-plist",
            s(&plist),
        ];
        let files: Vec<PathBuf> = ["gh", "installer.conf", "trusted_root.jsonl"]
            .iter()
            .map(|n| trust.join(n))
            .collect();
        for f in &files {
            args.extend(["--trust", s(f)]);
        }
        args.extend(["--out", s(&out)]);
        args.extend_from_slice(extra);
        xtask(&args)
    };
    let (code, _, stderr) = build(&["--version", "1.0.0", "--version-of", s(&binary)]);
    assert_eq!(code, Some(2), "{stderr}");
    let (code, _, stderr) = build(&[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--version or --version-of"), "{stderr}");
    let (code, stdout, stderr) = build(&["--version-of", s(&binary)]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""), "{stdout}");
    let built = out.join("kbf-installer-0.1.0+0123456789ab-macos-arm64.pkg");
    assert!(built.is_file(), "{stdout}");
    // The tarball of the binary, as `xtask package` makes it.
    let tarball = dir.join(format!("kbf-installer-{COMMIT}-darwin-arm64.tar.gz"));
    std::fs::write(
        &tarball,
        xtask::package::tarball("kbf-installer", &std::fs::read(&binary).unwrap()).unwrap(),
    )
    .unwrap();
    let dist = dir.join("dist");
    let asset = |package: &Path, expect: &str, mode: &[&str]| {
        let mut args = vec![
            "pkg-macos-asset",
            "--package",
            s(package),
            "--commit",
            COMMIT,
            "--binary-tarball",
            s(&tarball),
            "--expect-ref",
            expect,
            "--gh",
            s(&gh),
        ];
        args.extend_from_slice(mode);
        xtask(&args)
    };
    let (code, stdout, stderr) = asset(&out, MAIN, &["--out", s(&dist)]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""), "{stdout}");
    let name = format!("kbf-installer-{COMMIT}-darwin-arm64.pkg");
    assert!(dist.join(&name).is_file());
    let sums = std::fs::read_to_string(dist.join("SHA256SUMS.darwin-arm64")).unwrap();
    assert!(sums.trim_end().ends_with(&format!("  {name}")), "{sums}");
    let before = std::fs::read_dir(&dist).unwrap().count();
    let (code, stdout, stderr) = asset(&dist.join(&name), MAIN, &["--verify-only"]);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert!(stdout.contains(&name), "{stdout}");
    assert_eq!(std::fs::read_dir(&dist).unwrap().count(), before);
    // Attested for another branch than it trusts: refused, in both modes.
    let (code, _, stderr) = asset(
        &dist.join(&name),
        "refs/heads/feature/x",
        &["--verify-only"],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("attested for refs/heads/feature/x"),
        "{stderr}"
    );
    let (code, _, stderr) = asset(
        &out,
        "refs/heads/feature/x",
        &["--out", s(&dir.join("fresh"))],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(!dir.join("fresh").exists());
    // Not named as the asset: only `--out` renames it.
    let (code, _, stderr) = asset(&built, MAIN, &["--verify-only"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("is not named"), "{stderr}");
    // `--out` and `--verify-only` together, and neither: usage errors.
    let (code, _, _) = asset(&built, MAIN, &["--verify-only", "--out", s(&dist)]);
    assert_eq!(code, Some(2));
    let (code, _, _) = asset(&built, MAIN, &[]);
    assert_eq!(code, Some(2));
}
