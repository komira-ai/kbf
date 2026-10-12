//! `node-admin phase0 status|apply|revert` over a fake Mac. Each test names the
//! defect it catches.

mod admin_fake;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use admin_fake::{COMMERCE, FakeMac, SOFTWARE_UPDATE};

/// The record a first `apply` writes on the fake Mac as it starts.
const FIRST_RECORD: &str = "\
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\t1
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticallyInstallMacOSUpdates\t1
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tCriticalUpdateInstall\tABSENT
pref\t/Library/Preferences/com.apple.commerce\tAutoUpdate\t1
pmset\t-\tautorestart\t0
pmset\t-\tsleep\t1
";

/// What `apply` changes on the fake Mac as it starts, in order.
const APPLY_CHANGES: [&str; 6] = [
    "/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticDownload -bool false",
    "/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticallyInstallMacOSUpdates -bool false",
    "/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate CriticalUpdateInstall -bool false",
    "/usr/bin/defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool false",
    "/usr/bin/pmset -a autorestart 1",
    "/usr/bin/pmset -a sleep 0",
];

fn applied(mac: &FakeMac) {
    mac.run(&["phase0", "apply"]).succeeded();
    mac.clear_calls();
}

/// Catches: `status` changing anything, a wrong label or wanted value, a missing
/// kept setting, or a record that is not mentioned.
#[test]
fn status_shows_every_setting_and_changes_nothing() {
    let mac = FakeMac::new("status");
    let ran = mac.run(&["phase0", "status"]);
    ran.succeeded();
    let pad = |label: &str| format!("  {label:<58}");
    let want = [
        "Mac: 26.1 (25B78)".to_owned(),
        "phase0:".to_owned(),
        format!(
            "{} 1 (want 0)",
            pad("com.apple.SoftwareUpdate AutomaticDownload")
        ),
        format!(
            "{} 1 (want 0)",
            pad("com.apple.SoftwareUpdate AutomaticallyInstallMacOSUpdates")
        ),
        format!(
            "{} ABSENT (want 0)",
            pad("com.apple.SoftwareUpdate CriticalUpdateInstall")
        ),
        format!("{} 1 (want 0)", pad("com.apple.commerce AutoUpdate")),
        format!("{} 0 (want 1)", pad("pmset autorestart")),
        format!("{} 1 (want 0)", pad("pmset sleep")),
        format!(
            "{} 1",
            pad("com.apple.SoftwareUpdate ConfigDataInstall (kept)")
        ),
        format!(
            "{} ABSENT",
            pad("com.apple.SoftwareUpdate AutomaticCheckEnabled (kept)")
        ),
        "record: none (phase0 apply never ran)".to_owned(),
    ];
    assert_eq!(ran.out.lines().collect::<Vec<_>>(), want);
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
    applied(&mac);
    let ran = mac.run(&["phase0", "status"]);
    let last = ran.out.lines().last().unwrap().to_owned();
    assert_eq!(
        last,
        format!(
            "record: {} (phase0 revert restores it)",
            mac.record().display()
        )
    );
}

/// Catches: `status` stopping when `sw_vers` cannot say the version.
#[test]
fn status_without_sw_vers_shows_question_marks() {
    let mac = FakeMac::new("status-sw-vers");
    let fails = [("/usr/bin/sw_vers -buildVersion", "no")];
    mac.state
        .borrow_mut()
        .fails
        .extend(fails.map(|(a, b)| (a.to_owned(), b.to_owned())));
    mac.state
        .borrow_mut()
        .unstartable
        .insert("/usr/bin/sw_vers -productVersion".to_owned());
    let ran = mac.run(&["phase0", "status"]);
    ran.succeeded();
    assert!(ran.out.starts_with("Mac: ? (?)\n"), "{}", ran.out);
}

/// Catches: `apply` missing a setting, setting a wrong value, touching
/// `ConfigDataInstall` (XProtect's data), or changing a setting before the record of
/// the original values is written.
#[test]
fn apply_sets_the_plan_and_records_the_originals_first() {
    let mac = FakeMac::new("apply");
    let ran = mac.run(&["phase0", "apply"]);
    ran.succeeded();
    assert_eq!(mac.changes(), APPLY_CHANGES);
    for (domain, key) in [
        (SOFTWARE_UPDATE, "AutomaticDownload"),
        (SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates"),
        (SOFTWARE_UPDATE, "CriticalUpdateInstall"),
        (COMMERCE, "AutoUpdate"),
    ] {
        assert_eq!(mac.pref(domain, key).as_deref(), Some("0"), "{key}");
    }
    assert_eq!(
        mac.pref(SOFTWARE_UPDATE, "ConfigDataInstall").as_deref(),
        Some("1")
    );
    assert_eq!(
        (mac.pmset("autorestart"), mac.pmset("sleep")),
        ("1".into(), "0".into())
    );
    assert_eq!(mac.pmset("powernap"), "1");
    assert_eq!(fs::read_to_string(mac.record()).unwrap(), FIRST_RECORD);
    let mode = fs::metadata(mac.record()).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600);
    let wrote = ran.out.find("record: wrote ").unwrap();
    let first_change = ran.out.find("+ /usr/bin/defaults write").unwrap();
    assert!(wrote < first_change, "{}", ran.out);
    assert!(ran.out.contains("\nafter:\n"), "{}", ran.out);
}

