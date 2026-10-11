//! The per-binary entitlement allow list (`tools/ci/darwin-entitlements.txt`) and the
//! parts of `tools/ci/darwin-entitlements.sh` that run on any host: the list's lines,
//! the plist it renders for a name, and the refusals of a missing or malformed list.
//!
//! The comparison with a real signature needs `codesign` and runs on the hosted macOS
//! job (`tools/ci/sign-darwin-tests.sh`); docs/design/macos-vm-guests.md section 7.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

const SCRIPT: &str = "tools/ci/darwin-entitlements.sh";
const LIST: &str = "tools/ci/darwin-entitlements.txt";

/// The plist the committed list gives `kbf-vmm`, byte for byte.
const VMM_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.virtualization</key>
	<true/>
</dict>
</plist>
"#;

/// Runs the script with `args`, from the repository root.
fn script(args: &[&str]) -> Output {
    let root = repo_root();
    Command::new("bash")
        .arg(root.join(SCRIPT))
        .args(args)
        .current_dir(&root)
        .output()
        .expect("run bash")
}

/// A fresh file holding `text`, for one test.
fn list_file(test: &str, text: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("darwin-entitlements");
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let path = dir.join(format!("{test}-{}.txt", std::process::id()));
    std::fs::write(&path, text).expect("write the list");
    path
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Asserts the run exited 1 with nothing on stdout and `want` on stderr.
fn assert_refused(out: &Output, want: &str, what: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{what}: {out:?}");
    assert!(out.stdout.is_empty(), "{what}: printed {:?}", stdout(out));
    assert!(
        stderr.contains(want),
        "{what}: stderr {stderr:?} lacks {want:?}"
    );
}

/// Catches an entitlement given to any binary, or kbf-vmm's set changed, without this
/// test changing in the same review: the list's lines, comments and blanks aside, are
/// exactly kbf-vmm with `com.apple.security.virtualization`.
#[test]
fn the_committed_list_gives_kbf_vmm_the_virtualization_entitlement_only() {
    let text = std::fs::read_to_string(repo_root().join(LIST)).expect(LIST);
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines, ["kbf-vmm com.apple.security.virtualization"]);
}

/// Catches a renderer whose plist is not the one the signer applies (wrong key, a value
/// other than true, a malformed document) for the one listed binary.
#[test]
fn kbf_vmm_gets_the_virtualization_plist() {
    let out = script(&["plist", "kbf-vmm"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), VMM_PLIST);
}

/// Catches a lookup that ignores the binary's name, or matches a prefix or a part of
/// it: every other name, the shipped binaries and near-misses of `kbf-vmm` included,
/// gets no plist at all (so `sign-darwin.sh` signs it with no entitlements).
#[test]
fn every_other_binary_gets_none() {
    for name in [
        "kbf-daemon",
        "kbf-mac-session",
        "kbf-guest",
        "kbf-server",
        "kbf-vm",
        "kbf-vmm2",
        "vmm",
        "KBF-VMM",
    ] {
        let out = script(&["plist", name]);
        assert!(out.status.success(), "{name}: {out:?}");
        assert_eq!(stdout(&out), "", "{name}");
    }
}

/// Catches a list that fails open: with the file missing, `plist` and `check` refuse
/// for a listed and an unlisted name alike, rather than read "no list" as "no
/// entitlements" (which `check` would accept for an unentitled binary).
#[test]
fn a_missing_list_is_refused() {
    let missing = list_file("missing", "").with_file_name("no-such-list.txt");
    let missing = missing.to_str().expect("UTF-8 path");
    for args in [
        &["--allowlist", missing, "plist", "kbf-daemon"][..],
        &["--allowlist", missing, "plist", "kbf-vmm"][..],
        &[
            "--allowlist",
            missing,
            "check",
            "/no/such/binary",
            "kbf-daemon",
        ][..],
    ] {
        assert_refused(
            &script(args),
            &format!("the entitlement allow list {missing} is missing"),
            &format!("{args:?}"),
        );
    }
}

/// Catches a parser that accepts a list two readers could read two ways: a binary
/// listed twice (which line wins?), an entitlement repeated, a binary with no
/// entitlement, and a name or entitlement holding characters the plist would have to
/// escape. Each is refused whatever name is asked for, the unlisted one included.
#[test]
fn a_malformed_list_is_refused() {
    let cases = [
        (
            "twice",
            "kbf-vmm a.b\nkbf-vmm c.d\n",
            "kbf-vmm is listed twice",
        ),
        (
            "repeat",
            "kbf-vmm a.b c.d a.b\n",
            "kbf-vmm repeats an entitlement",
        ),
        ("empty", "kbf-vmm\n", "kbf-vmm lists no entitlement"),
        ("key", "kbf-vmm a<b\n", "'a<b' is not an entitlement name"),
        ("name", "-kbf-vmm a.b\n", "'-kbf-vmm' is not a binary name"),
    ];
    for (test, text, want) in cases {
        let list = list_file(test, &format!("# a comment\n\n{text}"));
        let list = list.to_str().expect("UTF-8 path");
        for name in ["kbf-vmm", "kbf-daemon"] {
            assert_refused(
                &script(&["--allowlist", list, "plist", name]),
                want,
                &format!("{test} asking for {name}"),
            );
        }
    }
}

/// Catches a renderer that keeps the list's order or drops all but the first key, and a
/// lookup that takes another line's set: the keys of the asked line come out sorted, a
/// comment after them is ignored, a last line without a newline is read, and the other
/// binary gets its own set.
#[test]
fn a_line_renders_its_own_keys_sorted() {
    let list = list_file("two", "kbf-a z.z a.a # trailing comment\nkbf-b m.m");
    let list = list.to_str().expect("UTF-8 path");
    let out = script(&["--allowlist", list, "plist", "kbf-a"]);
    assert!(out.status.success(), "{out:?}");
    let keys: Vec<String> = stdout(&out)
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("<key>")?
                .strip_suffix("</key>")
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(keys, ["a.a", "z.z"]);
    assert_eq!(stdout(&out).matches("<true/>").count(), 2);
    let out = script(&["--allowlist", list, "plist", "kbf-b"]);
    assert!(
        stdout(&out).contains("<key>m.m</key>") && !stdout(&out).contains("a.a"),
        "{out:?}"
    );
}

/// Catches a script that treats a mistyped call as an empty answer: no command, an
/// unknown command and a wrong argument count exit 2 with the usage line.
#[test]
fn a_wrong_call_is_a_usage_error() {
    for args in [
        &[][..],
        &["sign", "kbf-vmm"][..],
        &["plist"][..],
        &["plist", "a", "b"][..],
        &["check"][..],
        &["--allowlist"][..],
    ] {
        let out = script(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("usage:"),
            "{args:?}"
        );
    }
}
