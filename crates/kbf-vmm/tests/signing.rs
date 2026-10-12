//! `cargo xtask sign-darwin` and `darwin-asset` on a Mac, with the per-binary
//! entitlement allow list (tools/ci/darwin-entitlements.txt; docs/design/macos-vm-guests.md
//! section 7), on this crate's stub: the one binary the list gives an entitlement.
//!
//! Ignored by default: it runs `codesign` on copies of the stub, so the hosted macOS job
//! runs it on its own (`cargo test -p kbf-vmm --test signing -- --ignored`). Each case
//! says which defect it catches. The same comparisons on `codesign`'s text, without a
//! Mac, are in crates/kbf-it/tests/darwin_entitlements.rs.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::darwin;
use xtask::entitlements::ALLOWLIST;

const STUB: &str = env!("CARGO_BIN_EXE_kbf-vmm");
const VZ: &str = "com.apple.security.virtualization";
const GTA: &str = "com.apple.security.get-task-allow";

fn allowlist() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(ALLOWLIST)
}

/// A fresh copy of the stub at `<case>/<name>` in this test's scratch.
fn copy(case: &str, name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("signing")
        .join(case);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let path = dir.join(name);
    std::fs::copy(STUB, &path).expect("copy the stub");
    path
}

/// Signs ad hoc with the hardened runtime and exactly `keys` (all true), bypassing the
/// allow list.
fn sign_with(binary: &Path, keys: &[&str]) {
    let mut cmd = Command::new("codesign");
    cmd.args(["--force", "--sign", "-", "--options", "runtime"]);
    if !keys.is_empty() {
        let body: String = keys
            .iter()
            .map(|k| format!("<key>{k}</key><true/>\n"))
            .collect();
        let plist = binary.with_extension("plist");
        std::fs::write(
            &plist,
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n{body}</dict>\n</plist>\n"),
        )
        .expect("write the plist");
        cmd.arg("--entitlements").arg(plist);
    }
    let status = cmd.arg(binary).status().expect("run codesign");
    assert!(status.success(), "codesign {binary:?} {keys:?}");
}

/// The signature's entitlements, as codesign prints them.
fn signed_entitlements(binary: &Path) -> String {
    let out = Command::new("codesign")
        .args(["--display", "--entitlements", "-", "--xml"])
        .arg(binary)
        .output()
        .expect("run codesign");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The binary as `cargo xtask package` lays it out: its tarball, next to it.
fn package(binary: &Path) -> PathBuf {
    let name = binary.file_name().and_then(|n| n.to_str()).expect("name");
    let bytes = std::fs::read(binary).expect("read the binary");
    let tgz = xtask::package::tarball(name, &bytes).expect("tarball");
    let path = binary.with_extension("tar.gz");
    std::fs::write(&path, tgz).expect("write the tarball");
    path
}

fn sign(binary: &Path) -> Result<Vec<String>, xtask::Error> {
    let minos = darwin::minos_of(binary).expect("the stub's minos");
    darwin::sign(binary, &allowlist(), &minos, None)
}

/// Asserts `got` is an error whose message holds `want`.
#[track_caller]
fn assert_refused<T: std::fmt::Debug>(got: Result<T, xtask::Error>, want: &str, what: &str) {
    match got {
        Err(e) => assert!(e.to_string().contains(want), "{what}: {e} lacks {want:?}"),
        Ok(v) => panic!("{what}: accepted: {v:?}"),
    }
}

#[test]
#[ignore = "signs with codesign; the native-macos job runs it with --ignored"]
fn the_signer_applies_the_list_and_the_asset_check_accepts_it() {
    // Catches: sign-darwin not passing --entitlements for a listed binary (the stub
    // would carry none), a signer that gives an unlisted binary the list's entitlement
    // (a lookup that ignores the name), and a check that refuses the right set, in the
    // signer or in darwin-asset.
    for (name, want) in [("kbf-vmm", Some(VZ)), ("kbf-daemon", None)] {
        let binary = copy("signed", name);
        let lines = sign(&binary).unwrap_or_else(|e| panic!("sign {name}: {e}"));
        assert!(
            lines.iter().any(
                |l| l.starts_with(&format!("signed {}: flags adhoc,runtime", binary.display()))
            ),
            "{name}: {lines:#?}"
        );
        let got = signed_entitlements(&binary);
        match want {
            Some(key) => assert!(got.contains(&format!("<key>{key}</key>")), "{name}: {got}"),
            None => assert_eq!(got, "", "{name}"),
        }
        assert!(!got.contains(GTA), "{name}: {got}");
        let lines = darwin::asset(&package(&binary), name, &allowlist())
            .unwrap_or_else(|e| panic!("darwin-asset {name}: {e}"));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("signature valid, flags adhoc,runtime")),
            "{name}: {lines:#?}"
        );
    }
}

