//! The record of the power and software update settings from before `apply` first
//! changed one, and `revert`, which puts them back (the record and revert of
//! kbf-mdm's `mac_phase0.sh`, for the same settings and more).

mod support;

use std::fs;

use kbf_mac_provision::cli::{self, Error};
use kbf_mac_provision::record::{RECORD, RECORD_DIR, RevertError};
use support::{
    COMMERCE, FakeMac, SOFTWARE_UPDATE, apply, apply_keys, calls, ls, pmset_value, preference,
};

/// The record a fresh fake Mac gets: its pmset values, and no preference set.
const FRESH_RECORD: &str = "\
pmset\t-\tsleep\t1
pmset\t-\tstandby\t1
pmset\t-\tpowernap\t1
pmset\t-\tautorestart\t0
pmset\t-\twomp\t0
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticallyInstallMacOSUpdates\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tCriticalUpdateInstall\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tConfigDataInstall\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticallyInstallAppUpdates\tABSENT
pref\t/Library/Preferences/com.apple.commerce\tAutoUpdate\tABSENT
";

/// The fresh fake Mac's power settings, in `pmset -g`'s order.
const FRESH_POWER: [(&str, &str); 5] = [
    ("sleep", "1"),
    ("standby", "1"),
    ("powernap", "1"),
    ("autorestart", "0"),
    ("womp", "0"),
];

/// Every preference the record holds.
const PREFERENCES: [(&str, &str); 6] = [
    (SOFTWARE_UPDATE, "AutomaticDownload"),
    (SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates"),
    (SOFTWARE_UPDATE, "CriticalUpdateInstall"),
    (SOFTWARE_UPDATE, "ConfigDataInstall"),
    (SOFTWARE_UPDATE, "AutomaticallyInstallAppUpdates"),
    (COMMERCE, "AutoUpdate"),
];

/// What is in the record directory besides the record: the names `revert` moved it to.
fn reverted(mac: &FakeMac) -> Vec<String> {
    fs::read_dir(mac.path(RECORD_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("settings.before.reverted-"))
        .collect()
}

/// Catches: an apply that skips the old-value record (revert then has nothing to put
/// back), or writes it after its change (it then holds the profile's values, and revert
/// changes nothing); and a revert that does not put a value back, does not delete a
/// preference that was not set, or keeps the record in place.
#[test]
fn revert_puts_back_what_the_first_apply_found() {
    let mac = FakeMac::fresh("revert");
    assert_eq!(apply(&mac).failed, 0);
    assert_eq!(fs::read_to_string(mac.path(RECORD)).unwrap(), FRESH_RECORD);
    assert_eq!(pmset_value(&mac, "sleep").as_deref(), Some("0"));
    let out = cli::revert(&mac).unwrap();
    let names = reverted(&mac);
    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "CHANGE power_sleep: pmset -a sleep 1 (reverted)",
            "CHANGE power_standby: pmset -a standby 1 (reverted)",
            "CHANGE power_powernap: pmset -a powernap 1 (reverted)",
            "CHANGE power_autorestart: pmset -a autorestart 0 (reverted)",
            "CHANGE power_womp: pmset -a womp 0 (reverted)",
            "CHANGE updates_auto_download: AutomaticDownload deleted (reverted)",
            "CHANGE updates_auto_install_macos: AutomaticallyInstallMacOSUpdates deleted (reverted)",
            "CHANGE updates_auto_install_security: CriticalUpdateInstall deleted (reverted)",
            "CHANGE updates_config_data: ConfigDataInstall deleted (reverted)",
            "CHANGE appstore_auto_update: AutomaticallyInstallAppUpdates deleted (reverted)",
            "CHANGE appstore_auto_update: com.apple.commerce AutoUpdate deleted (reverted)",
            &format!("CHANGE record: moved {RECORD} to {RECORD_DIR}/{}", names[0]),
        ]
    );
    for (key, value) in FRESH_POWER {
        assert_eq!(pmset_value(&mac, key).as_deref(), Some(value), "{key}");
    }
    for (domain, key) in PREFERENCES {
        assert_eq!(preference(&mac, domain, key), None, "{key}");
    }
    assert!(!mac.path(RECORD).exists());
    // With the record moved aside, the next apply records afresh.
    assert_eq!(apply(&mac).failed, 0);
    assert_eq!(fs::read_to_string(mac.path(RECORD)).unwrap(), FRESH_RECORD);
}