/// Catches: a second `apply` rewriting the record with values the first one set
/// (so `revert` could never restore the originals), or writing settings already set.
#[test]
fn apply_again_keeps_the_record_and_changes_nothing() {
    let mac = FakeMac::new("apply-again");
    applied(&mac);
    let ran = mac.run(&["phase0", "apply"]);
    ran.succeeded();
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
    assert_eq!(fs::read_to_string(mac.record()).unwrap(), FIRST_RECORD);
    assert!(
        ran.out
            .contains("kept (it holds what the first apply found)"),
        "{}",
        ran.out
    );
}

/// Catches: a record someone else owns being kept by `apply` or trusted by `revert`.
#[test]
fn a_record_that_is_not_roots_is_refused() {
    let mac = FakeMac::new("record-owner");
    applied(&mac);
    mac.own(&mac.record(), 501);
    mac.run(&["phase0", "apply"])
        .refused_for("phase0.before is owned by uid 501, not root");
    mac.run(&["phase0", "revert"])
        .refused_for("phase0.before is owned by uid 501, not root");
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
}

/// Catches: a failed `defaults` or `pmset` (exit status, or not started) passing as
/// applied.
#[test]
fn a_failed_change_fails_apply() {
    let mac = FakeMac::new("apply-fails");
    let fail = "/usr/bin/pmset -a autorestart".to_owned();
    mac.state
        .borrow_mut()
        .fails
        .insert(fail, "not permitted\n".to_owned());
    mac.run(&["phase0", "apply"])
        .failed_for("/usr/bin/pmset -a autorestart 1 failed: not permitted");
    assert!(mac.record().exists());
    let mac = FakeMac::new("apply-unstartable");
    mac.state
        .borrow_mut()
        .unstartable
        .insert("/usr/bin/defaults write".to_owned());
    mac.run(&["phase0", "apply"])
        .failed_for("/usr/bin/defaults: no such program");
}

/// Catches: a `pmset -g` that failed, or that lists no such setting, read as a value.
#[test]
fn an_unreadable_power_setting_fails() {
    let mac = FakeMac::new("pmset-fails");
    mac.state
        .borrow_mut()
        .fails
        .insert("/usr/bin/pmset -g".to_owned(), "no\n".to_owned());
    mac.run(&["phase0", "status"])
        .failed_for("pmset -g failed: no");
    let mac = FakeMac::new("pmset-missing");
    mac.state.borrow_mut().pmset.remove("sleep");
    mac.run(&["phase0", "apply"])
        .failed_for("pmset -g lists no sleep");
    assert!(!mac.record().exists());
}

/// Catches: settings changed although the record of the originals could not be
/// written (a `revert` would then have nothing to restore).
#[test]
fn apply_changes_nothing_when_the_record_cannot_be_written() {
    let mac = FakeMac::new("record-fails");
    // The temporary file's name is taken (by a folder, which the sweep leaves).
    let temp = mac
        .state_dir()
        .join(format!(".incoming.{}", std::process::id()));
    fs::create_dir(&temp).unwrap();
    mac.run(&["phase0", "apply"]).failed_for("phase0.before: ");
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
    assert!(!mac.record().exists());
    assert!(temp.is_dir());
}

/// Catches: `revert` without a record doing anything.
#[test]
fn revert_without_a_record_is_refused() {
    let mac = FakeMac::new("revert-none");
    mac.run(&["phase0", "revert"])
        .refused_for("phase0 apply never ran, nothing to revert");
    assert!(mac.changes().is_empty());
}