#[test]
#[ignore = "signs with codesign; the native-macos job runs it with --ignored"]
fn wrong_signatures_are_refused() {
    // Each row: a copy named `name`, signed by hand with `keys`, and the refusal the
    // check must give. Catches a check that lets any entitlement through for an
    // unlisted binary (kbf-daemon with the virtualization entitlement, kbf-mac-session
    // with get-task-allow), a subset compare that accepts one more key, a check that
    // treats "no entitlements" as always fine, and a compare of the count only.
    let other = "other entitlements than the allow list gives it";
    let cases: [(&str, &str, &[&str], &str); 5] = [
        (
            "daemon-vz",
            "kbf-daemon",
            &[VZ],
            "gives kbf-daemon no entitlements",
        ),
        (
            "session-gta",
            "kbf-mac-session",
            &[GTA],
            "gives kbf-mac-session no entitlements",
        ),
        ("vmm-extra", "kbf-vmm", &[VZ, GTA], other),
        ("vmm-none", "kbf-vmm", &[], "its signature carries none"),
        ("vmm-other", "kbf-vmm", &[GTA], other),
    ];
    for (case, name, keys, want) in cases {
        let binary = copy(case, name);
        sign_with(&binary, keys);
        assert_refused(
            darwin::check_entitlements(&binary, name, &allowlist()),
            want,
            case,
        );
        // The shipped-bytes check refuses it the same way.
        assert_refused(
            darwin::asset(&package(&binary), name, &allowlist()),
            want,
            case,
        );
    }
}

#[test]
#[ignore = "signs with codesign; the native-macos job runs it with --ignored"]
fn a_missing_list_fails_closed() {
    // Catches: a check or signer that skips itself, or reads "no list" as "no
    // entitlements", when the file is not there: for kbf-daemon with no entitlements
    // (which would otherwise pass) and for kbf-vmm.
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-list.txt");
    let want = format!(
        "the entitlement allow list {} is missing",
        missing.display()
    );
    for (case, name) in [("missing-daemon", "kbf-daemon"), ("missing-vmm", "kbf-vmm")] {
        let binary = copy(case, name);
        sign_with(&binary, &[]);
        assert_refused(
            darwin::check_entitlements(&binary, name, &missing),
            &want,
            case,
        );
        let minos = darwin::minos_of(&binary).expect("minos");
        assert_refused(darwin::sign(&binary, &missing, &minos, None), &want, case);
    }
}

#[test]
#[ignore = "signs with codesign; the native-macos job runs it with --ignored"]
fn the_signer_refuses_another_minos_and_a_tarball_without_the_binary() {
    // Catches: the minimum-macOS check skipped (a binary built for another macOS
    // signed and passed), and darwin-asset checking a binary the tarball does not hold
    // under the asked name.
    let binary = copy("minos", "kbf-daemon");
    assert_refused(
        darwin::sign(&binary, &allowlist(), "99.0", None),
        "want '99.0'",
        "minos",
    );
    let tarball = package(&copy("absent", "kbf-daemon"));
    assert_refused(
        darwin::asset(&tarball, "kbf-server", &allowlist()),
        "holds no kbf-server",
        "absent",
    );
}
