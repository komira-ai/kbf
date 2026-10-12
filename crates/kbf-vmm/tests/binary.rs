//! The `kbf-vmm` stub: what it answers, and what it refuses.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-vmm");

/// Catches a stub that does not answer `--version` with its name and version and exit
/// status 0: `cargo xtask sign-darwin` runs `<binary> --version` to show the signed
/// binary starts, so such a stub fails the signing job for a reason other than its
/// signature.
#[test]
fn version_prints_name_and_version() {
    let out = Command::new(BIN)
        .arg("--version")
        .output()
        .expect("run kbf-vmm");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("kbf-vmm {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// Catches a stub that exits 0 for anything but `--version`, so a caller could take
/// it for a VM helper that did its work: no arguments, an unknown verb and `--version`
/// with something after it all exit 2 and say why on stderr.
#[test]
fn anything_else_is_refused() {
    for args in [&[][..], &["run"][..], &["--version", "x"][..]] {
        let out = Command::new(BIN).args(args).output().expect("run kbf-vmm");
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("only --version is implemented"),
            "{args:?}: {out:?}"
        );
    }
}
