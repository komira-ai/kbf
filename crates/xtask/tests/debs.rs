//! The `cargo xtask debs` binary: its flags, what it prints and its exit codes, with the pinned
//! files already in the directory (as the actions cache restores them), so that no test
//! downloads anything. The downloads, the mirror fallback and the rounds are the unit tests'
//! (src/debs/tests.rs).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use xtask::debs::Arch;
use xtask::fetch::Sha256;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("xtask-debs-{name}"))
}

/// A pins file of one package, `p 1.0`, for both architectures, from a mirror no test reaches
/// (`.invalid` never resolves).
fn pins(dir: &Path) -> PathBuf {
    let mut text = String::new();
    for arch in ["amd64", "arm64"] {
        text.push_str(&format!(
            "[{arch}]\nmirrors = [\"https://mirror.invalid/ubuntu\"]\n\
             debs = [{{ package = \"p\", version = \"1.0\", path = \"pool/main/p/p_1.0_{arch}.deb\", sha256 = \"{}\" }}]\n",
            Sha256::of(b"p")
        ));
    }
    let file = dir.join("pins.toml");
    std::fs::write(&file, text).expect("pins");
    file
}

/// This host's file name of `p`.
fn name() -> String {
    format!("p_1.0_{}.deb", Arch::host().expect("arch").name())
}

fn xtask(pins: &Path, dir: &Path) -> std::process::Output {
    Command::new(XTASK)
        .args(["debs", "--pins"])
        .arg(pins)
        .arg("--dir")
        .arg(dir)
        .output()
        .expect("run xtask")
}

/// Catches a verb that downloads or fails when the cache restored the pinned file: it exits 0
/// and prints one line saying the file was cached, and nothing on stderr.
#[test]
fn a_restored_directory_passes_and_says_so() {
    let dir = scratch("restored");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(name()), b"p").expect("p");
    let out = xtask(&pins(dir.path()), &debs);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "ok: the cached {} has SHA-256 {}\n",
            name(),
            Sha256::of(b"p")
        )
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
}

/// Catches a stray file in the directory passing (so `dpkg --install --recursive` would install
/// it): exit 1 with the reason on stderr and nothing on stdout; and a missing flag accepted
/// instead of a usage error (exit 2).
#[test]
fn a_stray_file_fails_and_a_missing_flag_is_a_usage_error() {
    let dir = scratch("stray");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(name()), b"p").expect("p");
    std::fs::write(debs.join("extra.deb"), b"x").expect("extra");
    let out = xtask(&pins(dir.path()), &debs);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("xtask: {debs:?} holds \"extra.deb\", which no pin names; nothing was installed\n")
    );
    assert_eq!(out.status.code(), Some(1));

    let usage = Command::new(XTASK)
        .args(["debs", "--pins", "pins.toml"])
        .output()
        .expect("run xtask");
    assert_eq!(usage.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&usage.stderr).contains("--dir <DIR>"),
        "{usage:?}"
    );
}

/// Catches a cached file of other bytes being installed: exit 1 naming both digests, and the
/// file left as it was.
#[test]
fn a_tampered_cached_file_fails() {
    let dir = scratch("tampered");
    let debs = dir.path().join("debs");
    std::fs::create_dir_all(&debs).expect("debs");
    std::fs::write(debs.join(name()), b"q").expect("q");
    let out = xtask(&pins(dir.path()), &debs);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!(
            "xtask: the cached {:?} has SHA-256 {}, not the pinned {}; nothing was installed\n",
            debs.join(name()),
            Sha256::of(b"q"),
            Sha256::of(b"p")
        )
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read(debs.join(name())).expect("q"), b"q");
}
