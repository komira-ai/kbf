//! The pure parts: the native version, the version line, the unit state, the constants.

use crate::{
    CLEANUP_SECS, PARAM_KEYS, ROLLBACK_SECS, UnitState, VERIFY_DEADLINE_SECS, VERIFY_SECS,
    VersionError, native_version, parse_version_output,
};

/// The package version is major, minor and the commit count, three plain numbers.
#[test]
fn the_native_version_is_major_minor_and_seq() {
    assert_eq!(native_version("0.1.0", 7472).as_deref(), Ok("0.1.7472"));
    assert_eq!(native_version("12.0.9", 0).as_deref(), Ok("12.0.0"));
    // Leading zeros are not kept: the repository compares numbers.
    assert_eq!(native_version("01.02.3", 5).as_deref(), Ok("1.2.5"));
}

/// A version that is not three decimal numbers is refused, whatever it is instead.
#[test]
fn the_native_version_refuses_what_is_not_three_numbers() {
    for bad in [
        "",
        "0.1",
        "0.1.2.3.4",
        "0.1.x",
        "0.1.-1",
        "1.2.3+4",
        "1.2.3~rc",
        "v1.2.3",
        "1..3",
        " 1.2.3",
        "1.2.99999999999999999999",
    ] {
        assert_eq!(
            native_version(bad, 1),
            Err(VersionError::NotThreeNumbers(bad.to_owned())),
            "{bad:?}"
        );
    }
}

/// The version line of `kbf --version --json` becomes the native version, and a line without a
/// version or a seq is refused by name.
#[test]
fn the_version_line_is_read_for_version_and_seq() {
    assert_eq!(
        parse_version_output("{\"commit\":\"abc\",\"seq\":42,\"version\":\"0.1.0\"}\n").as_deref(),
        Ok("0.1.42")
    );
    assert!(matches!(
        parse_version_output("not json"),
        Err(VersionError::BadOutput(_))
    ));
    assert_eq!(
        parse_version_output("{\"seq\":1}"),
        Err(VersionError::BadOutput(
            "no text field \"version\"".to_owned()
        ))
    );
    assert_eq!(
        parse_version_output("{\"version\":\"0.1.0\"}"),
        Err(VersionError::BadOutput(
            "no whole-number field \"seq\"".to_owned()
        ))
    );
    assert_eq!(
        parse_version_output("{\"version\":\"0.1\",\"seq\":1}"),
        Err(VersionError::NotThreeNumbers("0.1".to_owned()))
    );
}

/// Each word systemd prints is its state, and any other word is kept as it was.
#[test]
fn unit_states_round_trip_through_their_words() {
    for state in [
        UnitState::Active,
        UnitState::Activating,
        UnitState::Inactive,
        UnitState::Failed,
        UnitState::Other("deactivating".to_owned()),
    ] {
        assert_eq!(UnitState::parse(&format!("{}\n", state.word())), state);
    }
    assert_eq!(UnitState::parse(""), UnitState::Other(String::new()));
}

/// The manifest's constants agree: the verify deadline covers a verify and a rollback, the
/// param keys are the ones the hook reads, and the unbounded drain is the one step with no bound.
#[test]
fn the_manifest_constants_agree() {
    const { assert!(VERIFY_DEADLINE_SECS >= VERIFY_SECS + ROLLBACK_SECS) };
    assert_eq!(PARAM_KEYS, ["mode", "state_dir", "unit"]);
    assert_eq!(crate::PREPARE_SECS, 0);
    const { assert!(CLEANUP_SECS > 0 && crate::CLEANUP_RETRIES > 0 && crate::CUTOVER_SECS > 0) };
    assert_eq!(crate::HOOK_COMMAND, ["/usr/bin/kbf", "package-hook"]);
    assert_eq!(crate::PACKAGE, "kbf");
    assert_eq!(crate::RUN_UNIT, "kbf-storage-and-daemon.service");
    assert_eq!(crate::MODE, "storage-and-daemon");
    assert_eq!(crate::STATE_DIR, "/var/lib/kbf-package-hook");
}