/// Catches: `revert` leaving a setting as `apply` set it, writing back a wrong value,
/// writing a preference that was not set instead of deleting it, keeping the record
/// where a second `revert` would apply it again, or rewriting a setting already at
/// its original value.
#[test]
fn revert_restores_the_originals_and_moves_the_record_aside() {
    let mac = FakeMac::new("revert");
    applied(&mac);
    // Someone put sleep back by hand: revert leaves it.
    mac.state
        .borrow_mut()
        .pmset
        .insert("sleep".to_owned(), "1".to_owned());
    let ran = mac.run(&["phase0", "revert"]);
    ran.succeeded();
    assert_eq!(
        mac.changes(),
        [
            "/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticDownload -bool true",
            "/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticallyInstallMacOSUpdates -bool true",
            "/usr/bin/defaults delete /Library/Preferences/com.apple.SoftwareUpdate CriticalUpdateInstall",
            "/usr/bin/defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool true",
            "/usr/bin/pmset -a autorestart 0",
        ]
    );
    assert_eq!(
        mac.pref(SOFTWARE_UPDATE, "AutomaticDownload").as_deref(),
        Some("1")
    );
    assert_eq!(mac.pref(SOFTWARE_UPDATE, "CriticalUpdateInstall"), None);
    assert_eq!(
        (mac.pmset("autorestart"), mac.pmset("sleep")),
        ("0".into(), "1".into())
    );
    assert!(!mac.record().exists());
    let aside = mac
        .state_dir()
        .join("phase0.before.reverted-20270115T080000Z");
    assert_eq!(fs::read_to_string(&aside).unwrap(), FIRST_RECORD);
    assert!(
        ran.out
            .contains(&format!("record: moved to {}", aside.display())),
        "{}",
        ran.out
    );
    mac.run(&["phase0", "revert"])
        .refused_for("nothing to revert");
}

/// Catches: `revert` writing back a value `false` from a record that says 0.
#[test]
fn revert_writes_false_for_zero() {
    let mac = FakeMac::new("revert-zero");
    let record = FIRST_RECORD.replace("AutomaticDownload\t1", "AutomaticDownload\t0");
    fs::write(mac.record(), record).unwrap();
    mac.run(&["phase0", "revert"]).succeeded();
    assert_eq!(
        mac.pref(SOFTWARE_UPDATE, "AutomaticDownload").as_deref(),
        Some("0")
    );
    let writes = mac.changes();
    assert!(
        writes.contains(&"/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticDownload -bool false".to_owned()),
        "{writes:?}"
    );
}

/// Catches: a `revert` whose step failed moving the record aside anyway (the
/// originals would be lost).
#[test]
fn a_failed_revert_keeps_the_record() {
    let mac = FakeMac::new("revert-fails");
    applied(&mac);
    let fail = "/usr/bin/defaults delete".to_owned();
    mac.state
        .borrow_mut()
        .fails
        .insert(fail, "denied\n".to_owned());
    mac.run(&["phase0", "revert"])
        .failed_for("CriticalUpdateInstall failed: denied");
    assert_eq!(fs::read_to_string(mac.record()).unwrap(), FIRST_RECORD);
}

/// Catches: `revert` trusting a record it did not write: a setting that is not
/// phase 0's (root would write any preference), a value it cannot restore (or one
/// that carries more than a number), a setting listed twice or missing, or a line of
/// another shape. Nothing is changed for any of them.
#[test]
fn revert_checks_the_whole_record_first() {
    let foreign = "pref\t/Library/Preferences/com.apple.loginwindow\tautoLoginUser\t1\n";
    let cases = [
        (
            format!("{FIRST_RECORD}{foreign}"),
            "'pref /Library/Preferences/com.apple.loginwindow autoLoginUser' is not a phase0 setting",
        ),
        (
            FIRST_RECORD.replace("sleep\t1", "sleep\t1;reboot"),
            "sleep=1;reboot is not a value phase0 restores",
        ),
        (
            FIRST_RECORD.replace("sleep\t1", "sleep\t1a"),
            "sleep=1a is not a value phase0 restores",
        ),
        (
            FIRST_RECORD.replace("sleep\t1", "sleep\t"),
            "sleep= is not a value phase0 restores",
        ),
        (
            FIRST_RECORD.replace("sleep\t1", "sleep\t1000"),
            "sleep=1000 is not a value phase0 restores",
        ),
        (
            FIRST_RECORD.replace("AutoUpdate\t1", "AutoUpdate\t2"),
            "AutoUpdate=2 is not a value phase0 restores",
        ),
        (
            format!("{FIRST_RECORD}pmset\t-\tsleep\t5\n"),
            "sleep is listed twice",
        ),
        (
            FIRST_RECORD.replace("pmset\t-\tautorestart\t0\n", ""),
            "autorestart is not in the record",
        ),
        (
            format!("{FIRST_RECORD}pmset sleep 1\n"),
            "\"pmset sleep 1\" is not kind<TAB>domain<TAB>key<TAB>value",
        ),
        (
            FIRST_RECORD.replace("pmset\t-\tsleep", "pmset\t/x\tsleep"),
            "'pmset /x sleep' is not a phase0 setting",
        ),
    ];
    for (n, (record, why)) in cases.iter().enumerate() {
        let mac = FakeMac::new(&format!("revert-bad-{n}"));
        fs::write(mac.record(), record).unwrap();
        mac.run(&["phase0", "revert"]).refused_for(why);
        assert!(mac.changes().is_empty(), "{why}: {:?}", mac.changes());
        assert!(mac.record().exists(), "{why}");
    }
    let mac = FakeMac::new("revert-pmset-max");
    fs::write(mac.record(), FIRST_RECORD.replace("sleep\t1", "sleep\t999")).unwrap();
    mac.run(&["phase0", "revert"]).succeeded();
    assert_eq!(mac.pmset("sleep"), "999");
}

