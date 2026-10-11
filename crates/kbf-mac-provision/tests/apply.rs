//! `apply` against a fake Mac: it converges a fresh Mac in one run, a second run
//! changes nothing, it runs only as root, `--keys` limits it, and it installs an Xcode
//! only from the `.xip` the profile pins (the script's `t_converge`, `t_not_root`,
//! `t_keys`, `t_other_values`, `t_lease_autologin` and `t_xcode_sources`).

mod support;

use std::fs;

use kbf_mac_provision::cli::{self, Error};
use support::{
    DROPIN, ERR_LOG, FakeMac, INTENT, LOGINWINDOW, PLIST, apply, apply_keys, calls, check,
    check_keys, golden_plist, lines, ls, sha256_hex,
};

const RECORD: &str = "/var/db/kbf-mac-provision/settings.before";

/// Catches: an item `apply` does not fix or fixes with another value (a fresh Mac fails
/// check after one apply), a fix that writes another file or owner than check wants,
/// and a CHANGE line that does not say what changed (the script's `t_converge`).
#[test]
fn one_apply_converges_a_fresh_mac() {
    let mac = FakeMac::fresh("apply-fresh");
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        r.text.lines().collect::<Vec<_>>(),
        [
            "CHANGE hostname: HostName = kbf-mac-07",
            "CHANGE hostname: LocalHostName = kbf-mac-07",
            "CHANGE hostname: ComputerName = kbf-mac-07",
            "CHANGE role_user: role account and group _kbf, id 480",
            "CHANGE power_sleep: wrote /var/db/kbf-mac-provision/settings.before (what revert restores)",
            "CHANGE power_sleep: pmset -a sleep 0",
            "CHANGE power_standby: pmset -a standby 0",
            "CHANGE power_powernap: pmset -a powernap 0",
            "CHANGE power_autorestart: pmset -a autorestart 1",
            "CHANGE power_womp: pmset -a womp 1",
            "CHANGE updates_auto_download: AutomaticDownload = 0",
            "CHANGE updates_auto_install_macos: AutomaticallyInstallMacOSUpdates = 0",
            "CHANGE updates_auto_install_security: CriticalUpdateInstall = 0",
            "CHANGE updates_config_data: ConfigDataInstall = 1",
            "CHANGE appstore_auto_update: AutomaticallyInstallAppUpdates = 0",
            "CHANGE appstore_auto_update: com.apple.commerce AutoUpdate = 0",
            "CHANGE ssh_password_auth: wrote /etc/ssh/sshd_config.d/000-kbf.conf",
            "CHANGE firewall: firewall on",
            "CHANGE time_server: network time on, server time.example.net",
            "CHANGE xcode: installed Xcode 17A000 at /Applications/Xcode-26.6.app",
            "CHANGE xcode: installed Xcode 18A000 at /Applications/Xcode-27.0.app",
            "CHANGE default_developer_dir: xcode-select -s /Applications/Xcode-27.0.app/Contents/Developer",
            "CHANGE launchd_label: wrote /Library/LaunchDaemons/org.example.kbf-daemon.plist and restarted the job (error log truncated)",
        ]
    );
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(r.text.ends_with("check: 27 pass, 0 fail\n"), "{}", r.text);
    let sha = sha256_hex(mac.profile_text().as_bytes());
    assert_eq!(
        fs::read_to_string(mac.path(PLIST)).unwrap(),
        golden_plist(&sha[..12])
    );
    assert_eq!(
        fs::read_to_string(mac.path(DROPIN)).unwrap(),
        "PasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin no\n"
    );
    for (path, want) in [
        (PLIST, "-rw-r--r-- 0 0"),
        (DROPIN, "-rw-r--r-- 0 0"),
        ("/Library/Logs/kbf", "drwxr-xr-x 0 0"),
        (ERR_LOG, "-rw-r--r-- 480 480"),
        (RECORD, "-rw------- 0 0"),
        ("/var/db/kbf-mac-provision", "drwx------ 0 0"),
    ] {
        assert_eq!(ls(&mac, path), want, "{path}");
    }
    // Each Xcode's license is accepted and its first launch run, with that Xcode.
    for dir in ["Xcode-26.6.app", "Xcode-27.0.app"] {
        let env = format!("DEVELOPER_DIR=/Applications/{dir}/Contents/Developer xcodebuild ");
        assert_eq!(
            calls(&mac, &env),
            [
                format!("{env}-license accept"),
                format!("{env}-runFirstLaunch")
            ]
        );
    }
    assert!(!mac.path("/Applications/.kbf-xip-17A000").exists());
}

