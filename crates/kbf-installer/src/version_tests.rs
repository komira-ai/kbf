use std::ffi::OsString;

use super::*;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// Catches `--version --json` read from a longer or shorter command line, from `--json`
/// alone, or from a verb's own words: only the two words and nothing else ask for the
/// identity, so a typo is a usage error and never an answer.
#[test]
fn only_version_and_json_ask_for_the_identity() {
    for yes in [
        ["kbf-installer", "--version", "--json"],
        ["kbf-installer", "--json", "--version"],
    ] {
        assert!(asked(&os(&yes)), "{yes:?}");
    }
    for no in [
        &["kbf-installer"][..],
        &["kbf-installer", "--version"],
        &["kbf-installer", "--json"],
        &["kbf-installer", "--version", "--json", "extra"],
        &["kbf-installer", "--version", "--version"],
        &["kbf-installer", "--json", "--json"],
        &["kbf-installer", "status", "--json"],
        &["kbf-installer", "--version", "-json"],
        &["kbf-installer", "--json", "--version", "--apply"],
    ] {
        assert!(!asked(&os(no)), "{no:?}");
    }
}

/// Catches an identity that adds, drops or renames a field (a host reads these to check
/// what it runs, so the set is the contract), prints more than one line, or reports a
/// value other than the constants the binary was built with.
#[test]
fn the_identity_is_one_line_of_three_sorted_fields() {
    let line = line();
    assert_eq!(line.lines().count(), 1, "{line:?}");
    assert!(line.ends_with('\n'), "{line:?}");
    let v: Value = serde_json::from_str(&line).expect("one JSON value");
    let fields: Vec<&str> = v
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, ["commit", "profile", "version"], "{line}");
    assert_eq!(v["commit"], BUILD_COMMIT);
    assert_eq!(v["profile"], PROFILE);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

/// Catches a version that names the package version alone (two builds of one package
/// version would read the same), or one in a form `build-commit check` refuses: it wants
/// `<package version>+<12 lowercase hex digits>`.
#[test]
fn the_version_names_the_commit_built() {
    let (package, commit) = VERSION.split_once('+').expect("a + in the version");
    assert_eq!(package, env!("CARGO_PKG_VERSION"));
    assert_eq!(commit, BUILD_COMMIT);
}

/// Catches a build script that embeds something other than a commit as git writes it
/// (12 lowercase hex digits), or than the profile Cargo names. `unknown` is the one other
/// commit a build may carry: a source archive with no git.
#[test]
fn the_stamps_have_the_shape_the_workflow_checks() {
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    assert!(
        BUILD_COMMIT == "unknown" || (BUILD_COMMIT.len() == 12 && BUILD_COMMIT.bytes().all(hex)),
        "{BUILD_COMMIT:?}"
    );
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    assert_eq!(PROFILE, profile);
}