/// Catches: a record rewritten by a later apply, which then holds kbf's own values or a
/// drift's, so revert never gets back to the Mac's original ones.
#[test]
fn a_later_apply_keeps_the_first_record() {
    let mut mac = FakeMac::fresh("revert-later");
    assert_eq!(apply(&mac).failed, 0);
    mac.state_mut().pmset_set("sleep", "5");
    mac.state_mut()
        .default_set(SOFTWARE_UPDATE, "AutomaticDownload", "1");
    let r = apply(&mac);
    assert_eq!(
        (r.failed, r.text.as_str()),
        (
            0,
            "CHANGE power_sleep: pmset -a sleep 0\nCHANGE updates_auto_download: AutomaticDownload = 0\n"
        )
    );
    assert_eq!(fs::read_to_string(mac.path(RECORD)).unwrap(), FRESH_RECORD);
    cli::revert(&mac).unwrap();
    assert_eq!(pmset_value(&mac, "sleep").as_deref(), Some("1"));
    assert_eq!(preference(&mac, SOFTWARE_UPDATE, "AutomaticDownload"), None);
}

/// Catches: a record of only the setting being changed, so a later apply of another
/// recorded setting (already recorded, so not recorded again) cannot be reverted; and
/// a record that is not root's alone.
#[test]
fn the_first_record_holds_every_setting_and_is_roots_alone() {
    let mac = FakeMac::fresh("revert-keys");
    let r = apply_keys(&mac, Some("power_sleep"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(fs::read_to_string(mac.path(RECORD)).unwrap(), FRESH_RECORD);
    assert_eq!(ls(&mac, RECORD), "-rw------- 0 0");
    assert_eq!(ls(&mac, RECORD_DIR), "drwx------ 0 0");
    assert_eq!(
        apply_keys(&mac, Some("power_womp,appstore_auto_update")).failed,
        0
    );
    cli::revert(&mac).unwrap();
    assert_eq!(pmset_value(&mac, "womp").as_deref(), Some("0"));
    assert_eq!(preference(&mac, COMMERCE, "AutoUpdate"), None);
}

/// Catches: a fix that changes its setting when the record cannot be written, so the
/// change could never be reverted.
#[test]
fn a_setting_is_not_changed_when_its_record_cannot_be_written() {
    let mac = FakeMac::fresh("revert-unwritable");
    // A file where the record's directory belongs.
    mac.write(RECORD_DIR, "");
    let r = apply_keys(&mac, Some("power_sleep,updates_auto_download"));
    assert_eq!(r.failed, 2, "{}", r.text);
    assert!(
        r.text
            .starts_with(&format!("NOTE power_sleep: {RECORD_DIR}: ")),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains(&format!("NOTE updates_auto_download: {RECORD_DIR}: "))
    );
    assert_eq!(pmset_value(&mac, "sleep").as_deref(), Some("1"));
    assert_eq!(calls(&mac, "pmset -a"), Vec::<String>::new());
    assert_eq!(calls(&mac, "defaults write"), Vec::<String>::new());
}

/// Catches: a revert that changes the Mac on a record it should refuse: none, one
/// another user could have written, or a line that is not a recorded setting with a
/// value revert restores; and an error that does not say which.
#[test]
fn revert_refuses_a_missing_foreign_or_malformed_record() {
    let mut mac = FakeMac::fresh("revert-refused");
    assert_eq!(cli::revert(&mac), Err(Error::Revert(RevertError::NoRecord)));
    assert_eq!(
        RevertError::NoRecord.to_string(),
        format!("no record at {RECORD}: apply never changed a recorded setting, nothing to revert")
    );
    mac.write(RECORD, FRESH_RECORD);
    mac.chmod(RECORD, 0o600);
    mac.own(RECORD, 501, 20);
    assert_eq!(
        cli::revert(&mac),
        Err(Error::Revert(RevertError::NotRoots(
            "-rw------- 501 20".into()
        )))
    );
    mac.own(RECORD, 0, 0);
    mac.chmod(RECORD, 0o620);
    assert!(matches!(
        cli::revert(&mac),
        Err(Error::Revert(RevertError::NotRoots(_)))
    ));
    mac.chmod(RECORD, 0o600);
    mac.remove(RECORD);
    mac.symlink(&mac.path("/etc/sshd_config"), RECORD);
    assert!(matches!(
        cli::revert(&mac),
        Err(Error::Revert(RevertError::NotRoots(_)))
    ));
    mac.remove(RECORD);
    for (line, why) in [
        ("pmset\t-\tsleep", "not four tab-separated fields"),
        (
            "pmset\t-\tdisplaysleep\t1",
            "'pmset - displaysleep' is not a recorded setting",
        ),
        (
            "pref\t/Library/Preferences/com.apple.loginwindow\tautoLoginUser\t0",
            "is not a recorded setting",
        ),
        (
            "pmset\t-\tsleep\tnever",
            "sleep=never is not a value revert restores",
        ),
        (
            "pref\t/Library/Preferences/com.apple.commerce\tAutoUpdate\ttrue",
            "AutoUpdate=true is not a value revert restores",
        ),
        ("pmset\t-\twomp\t0", "womp is recorded twice"),
    ] {
        // The bad line comes last: nothing before it may have been restored.
        mac.write(RECORD, &format!("{FRESH_RECORD}{line}\n"));
        mac.chmod(RECORD, 0o600);
        mac.own(RECORD, 0, 0);
        mac.log.borrow_mut().clear();
        let e = cli::revert(&mac).unwrap_err();
        let Error::Revert(RevertError::Malformed { line: 12, why: got }) = &e else {
            panic!("{line:?}: {e:?}");
        };
        assert!(got.contains(why), "{line:?}: {got}");
        assert!(
            e.to_string().starts_with(&format!("{RECORD} line 12: ")),
            "{e}"
        );
        assert_eq!(mac.writes(), Vec::<String>::new(), "{line:?}");
    }
}

/// Catches: a revert that writes a setting already at its recorded value, deletes a
/// power setting pmset did not report, or overwrites a preference that held a value
/// other than a boolean (which it cannot put back).
#[test]
fn revert_changes_only_what_differs_and_leaves_what_it_cannot_restore() {
    let mut mac = FakeMac::converged("revert-partial");
    mac.state_mut().default_set(COMMERCE, "AutoUpdate", "1");
    let record = "\
pmset\t-\tsleep\t0
pmset\t-\tstandby\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\t1
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tConfigDataInstall\tOTHER
pref\t/Library/Preferences/com.apple.commerce\tAutoUpdate\t1
";
    mac.mkdir(RECORD_DIR);
    mac.write(RECORD, record);
    mac.chmod(RECORD, 0o600);
    mac.own(RECORD, 0, 0);
    let out = cli::revert(&mac).unwrap();
    let mut lines = out.lines();
    assert_eq!(
        lines.next(),
        Some("CHANGE updates_auto_download: AutomaticDownload = 1 (reverted)")
    );
    assert_eq!(
        lines.next(),
        Some(
            "NOTE updates_config_data: ConfigDataInstall held a value that is not 0 or 1; left as it is"
        )
    );
    assert!(
        lines.next().unwrap().starts_with("CHANGE record: moved "),
        "{out}"
    );
    assert_eq!(lines.next(), None, "{out}");
    assert_eq!(
        preference(&mac, SOFTWARE_UPDATE, "AutomaticDownload").as_deref(),
        Some("1")
    );
    assert_eq!(
        preference(&mac, SOFTWARE_UPDATE, "ConfigDataInstall").as_deref(),
        Some("1")
    );
    assert_eq!(pmset_value(&mac, "standby").as_deref(), Some("0"));
    assert_eq!(calls(&mac, "pmset -a"), Vec::<String>::new());
}

/// Catches: a value apply records wrongly: a preference that is not a boolean recorded
/// as one (revert would then write it as a boolean), a power setting pmset does not
/// report recorded as a value.
#[test]
fn values_that_are_not_booleans_or_numbers_are_recorded_as_such() {
    let mut mac = FakeMac::fresh("revert-other");
    mac.state_mut()
        .default_set(SOFTWARE_UPDATE, "AutomaticDownload", "(\n    a\n)");
    mac.state_mut().pmset.retain(|(k, _)| k != "womp");
    assert_eq!(apply_keys(&mac, Some("power_sleep")).failed, 0);
    let text = fs::read_to_string(mac.path(RECORD)).unwrap();
    assert!(text.contains("pmset\t-\twomp\tABSENT\n"), "{text}");
    assert!(
        text.contains(
            "pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\tOTHER\n"
        ),
        "{text}"
    );
}

/// Catches: a revert that reports success when the record could not be moved aside
/// (the next apply would then keep a record of values already put back).
#[test]
fn a_record_that_cannot_be_moved_aside_is_an_error() {
    let mac = FakeMac::fresh("revert-stuck");
    assert_eq!(apply(&mac).failed, 0);
    mac.chmod(RECORD_DIR, 0o500);
    let e = cli::revert(&mac).unwrap_err();
    mac.chmod(RECORD_DIR, 0o700);
    assert!(
        matches!(&e, Error::Revert(RevertError::Move(why)) if why.contains(RECORD)),
        "{e:?}"
    );
    assert!(
        e.to_string().starts_with("cannot move the record aside: "),
        "{e}"
    );
}
