//! The contract between `kbf --version --json` and the installer that reads it before it
//! stages a release: the installer's parser (which ignores fields it does not know) must
//! accept what kbf-cli emits, and keep each field it names: a field the parser does not
//! hold would be dropped without notice.

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
    assert_eq!(parsed.relay, Some(num("relay")));
    assert_eq!(parsed.seq, Some(num("seq")));
    assert_eq!(parsed.state_schema, num("state_schema"));
    assert_eq!(parsed.commit, text("commit"));
    // Absent from a build that does not know its full commit, and then `None`.
    assert_eq!(parsed.commit_full.as_deref(), said["commit_full"].as_str());
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

/// The installer rolls a `bundled` host back only to a `kbf` that declares the relay
/// (`RELAY_NEEDED`), and `kbf` is shipped by the same build as the installer: the level this
/// build declares must meet the level this build's installer needs, or `rollback` would refuse
/// the release it was shipped with.
///
/// Catches the two numbers drifting apart (the installer's raised, the build's not).
///
/// Catches: `RELAY_NEEDED` raised to 2 (red on both assertions).
#[test]
fn the_build_declares_the_relay_its_installer_needs() {
    let parsed = Version::parse(kbf_cli::version::line().as_bytes()).expect("parses");
    assert!(kbf_installer::binary::relays(&parsed), "{parsed:?}");
    let declared = kbf_cli::version::RELAY;
    let needed = kbf_installer::binary::RELAY_NEEDED;
    assert!(declared >= needed, "declares {declared}, needs {needed}");
}

/// The installer refuses a release that declares a `protocol`, `min_protocol` or
/// `state_schema` above the numbers it understands (`installer_too_old`), and it is put on a
/// host by hand, never updated by a release. So the numbers `kbf --version --json` declares
/// must never pass those the installer of the same build understands: the day the worker
/// protocol moves on, this fails until the installer's numbers move with it (decision D3:
/// they are the lead's to raise).
///
/// Catches a protocol bump or a `min_protocol` raise that would leave every host's installer
/// refusing the next release as too old. Those two numbers are the real pin: the installer
/// cannot link `kbf-server`, so `supported()` holds them as literals and this test is where
/// they meet the server's.
///
/// `state_schema` is weaker. `kbf --version --json` and `supported()` both read
/// `kbf_pack_index::schema::latest()` in the same build, so a new migration moves both
/// together and that line cannot go red for it. It only catches `supported()` being changed to
/// a number below `latest()`.
///
/// Catches: `kbf_installer::stage::supported()` with `protocol: 0` (red on the
/// protocol line); with `state_schema: 0` (red on the schema line); the server's
/// `PROTOCOL_VERSION` raised to 2 (red on the protocol line).
#[test]
fn the_installer_understands_what_kbf_declares() {
    let said = kbf_cli::version::identity();
    let num = |k: &str| {
        said[k]
            .as_u64()
            .unwrap_or_else(|| panic!("{k} is a number"))
    };
    let understood = kbf_installer::stage::supported();
    assert!(
        understood.protocol >= num("protocol"),
        "kbf speaks protocol {}, the installer understands {}",
        num("protocol"),
        understood.protocol
    );
    assert!(
        understood.min_protocol >= num("min_protocol"),
        "kbf's oldest protocol is {}, the installer understands {}",
        num("min_protocol"),
        understood.min_protocol
    );
    assert!(
        understood.state_schema >= num("state_schema"),
        "kbf embeds schema {}, the installer understands {}",
        num("state_schema"),
        understood.state_schema
    );
}
