//! `cargo xtask pkg build|cask` run as CI runs them, with `xtask-fake` copied
//! as `kbf-installer`, the stub of the real tool, as the binary to package.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const PLIST: &str = "../../packaging/macos/com.komira.kbf-installer.plist";
const URL: &str = "https://github.com/komira-ai/komira-build-farm/releases/download/v1/x";

/// An empty directory of its own for one test, holding the stub as `kbf-installer`.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-pkg")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
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
    let out = Command::new(dir.join("kbf-installer"))
        .args(["serve", "--version"])
        .output()
        .expect("run the stub");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"kbf-installer-stub serve --version\n");
    assert_eq!(out.stderr, b"kbf-installer-stub serve --version\n");
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