/// Catches: a record that cannot be read (not text) passing as an empty one.
#[test]
fn an_unreadable_record_fails_revert() {
    let mac = FakeMac::new("revert-binary");
    fs::write(mac.record(), [0xff, 0xfe]).unwrap();
    mac.run(&["phase0", "revert"]).failed_for("phase0.before: ");
    assert!(mac.changes().is_empty());
}

/// The earlier tool's record: the Mac's originals, before it set phase 0.
const EARLIER: &str = "\
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticDownload\tABSENT
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tAutomaticallyInstallMacOSUpdates\t1
pref\t/Library/Preferences/com.apple.SoftwareUpdate\tCriticalUpdateInstall\t1
pref\t/Library/Preferences/com.apple.commerce\tAutoUpdate\t1
pmset\t-\tautorestart\t0
pmset\t-\tsleep\t10
";

/// Catches: `apply` on a Mac where the earlier phase-0 tool already applied phase 0
/// recording the values it finds (those the earlier tool set), so `revert` could
/// never restore the originals; or leaving the earlier record where it would count a
/// second time.
#[test]
fn apply_takes_over_the_earlier_tools_record() {
    let mac = FakeMac::new("adopt");
    let earlier = mac.earlier_record(EARLIER);
    let ran = mac.run(&["phase0", "apply"]);
    ran.succeeded();
    assert_eq!(fs::read_to_string(mac.record()).unwrap(), EARLIER);
    assert!(!earlier.exists());
    let aside = earlier.with_file_name("phase0.before.adopted-20270115T080000Z");
    assert_eq!(fs::read_to_string(&aside).unwrap(), EARLIER);
    assert!(ran.out.contains("record: took over "), "{}", ran.out);
    assert!(!ran.out.contains("record: wrote"), "{}", ran.out);
    mac.clear_calls();
    mac.run(&["phase0", "revert"]).succeeded();
    assert_eq!(mac.pref(SOFTWARE_UPDATE, "AutomaticDownload"), None);
    assert_eq!(
        mac.pref(SOFTWARE_UPDATE, "CriticalUpdateInstall")
            .as_deref(),
        Some("1")
    );
    assert_eq!(mac.pmset("sleep"), "10");
}

/// Catches: `apply` picking one of two records (either may hold the values the other
/// tool set), or taking over an earlier record that is not root's or not valid.
#[test]
fn apply_refuses_an_earlier_record_it_cannot_trust() {
    let mac = FakeMac::new("adopt-both");
    applied(&mac);
    let earlier = mac.earlier_record(EARLIER);
    mac.run(&["phase0", "apply"])
        .refused_for("it is not clear which holds the Mac's original values");
    assert!(earlier.exists());
    let mac = FakeMac::new("adopt-owner");
    let earlier = mac.earlier_record(EARLIER);
    mac.own(&earlier, 501);
    mac.run(&["phase0", "apply"])
        .refused_for("phase0.before is owned by uid 501, not root");
    let folder = earlier.parent().unwrap().to_owned();
    mac.own(&earlier, 0);
    mac.own(&folder, 501);
    mac.run(&["phase0", "apply"])
        .refused_for("kbf-mdm is owned by uid 501, not root");
    let mac = FakeMac::new("adopt-invalid");
    mac.earlier_record(&format!("{EARLIER}pmset\t-\tsleep\t0\n"));
    mac.run(&["phase0", "apply"])
        .refused_for("sleep is listed twice");
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
    assert!(!mac.record().exists());
}

/// Catches: a taken-over record left in place when it cannot be moved aside.
#[test]
fn an_earlier_record_that_cannot_be_moved_fails_apply() {
    let mac = FakeMac::new("adopt-stuck");
    let earlier = mac.earlier_record(EARLIER);
    let aside = earlier.with_file_name("phase0.before.adopted-20270115T080000Z");
    fs::create_dir_all(aside.join("x")).unwrap();
    mac.run(&["phase0", "apply"])
        .failed_for("kbf-mdm/phase0.before: ");
    assert!(mac.changes().is_empty(), "{:?}", mac.changes());
}
