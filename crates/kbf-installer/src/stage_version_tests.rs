//! The version text a stage records in `retain.json` for the listing of kept releases. They
//! share `stage_tests`' rig.

use super::*;
use crate::retain::{self, Retained};

fn recorded(rig: &Rig) -> Retained {
    Retained::read(&retain::path(&rig.host)).expect("retain.json")
}

/// Catches a release that is installed and its version not recorded, or recorded for another
/// release, or recorded over what the file already held: after the stage `retain.json` has
/// the version the binary declared under the release's commit, the pin and the version of
/// another release that were there are still there, and a stale text for this release is
/// replaced when the release is staged again.
///
/// Catches: the update removed from `verify`; the commit or the text of another field
/// inserted; the file replaced instead of updated.
#[test]
fn a_staged_release_has_its_version_recorded_beside_what_the_file_held() {
    let rig = Rig::new("records-version");
    let other = CommitId::new(&"7".repeat(40)).expect("commit");
    Retained::update(&retain::path(&rig.host), |r| {
        r.pinned.insert(other.clone());
        r.versions.insert(other.clone(), "an older one".to_owned());
    })
    .expect("seed");

    let staged = rig.activate(&honest()).expect("staged");
    assert!(!staged.version.version.is_empty());
    let now = recorded(&rig);
    assert_eq!(
        now.versions.get(&commit()),
        Some(&staged.version.version),
        "the declared version, under the commit"
    );
    assert_eq!(
        now.versions.get(&other).map(String::as_str),
        Some("an older one")
    );
    assert_eq!(now.pinned, [other].into());

    // Staged again: the release is there already, and the text is made right.
    Retained::update(&retain::path(&rig.host), |r| {
        r.versions.insert(commit(), "stale".to_owned());
    })
    .expect("stale");
    let again = rig.activate(&honest()).expect("staged again");
    assert!(again.already_installed);
    assert_eq!(recorded(&rig).versions[&commit()], again.version.version);
}

/// Catches the frontend's own update not recording its release: that kind installs a
/// release too, and the listing must name its version.
///
/// Catches: the record made only for an activation.
#[test]
fn a_frontend_update_records_its_version_too() {
    let rig = Rig::new("records-version-frontend");
    let (locator, fetched) = rig.fetch(&tarball());
    let staged = rig
        .verify(
            &honest(),
            OrderKind::FrontendUpdate,
            &[locator],
            None,
            fetched,
        )
        .expect("staged");
    assert_eq!(recorded(&rig).versions[&commit()], staged.version.version);
}

/// Catches a refused release leaving a version behind: a release that `gh` does not accept
/// is not installed, and `retain.json` is not made.
///
/// Catches: the version recorded before the release is installed.
#[test]
fn a_refused_release_records_no_version() {
    let rig = Rig::new("records-version-refused");
    let runner = runner(
        ran(1, "", "no attestation"),
        ran(0, &version_json(COMMIT, "release"), ""),
    );
    rig.activate(&runner).expect_err("refused");
    assert!(!retain::path(&rig.host).exists());
    rig.assert_nothing_installed();
}

/// Catches a version that cannot be written failing the stage: the text is for the listing
/// only, so a `retain.json` that cannot be parsed (it may hold a pin) is left exactly as it
/// was and the release is installed all the same.
///
/// Catches: the result of the update propagated with `?`; the file overwritten.
#[test]
fn a_retain_file_that_cannot_be_parsed_does_not_stop_the_stage_and_is_left_alone() {
    let rig = Rig::new("records-version-garbled");
    let file = retain::path(&rig.host);
    fs::write(&file, "{ not json").expect("garble");
    let staged = rig.activate(&honest()).expect("staged");
    assert_eq!(staged.commit, commit());
    assert!(rig.release().join("kbf").exists());
    assert_eq!(fs::read_to_string(&file).expect("same"), "{ not json");
}
