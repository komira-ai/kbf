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
    // Typed, so a value cannot pass by matching another field's.
    let num = |k: &str| {
        said[k]
            .as_u64()
            .unwrap_or_else(|| panic!("{k} is a number"))
    };
    let text = |k: &str| {
        said[k]
            .as_str()
            .unwrap_or_else(|| panic!("{k} is text"))
            .to_owned()
    };
    assert_eq!(parsed.admin_protocol, Some(num("admin_protocol")));
    assert_eq!(parsed.meta_log, Some(num("meta_log")));
    assert_eq!(parsed.min_protocol, num("min_protocol"));
    assert_eq!(parsed.protocol, num("protocol"));
    assert_eq!(parsed.state_schema, num("state_schema"));
    assert_eq!(parsed.commit, text("commit"));
    assert_eq!(parsed.profile, text("profile"));
    assert_eq!(parsed.version, text("version"));
    // Every key kbf emits is one `Version` holds (`Debug` names each field it has), so a
    // new key cannot be added here without a typed line above.
    let held = format!("{parsed:?}");
    for key in said.as_object().expect("an object").keys() {
        assert!(
            held.contains(&format!("{key}: ")),
            "Version holds no {key}: {held}"
        );
    }
    assert_eq!(parsed.platform, None);
}
