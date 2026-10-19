//! The contract between `kbf --version --json` and the installer that reads it before it
//! stages a release: the installer's strict parser (`deny_unknown_fields`) must accept
//! every field kbf-cli emits, and keep each one.

use kbf_installer::binary::Version;

/// Catches a field added to `kbf --version --json` that the installer's `Version` does
/// not know (`admin_protocol` was, from the day it was emitted: every real release was
/// refused at staging), and a field the parser accepts but drops. Every key kbf emits
/// must be one the parsed struct holds, with the emitted value.
#[test]
fn the_installer_parses_what_kbf_prints() {
    let line = kbf_cli::version::line();
    let said = kbf_cli::version::identity();
    let parsed = Version::parse(line.as_bytes())
        .unwrap_or_else(|e| panic!("the installer refuses {line}: {e:?}"));
    // `Version` is not `Serialize`; its `Debug` names every field it holds.
    let held = format!("{parsed:?}");
    for (key, value) in said.as_object().expect("an object") {
        assert!(
            held.contains(&format!("{key}: ")) && held.contains(&value.to_string()),
            "Version does not hold {key}={value}: {held}"
        );
    }
    assert_eq!(parsed.platform, None);
}