/// Catches: an apply that fixes an item that passes (the script's mutant "apply writes
/// on every run"), rewrites a file that is already right ("apply rewrites a drop-in
/// that is already right"), or records the settings again.
#[test]
fn a_second_apply_prints_nothing_and_changes_nothing() {
    let mac = FakeMac::fresh("apply-twice");
    assert_eq!(apply(&mac).failed, 0);
    let record = fs::read(mac.path(RECORD)).unwrap();
    mac.log.borrow_mut().clear();
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert_eq!(fs::read(mac.path(RECORD)).unwrap(), record);
}

/// Catches: a record written, or anything changed, on a Mac where every item passes.
#[test]
fn apply_on_a_converged_mac_changes_nothing() {
    let mac = FakeMac::converged("apply-converged");
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert!(!mac.path(RECORD).exists());
}

/// Catches: `apply`, `restart` or `revert` running without root (the script's mutant
/// "apply runs without root"), or changing anything before refusing.
#[test]
fn the_changing_verbs_refuse_without_root() {
    let mut mac = FakeMac::fresh("apply-not-root");
    mac.state_mut().euid = 501;
    let path = mac.profile_file(&mac.profile_text());
    assert_eq!(cli::apply(&path, None, &mac), Err(Error::NotRoot("apply")));
    assert_eq!(cli::restart(&path, &mac), Err(Error::NotRoot("restart")));
    assert_eq!(cli::revert(&mac), Err(Error::NotRoot("revert")));
    assert_eq!(Error::NotRoot("apply").to_string(), "apply runs as root");
    assert_eq!(mac.writes(), Vec::<String>::new());
}

/// Catches: `--keys` ignored by apply (it fixes every item), or an unknown item in it
/// accepted (the script's `t_keys`).
#[test]
fn keys_limits_apply_to_the_named_items() {
    let mac = FakeMac::fresh("apply-keys");
    let r = apply_keys(&mac, Some("firewall"));
    assert_eq!(
        (r.failed, r.text.as_str()),
        (0, "CHANGE firewall: firewall on\n")
    );
    let r = check(&mac);
    assert_eq!(lines(&r, "FAIL").len(), 17, "{}", r.text);
    let path = mac.profile_file(&mac.profile_text());
    assert!(matches!(
        cli::apply(&path, Some("firewall,bogus"), &mac),
        Err(Error::Keys(_))
    ));
}

/// Catches: a fix that sets only one value of an on/off key: Remote Login off, or
/// passwords allowed in the drop-in (the script's `t_other_values`).
#[test]
fn apply_sets_the_other_values_of_remote_login_and_ssh() {
    let mac = FakeMac::fresh("apply-other");
    let text = mac
        .profile_text()
        .replace("remote_login=1", "remote_login=0")
        .replace("ssh_password_auth=0", "ssh_password_auth=1")
        .replace("firewall=on", "firewall=off");
    let path = mac.profile_file(&text);
    let keys = Some("remote_login,ssh_password_auth,firewall");
    let r = cli::apply(&path, keys, &mac).unwrap();
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains("CHANGE remote_login: Remote Login off\n"),
        "{}",
        r.text
    );
    assert_eq!(
        fs::read_to_string(mac.path(DROPIN)).unwrap(),
        "PasswordAuthentication yes\nKbdInteractiveAuthentication yes\nPermitRootLogin no\n"
    );
    let r = cli::check(&path, keys, &mac).unwrap();
    assert!(
        r.text.contains("PASS remote_login: Remote Login: Off\n"),
        "{}",
        r.text
    );
    assert!(r.text.contains("PASS firewall: off\n"), "{}", r.text);
}

/// Catches: apply clearing a lease user's auto-login that kbf-mac-session set for a
/// whole_machine lease (the script's mutant "a lease user in range fails (apply clears
/// it mid-lease)", `t_lease_autologin`).
#[test]
fn apply_leaves_a_lease_users_auto_login() {
    let mut mac = FakeMac::converged("apply-lease");
    mac.state_mut().user("kbf-lease-4-2", 642);
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
    mac.write(INTENT, "4.2\n");
    mac.write("/etc/kcpassword", "x");
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert!(mac.path("/etc/kcpassword").exists());
}

/// Catches: a fix that does not clear auto-login and `/etc/kcpassword` both, or clears
/// a kcpassword through a link's target.
#[test]
fn apply_clears_auto_login_and_kcpassword() {
    let mut mac = FakeMac::converged("apply-autologin");
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "farmadmin");
    mac.write("/etc/precious", "keep");
    mac.symlink(&mac.path("/etc/precious"), "/etc/kcpassword");
    let r = apply_keys(&mac, Some("autologin"));
    assert_eq!(
        (r.failed, r.text.as_str()),
        (
            0,
            "CHANGE autologin: auto-login cleared\nCHANGE autologin: removed /etc/kcpassword\n"
        )
    );
    assert_eq!(
        fs::read_to_string(mac.path("/etc/precious")).unwrap(),
        "keep"
    );
}

