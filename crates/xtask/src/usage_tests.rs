//! Verbs run with arguments the command line cannot produce, to see the guards behind it.

use std::path::PathBuf;

use super::*;

#[test]
fn pkg_config_given_neither_version_flag_is_a_usage_error_and_writes_nothing() {
    // Catches: the guard behind clap's `required_unless_present` removed or turned into a
    // default version, so a caller that builds the verb itself writes a package whose
    // version names no commit. clap refuses this on the command line (tests/pkg_linux.rs),
    // so only the verb called directly reaches it.
    let out = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join(format!("xtask-usage-{}", std::process::id()));
    let nowhere = PathBuf::from("/nonexistent/xtask-usage");
    let err = run(Cli {
        root: None,
        verb: Verb::PkgConfig {
            binary: nowhere.clone(),
            units: nowhere.clone(),
            trust: nowhere.clone(),
            sysusers: nowhere.clone(),
            tmpfiles: nowhere,
            platform: package::Platform::LinuxX86_64,
            version: None,
            version_of: None,
            out: out.clone(),
        },
    })
    .expect_err("no version");
    assert!(
        matches!(&err, Error::Usage(m) if m == "--version or --version-of"),
        "{err:?}"
    );
    assert!(!out.exists(), "wrote {}", out.display());
}
