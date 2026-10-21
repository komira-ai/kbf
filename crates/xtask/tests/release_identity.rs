//! `cargo xtask release identity`: it runs the `kbf` of the linux-x86_64 tarball of the
//! commit with `--version --json` and nothing else, and writes what it printed.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

fn root(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("release-identity"),
        name,
    );
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    dir
}

/// Puts `<binary>-<COMMIT>-<platform>.tar.gz` holding `bytes` as the file `entry`.
fn tarball(root: &Path, binary: &str, platform: &str, entry: &str, bytes: &[u8]) {
    let tgz = xtask::package::tarball(entry, bytes).unwrap();
    let name = format!("{binary}-{COMMIT}-{platform}.tar.gz");
    std::fs::write(root.join("dist").join(name), tgz).unwrap();
}

fn identity(
    root: &Path,
    commit: &str,
    defect: Option<&str>,
) -> ((Option<i32>, String, String), PathBuf) {
    let out = root.join("version.json");
    let o = Command::new(XTASK)
        .args(["release", "identity"])
        .arg(root.join("dist"))
        .args(["--commit", commit, "--out"])
        .arg(&out)
        .env("TMPDIR", root)
        .env_remove("XTASK_FAKE_DEFECT")
        .envs(defect.map(|d| ("XTASK_FAKE_DEFECT", d)))
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    ((o.status.code(), text(&o.stdout), text(&o.stderr)), out)
}

/// The fake with the trailer it reads as `kbf`: `--version --json` prints `says`.
fn kbf(says: &str) -> Vec<u8> {
    let mut bytes = std::fs::read(FAKE).expect("the fake");
    bytes.extend_from_slice(format!("\nxtask-fake-kbf\nstamp\n{says}\n").as_bytes());
    bytes
}

#[test]
fn it_writes_what_the_binary_prints_for_exactly_version_json() {
    // Catches: another invocation (the script exits 9 for any other argument list), the
    // wrong tarball (a darwin one and another binary's sit beside it and print other
    // numbers), or the printed line altered.
    let root = root("prints");
    let says = r#"{"commit":"c0ffee012345","meta_log":3,"protocol":1}"#;
    tarball(&root, "kbf", "linux-x86_64", "kbf", &kbf(says));
    tarball(
        &root,
        "kbf",
        "darwin-arm64",
        "kbf",
        &kbf(r#"{"meta_log":99}"#),
    );
    tarball(
        &root,
        "kbf-deploy",
        "linux-x86_64",
        "kbf",
        &kbf(r#"{"meta_log":98}"#),
    );
    let ((code, o, err), out) = identity(&root, COMMIT, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(o.contains("wrote the identity of"), "{o}");
    assert_eq!(std::fs::read_to_string(out).unwrap(), format!("{says}\n"));
}

/// One way a tarball is no source of an identity: its entry, what `kbf` says, a defect
/// planted in the fake, and what the refusal says.
struct Bad {
    name: &'static str,
    tarball: Option<(&'static str, String)>,
    defect: Option<&'static str>,
    why: &'static str,
}

#[test]
fn it_refuses_what_it_cannot_read_as_an_identity() {
    // Catches: a failing binary, a non-JSON line, a JSON value that is not an object, a
    // tarball without `kbf`, or none at all, read as an identity (or as "unknown").
    let long = format!("{{\"x\":\"{}\"}}", "a".repeat(9000));
    let bad = |name, tarball, defect, why| Bad {
        name,
        tarball,
        defect,
        why,
    };
    let cases = [
        bad(
            "fails",
            Some(("kbf", "{}".into())),
            Some("json-fails"),
            "failed",
        ),
        bad(
            "not-json",
            Some(("kbf", "meta_log=3".into())),
            None,
            "did not print one JSON object",
        ),
        bad(
            "array",
            Some(("kbf", "[3]".into())),
            None,
            "did not print one JSON object",
        ),
        bad(
            "too-long",
            Some(("kbf", long)),
            None,
            "did not print one JSON object",
        ),
        bad(
            "other-entry",
            Some(("not-kbf", "{}".into())),
            None,
            "no kbf to run",
        ),
        bad("no-tarball", None, None, "open"),
    ];
    for c in cases {
        let root = root(c.name);
        if let Some((entry, says)) = &c.tarball {
            tarball(&root, "kbf", "linux-x86_64", entry, &kbf(says));
        }
        let ((code, _, err), out) = identity(&root, COMMIT, c.defect);
        assert_eq!(code, Some(1), "{}: {err}", c.name);
        assert!(err.contains(c.why), "{}: {err}", c.name);
        assert!(!out.exists(), "{}: wrote an identity", c.name);
    }
}

#[test]
fn it_refuses_a_commit_that_is_not_a_full_sha() {
    let root = root("short");
    let ((code, _, err), _) = identity(&root, "abc", None);
    assert_eq!(code, Some(2), "{err}");
}