/// Catches: an Xcode installed from a `.xip` whose SHA-256 is not the pinned one (the
/// script's mutant "an xip is used without its SHA-256"), or from one that expands to
/// another build ("an expanded Xcode is not checked for its build"), a staging
/// directory left behind, or an apply that says nothing when it cannot install (the
/// script's `t_xcode_sources`).
#[test]
fn an_xcode_is_installed_only_from_its_pinned_xip() {
    // No xip directory: nothing to install from.
    let mac = FakeMac::fresh("xcode-no-dir");
    let text: String = mac
        .profile_text()
        .lines()
        .filter(|l| !l.starts_with("xcode_xip_dir="))
        .map(|l| format!("{l}\n"))
        .collect();
    let path = mac.profile_file(&text);
    let r = cli::apply(&path, Some("xcode"), &mac).unwrap();
    assert_eq!(r.failed, 1);
    assert!(
        r.text.contains("NOTE xcode: no .xip with SHA-256 "),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("FAIL xcode: /Applications/Xcode-26.6.app has no Xcode, want 17A000"),
        "{}",
        r.text
    );

    // A tampered xip is never expanded; a directory named *.xip is skipped.
    let mac = FakeMac::fresh("xcode-tampered");
    mac.write(
        "/Library/kbf/xip/Xcode_26.6.xip",
        "app=Xcode-26.6.app build=17A000 tampered\n",
    );
    mac.mkdir("/Library/kbf/xip/dir.xip");
    let r = apply_keys(&mac, Some("xcode"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text.contains("NOTE xcode: no .xip with SHA-256 "),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("CHANGE xcode: installed Xcode 18A000 at /Applications/Xcode-27.0.app"),
        "{}",
        r.text
    );
    assert_eq!(calls(&mac, "xip --expand").len(), 1);
    assert!(calls(&mac, "xip --expand")[0].ends_with("/Library/kbf/xip/Xcode_27.0.xip"));

    // The pinned xip holds another build: not installed, and the staging is removed.
    let mac = FakeMac::fresh("xcode-other-build");
    let profile = mac.profile_text();
    mac.write(
        "/Library/kbf/xip/Xcode_26.6.xip",
        "app=Xcode-26.6.app build=17A999\n",
    );
    let sha = sha256_hex(&fs::read(mac.path("/Library/kbf/xip/Xcode_26.6.xip")).unwrap());
    let old_sha = sha256_hex(b"app=Xcode-26.6.app build=17A000\n");
    let path = mac.profile_file(&profile.replace(&old_sha, &sha));
    let r = cli::apply(&path, Some("xcode"), &mac).unwrap();
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .contains("NOTE xcode: /Library/kbf/xip/Xcode_26.6.xip did not expand to Xcode 17A000"),
        "{}",
        r.text
    );
    assert!(!mac.path("/Applications/.kbf-xip-17A000").exists());
    assert!(!mac.path("/Applications/Xcode-26.6.app").exists());
}

/// Catches: an unpinned Xcode left in place by apply, or a link to a pinned one removed.
#[test]
fn apply_removes_an_unpinned_xcode_and_keeps_links() {
    let mac = FakeMac::converged("xcode-unpinned");
    mac.mkdir("/Applications/Xcode-beta.app");
    mac.symlink(
        &mac.path("/Applications/Xcode-27.0.app"),
        "/Applications/Xcode.app",
    );
    let r = apply(&mac);
    assert_eq!(
        (r.failed, r.text.as_str()),
        (
            0,
            "CHANGE xcode: removed /Applications/Xcode-beta.app, which the profile does not pin\n"
        )
    );
    assert!(mac.path("/Applications/Xcode.app").is_symlink());
}

/// Catches: a fix's file error lost: the error is a NOTE naming the path, and the item
/// still fails.
#[test]
fn a_fix_that_cannot_write_says_why_and_the_item_still_fails() {
    let mac = FakeMac::converged("apply-unwritable");
    mac.remove(PLIST);
    // A file where the LaunchDaemons directory belongs: the property list cannot go there.
    mac.remove("/Library/LaunchDaemons");
    mac.write("/Library/LaunchDaemons", "");
    let r = apply(&mac);
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .starts_with("NOTE launchd_label: /Library/LaunchDaemons: "),
        "{}",
        r.text
    );
    assert!(r.text.contains("FAIL launchd_label: "), "{}", r.text);
}

/// Catches: the role account made over another account that holds its id (the script's
/// mutant "the role id of another account is taken over").
#[test]
fn apply_does_not_take_over_another_accounts_id() {
    let mut mac = FakeMac::fresh("apply-role-taken");
    mac.state_mut().user("other", 480);
    let r = apply_keys(&mac, Some("role_user"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text.contains("FAIL role_user: uid 480 belongs to other"),
        "{}",
        r.text
    );
    assert_eq!(calls(&mac, "dscl . -create"), Vec::<String>::new());
    let r = check_keys(&mac, Some("role_user"));
    assert_eq!(r.failed, 1);
}
