//! The script's `t_drift` cases: on a converged fake Mac, each drift fails exactly its
//! item, and the FAIL line says what drifted; then `apply` fixes it, or leaves it and
//! changes nothing.

mod support;

use std::fs;

use support::{
    COMMERCE, DROPIN, ERR_LOG, FakeMac, HOMES, INTENT, LOGINWINDOW, PLIST, SOFTWARE_UPDATE, VOLUME,
    apply, check, lines, only_fails,
};

type Drift = (&'static str, &'static str, fn(&mut FakeMac), &'static str);

/// The script's `t_drift` cases: item, what changed, the change, text the FAIL line holds.
const DRIFTS: &[Drift] = &[
    (
        "macos_version",
        "another version",
        |m| {
            m.state_mut().sw_vers.insert("version", "27.0.2".into());
        },
        "[check only: apply does not change this]",
    ),
    (
        "macos_build",
        "another build",
        |m| {
            m.state_mut().sw_vers.insert("build", "26A001".into());
        },
        "build 26A001, the profile pins 26A000",
    ),
    (
        "hostname",
        "ComputerName",
        |m| {
            m.state_mut()
                .scutil
                .insert("ComputerName".into(), "other".into());
        },
        "found ComputerName=other",
    ),
    (
        "admin_user",
        "a listed administrator",
        |m| {
            m.state_mut().attr(
                "/Groups/admin",
                "GroupMembership",
                "root farmadmin intruder",
            )
        },
        "other administrators: intruder",
    ),
    (
        "admin_user",
        "not in the group",
        |m| {
            m.state_mut()
                .attr("/Groups/admin", "GroupMembership", "root")
        },
        "farmadmin is not in the admin group",
    ),
    (
        "admin_user",
        "no account",
        |m| {
            m.state_mut().remove_user("farmadmin");
        },
        "no account farmadmin",
    ),
    (
        "admin_user",
        "an administrator by primary group",
        |m| {
            m.state_mut().user("hidden", 502);
            m.state_mut().user_attr("hidden", "PrimaryGroupID", "80");
        },
        "other administrators: hidden",
    ),
    (
        "role_user",
        "a shell",
        |m| m.state_mut().user_attr("_kbf", "UserShell", "/bin/zsh"),
        "UserShell=/bin/zsh",
    ),
    (
        "role_user",
        "a password",
        |m| m.state_mut().user_attr("_kbf", "Password", "********"),
        "Password=********",
    ),
    (
        "role_user",
        "a password hash",
        |m| {
            m.state_mut().user_attr(
                "_kbf",
                "AuthenticationAuthority",
                ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>",
            )
        },
        "AuthenticationAuthority=present",
    ),
    (
        "role_user",
        "a user with the id",
        |m| m.state_mut().user("other", 480),
        "uid 480 belongs to other",
    ),
    (
        "role_user",
        "a group with the id",
        |m| m.state_mut().attr("/Groups/other", "PrimaryGroupID", "480"),
        "gid 480 belongs to other",
    ),
    (
        "kbf_volume",
        "not mounted",
        |m| {
            m.state_mut().volumes.clear();
        },
        "/Volumes/kbf is not a mounted volume (Could not find disk: /Volumes/kbf)",
    ),
    (
        "kbf_volume",
        "not APFS",
        |m| {
            m.state_mut()
                .volumes
                .insert(VOLUME.into(), "Journaled HFS+".into());
        },
        "/Volumes/kbf is Journaled HFS+, not APFS",
    ),
    (
        "kbf_volume",
        "Spotlight on",
        |m| {
            m.state_mut().spotlight_off.clear();
        },
        "Spotlight indexing is on",
    ),
    (
        "kbf_volume",
        "no marker",
        |m| m.remove("/Volumes/kbf/.metadata_never_index"),
        "no /Volumes/kbf/.metadata_never_index",
    ),
    (
        "kbf_volume",
        "backed up",
        |m| {
            m.state_mut().backup_excluded.clear();
        },
        "not excluded from Time Machine",
    ),
    (
        "pool_users",
        "a login shell",
        |m| {
            m.state_mut()
                .user_attr("kbf-pool-1", "UserShell", "/bin/zsh")
        },
        "kbf-pool-1: UserShell=/bin/zsh, want /usr/bin/false",
    ),
    (
        "pool_users",
        "in wheel",
        |m| {
            m.state_mut()
                .attr("/Groups/wheel", "GroupMembership", "root kbf-pool-2")
        },
        "kbf-pool-2: in wheel",
    ),
    (
        "pool_users",
        "a uid below 600",
        |m| m.state_mut().user_attr("kbf-pool-0", "UniqueID", "599"),
        "kbf-pool-0: UniqueID=599, want 700",
    ),
    (
        "pool_users",
        "a home off the volume",
        |m| {
            m.state_mut()
                .user_attr("kbf-pool-3", "NFSHomeDirectory", "/var/kbf-pool-3")
        },
        "kbf-pool-3: NFSHomeDirectory=/var/kbf-pool-3, want /Volumes/kbf/home/kbf-pool-3",
    ),
    (
        "pool_users",
        "not hidden",
        |m| m.state_mut().user_attr("kbf-pool-0", "IsHidden", "0"),
        "kbf-pool-0: IsHidden=0, want 1",
    ),
    (
        "pool_users",
        "a password",
        |m| {
            m.state_mut()
                .user_attr("kbf-pool-0", "Password", "********")
        },
        "kbf-pool-0: Password=********, want *",
    ),
    (
        "pool_users",
        "a password hash",
        |m| {
            m.state_mut().user_attr(
                "kbf-pool-2",
                "AuthenticationAuthority",
                ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>",
            )
        },
        "kbf-pool-2: an AuthenticationAuthority",
    ),
    (
        "pool_users",
        "another primary group",
        |m| {
            m.state_mut()
                .user_attr("kbf-pool-1", "PrimaryGroupID", "20")
        },
        "kbf-pool-1: PrimaryGroupID=20, want 700",
    ),
    (
        "pool_users",
        "a user missing",
        |m| m.state_mut().remove_user("kbf-pool-3"),
        "kbf-pool-3: UniqueID=absent, want 703",
    ),
    (
        "pool_users",
        "a user with a pool uid",
        |m| m.state_mut().user("other", 701),
        "kbf-pool-1: uid 701 belongs to other",
    ),
    (
        "pool_users",
        "the group's gid",
        |m| {
            m.state_mut()
                .attr("/Groups/kbfpool", "PrimaryGroupID", "20")
        },
        "group kbfpool: PrimaryGroupID=20, want 700",
    ),
    (
        "pool_users",
        "a group with the pool gid",
        |m| m.state_mut().attr("/Groups/other", "PrimaryGroupID", "700"),
        "gid 700 belongs to other",
    ),
    (
        "pool_users",
        "a home missing",
        |m| m.remove("/Volumes/kbf/home/kbf-pool-2"),
        "kbf-pool-2: home /Volumes/kbf/home/kbf-pool-2 is missing, want drwx------ 702 700",
    ),
    (
        "pool_users",
        "a home of another owner",
        |m| m.own("/Volumes/kbf/home/kbf-pool-1", 700, 700),
        "kbf-pool-1: home /Volumes/kbf/home/kbf-pool-1 is drwx------ 700 700, want drwx------ 701 700",
    ),
    (
        "pool_users",
        "a home others can read",
        |m| m.chmod("/Volumes/kbf/home/kbf-pool-0", 0o755),
        "want drwx------ 700 700",
    ),
    (
        "pool_users",
        "the homes directory writable",
        |m| m.chmod(HOMES, 0o777),
        "/Volumes/kbf/home is drwxrwxrwx 0 0, want drwxr-xr-x 0 0",
    ),
    (
        "power_sleep",
        "sleep on",
        |m| m.state_mut().pmset_set("sleep", "1"),
        "sleep is 1, want 0",
    ),
    (
        "power_standby",
        "standby on",
        |m| m.state_mut().pmset_set("standby", "1"),
        "standby is 1, want 0",
    ),
    (
        "power_powernap",
        "power nap on",
        |m| m.state_mut().pmset_set("powernap", "1"),
        "powernap is 1, want 0",
    ),
    (
        "power_autorestart",
        "no restart",
        |m| m.state_mut().pmset_set("autorestart", "0"),
        "autorestart is 0, want 1",
    ),
    (
        "power_womp",
        "no wake on LAN",
        |m| m.state_mut().pmset_set("womp", "0"),
        "womp is 0, want 1",
    ),
    (
        "power_sleep",
        "not reported",
        |m| m.state_mut().pmset.retain(|(k, _)| k != "sleep"),
        "sleep is not reported",
    ),
    (
        "updates_auto_download",
        "downloads",
        |m| {
            m.state_mut()
                .default_set(SOFTWARE_UPDATE, "AutomaticDownload", "1")
        },
        "AutomaticDownload is 1, want 0",
    ),
    (
        "updates_auto_install_macos",
        "installs macOS",
        |m| {
            m.state_mut()
                .default_set(SOFTWARE_UPDATE, "AutomaticallyInstallMacOSUpdates", "1")
        },
        "AutomaticallyInstallMacOSUpdates is 1",
    ),
    (
        "updates_auto_install_security",
        "installs security updates",
        |m| {
            m.state_mut()
                .default_set(SOFTWARE_UPDATE, "CriticalUpdateInstall", "1")
        },
        "CriticalUpdateInstall is 1",
    ),
    (
        "updates_config_data",
        "no config data",
        |m| {
            m.state_mut()
                .default_set(SOFTWARE_UPDATE, "ConfigDataInstall", "0")
        },
        "ConfigDataInstall is 0",
    ),
    (
        "appstore_auto_update",
        "App Store updates",
        |m| {
            m.state_mut()
                .default_set(SOFTWARE_UPDATE, "AutomaticallyInstallAppUpdates", "1")
        },
        "AutomaticallyInstallAppUpdates is 1",
    ),
    (
        "appstore_auto_update",
        "the older App Store switch",
        |m| m.state_mut().default_set(COMMERCE, "AutoUpdate", "1"),
        "com.apple.commerce AutoUpdate is 1, want 0",
    ),
    (
        "updates_auto_download",
        "unset",
        |m| {
            m.state_mut()
                .defaults
                .remove(&(SOFTWARE_UPDATE.into(), "AutomaticDownload".into()));
        },
        "AutomaticDownload is unset",
    ),
    (
        "remote_login",
        "off",
        |m| m.state_mut().remote_login = false,
        "want Remote Login: On, found Remote Login: Off",
    ),
    (
        "ssh_password_auth",
        "drop-in content",
        |m| m.write(DROPIN, "PasswordAuthentication yes\n"),
        "000-kbf.conf differs from the profile",
    ),
    (
        "ssh_password_auth",
        "drop-in owner",
        |m| m.own(DROPIN, 1000, 1000),
        "000-kbf.conf is -rw-r--r-- 1000 1000, want -rw-r--r-- 0 0",
    ),
    (
        "ssh_password_auth",
        "drop-in mode",
        |m| m.chmod(DROPIN, 0o664),
        "is -rw-rw-r-- 0 0",
    ),
    (
        "ssh_password_auth",
        "no Include line",
        |m| m.write("/etc/ssh/sshd_config", "# no include\n"),
        "lacks the line 'Include /etc/ssh/sshd_config.d/*'",
    ),
    (
        "ssh_password_auth",
        "an earlier drop-in",
        |m| {
            m.write(
                "/etc/ssh/sshd_config.d/00-x.conf",
                "PasswordAuthentication yes\n",
            )
        },
        "sshd reads 00-x.conf before 000-kbf.conf",
    ),
    (
        "ssh_password_auth",
        "a keyword before Include",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "PasswordAuthentication yes\nInclude /etc/ssh/sshd_config.d/*\n",
            )
        },
        "sets PasswordAuthentication before its Include line",
    ),
    (
        "ssh_password_auth",
        "a Keyword=value before Include",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "  permitRootLogin=yes\nInclude /etc/ssh/sshd_config.d/*\n",
            )
        },
        "sets permitRootLogin=yes before its Include line",
    ),
    (
        "ssh_password_auth",
        "what sshd -T reports",
        |m| {
            m.state_mut().sshd_effective = Some(
                "passwordauthentication yes\nkbdinteractiveauthentication no\npermitrootlogin no\n"
                    .into(),
            )
        },
        "sshd -T reports 'kbdinteractiveauthentication no;passwordauthentication yes;permitrootlogin no'",
    ),
    (
        "firewall",
        "off",
        |m| m.state_mut().firewall = "0".into(),
        "firewall off, want on",
    ),
    (
        "firewall",
        "no answer",
        |m| m.state_mut().firewall = "broken".into(),
        "firewall unknown, want on",
    ),
    (
        "filevault",
        "on",
        |m| m.state_mut().filevault = "On".into(),
        "FileVault on, want off [check only",
    ),
    (
        "filevault",
        "decrypting",
        |m| m.state_mut().filevault = "Decrypting".into(),
        "FileVault unknown (FileVault is Decrypting.)",
    ),
    (
        "autologin",
        "an administrator with kcpassword",
        |m| {
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "farmadmin");
            m.write("/etc/kcpassword", "x");
        },
        "auto-login is farmadmin (uid 501); only a kbf-lease-* user in uids 600-699 may be",
    ),
    (
        "autologin",
        "an administrator",
        |m| {
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "farmadmin")
        },
        "auto-login is farmadmin",
    ),
    (
        "autologin",
        "a stale kcpassword",
        |m| m.write("/etc/kcpassword", "x"),
        "off, but /etc/kcpassword is left over",
    ),
    (
        "autologin",
        "a lease user out of range",
        |m| {
            m.state_mut().user("kbf-lease-9-9", 700);
            m.write(INTENT, "9.9\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-9-9");
        },
        "auto-login is kbf-lease-9-9 (uid 700)",
    ),
    (
        "autologin",
        "a lease user left behind",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
            m.write("/etc/kcpassword", "x");
        },
        "auto-login is kbf-lease-4-2 (uid 642), but no kbf-mac-session intent record names its lease: a lease user left behind; apply clears auto-login and /etc/kcpassword",
    ),
    (
        "autologin",
        "an intent record for another lease",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            m.write(INTENT, "4.3\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    ),
    (
        "autologin",
        "an intent record of two lines",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            m.write(INTENT, "4.2\n4.3\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    ),
    (
        "autologin",
        "an intent record with a leading zero",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            m.write(INTENT, "04.2\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    ),
    (
        "autologin",
        "an intent record that is a link",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            let target = m.path("/var/db/elsewhere");
            fs::write(&target, "4.2\n").unwrap();
            m.symlink(&target, INTENT);
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    ),
    (
        "autologin",
        "a user in range not named kbf-lease-*",
        |m| {
            m.state_mut().user("lease9", 650);
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "lease9");
        },
        "auto-login is lease9 (uid 650)",
    ),
    (
        "autologin",
        "a lease user with no record",
        |m| {
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-gone")
        },
        "auto-login is kbf-lease-gone (uid none)",
    ),
    (
        "time_server",
        "network time off",
        |m| m.state_mut().network_time = false,
        "network time is not on (Network Time: Off)",
    ),
    (
        "time_server",
        "another server",
        |m| m.state_mut().time_server = "time.apple.com".into(),
        "want time.example.net, found Network Time Server: time.apple.com",
    ),
    (
        "time_max_offset_ms",
        "1.5 s fast",
        |m| m.state_mut().sntp_offset = "+1.500000".into(),
        "1500 ms from time.example.net, more than 250",
    ),
    (
        "time_max_offset_ms",
        "0.9 s slow",
        |m| m.state_mut().sntp_offset = "-0.900000".into(),
        "900 ms from time.example.net, more than 250",
    ),
    (
        "time_max_offset_ms",
        "no answer",
        |m| m.state_mut().sntp_offset = "fail".into(),
        "no answer from time.example.net",
    ),
    (
        "xcode",
        "another build",
        |m| m.install_xcode("Xcode-26.6.app", "17A001"),
        "/Applications/Xcode-26.6.app has 17A001, want 17A000;",
    ),
    (
        "xcode",
        "a pinned Xcode removed",
        |m| m.remove("/Applications/Xcode-27.0.app"),
        "/Applications/Xcode-27.0.app has no Xcode, want 18A000;",
    ),
    (
        "xcode",
        "an unpinned Xcode",
        |m| m.mkdir("/Applications/Xcode-beta.app"),
        "/Applications/Xcode-beta.app is not pinned;",
    ),
    (
        "default_developer_dir",
        "another Xcode",
        |m| {
            m.state_mut().xcode_select =
                Some("/Applications/Xcode-26.6.app/Contents/Developer".into())
        },
        "xcode-select -p is /Applications/Xcode-26.6.app/Contents/Developer, want /Applications/Xcode-27.0.app/Contents/Developer",
    ),
    (
        "default_developer_dir",
        "unset",
        |m| m.state_mut().xcode_select = None,
        "xcode-select -p is unset",
    ),
    (
        "probe",
        "a changed probe",
        |m| {
            m.write(
                "/usr/local/kbf/probes/host_identity.sh",
                "echo identity\nchanged\n",
            )
        },
        "host_identity: /usr/local/kbf/probes/host_identity.sh is missing or has another SHA-256; [check only",
    ),
    (
        "probe",
        "a missing probe",
        |m| m.remove("/usr/local/kbf/probes/host_identity.sh"),
        "is missing or has another SHA-256",
    ),
    (
        "simulator_runtimes",
        "a runtime image",
        |m| m.write("/Library/Developer/CoreSimulator/Images/x.dmg", ""),
        "simulator runtimes on the host: /Library/Developer/CoreSimulator/Images/x.dmg (they belong in VM images; erase the node)",
    ),
    (
        "simulator_runtimes",
        "a runtime bundle",
        |m| m.mkdir("/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS.simruntime"),
        "/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS.simruntime",
    ),
    (
        "simulator_runtimes",
        "a mounted runtime",
        |m| m.mkdir("/Library/Developer/CoreSimulator/Volumes/iOS_23A"),
        "/Library/Developer/CoreSimulator/Volumes/iOS_23A",
    ),
    (
        "launchd_label",
        "a Standard job",
        |m| {
            let t = fs::read_to_string(m.path(PLIST)).unwrap();
            m.write(PLIST, &t.replace("Interactive", "Standard"));
        },
        "/Library/LaunchDaemons/org.example.kbf-daemon.plist is missing or differs from the profile",
    ),
    (
        "launchd_label",
        "no property list",
        |m| m.remove(PLIST),
        "is missing or differs from the profile",
    ),
    (
        "launchd_label",
        "the property list's owner",
        |m| m.own(PLIST, 1000, 1000),
        "org.example.kbf-daemon.plist is -rw-r--r-- 1000 1000, want -rw-r--r-- 0 0",
    ),
    (
        "launchd_label",
        "the log directory's owner",
        |m| m.own("/Library/Logs/kbf", 480, 480),
        "/Library/Logs/kbf is drwxr-xr-x 480 480, want drwxr-xr-x 0 0",
    ),
    (
        "launchd_label",
        "the error log root's",
        |m| m.own(ERR_LOG, 0, 0),
        "/Library/Logs/kbf/kbf-daemon.err.log is -rw-r--r-- 0 0, want a regular file with one link, -rw-r--r-- 480 480",
    ),
    (
        "launchd_label",
        "the error log's mode",
        |m| m.chmod(ERR_LOG, 0o600),
        "kbf-daemon.err.log is -rw------- 480 480, want -rw-r--r-- 480 480",
    ),
    (
        "launchd_label",
        "no error log",
        |m| m.remove(ERR_LOG),
        "kbf-daemon.err.log is missing, want a regular file with one link",
    ),
    (
        "launchd_label",
        "the job not loaded",
        |m| {
            m.state_mut().loaded.clear();
        },
        "system/org.example.kbf-daemon is not loaded",
    ),
];

/// Catches: a drift that check passes, a drift that fails another item as well, or a
/// FAIL line that does not say what drifted (the script's `t_drift`, check part).
#[test]
fn each_drift_fails_exactly_its_item() {
    let wrong: Vec<String> = DRIFTS
        .iter()
        .enumerate()
        .filter_map(|(n, (item, what, drift, text))| {
            only_fails(&format!("drift-{n}"), item, *drift, text)
                .err()
                .map(|e| format!("{item} ({what}): {e}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The drifts of items that are not check only which `apply` leaves: an id another
/// account holds, and an sshd configuration that is the operator's to change (`apply`
/// writes only kbf's drop-in).
const STAYS: &[(&str, &str)] = &[
    ("role_user", "a user with the id"),
    ("role_user", "a group with the id"),
    ("kbf_volume", "not mounted"),
    ("kbf_volume", "not APFS"),
    ("pool_users", "a user with a pool uid"),
    ("pool_users", "a group with the pool gid"),
    ("ssh_password_auth", "no Include line"),
    ("ssh_password_auth", "an earlier drop-in"),
    ("ssh_password_auth", "a keyword before Include"),
    ("ssh_password_auth", "a Keyword=value before Include"),
    ("ssh_password_auth", "what sshd -T reports"),
];

/// Catches: a drift `apply` does not fix (no CHANGE line for it, or check still fails
/// after it), a fix that breaks another item, a check only item `apply` changes (the
/// script's mutant "apply turns FileVault off"), an apply that exits 0 while an item
/// still fails ("apply exits 0 when something still fails"), and a drift apply should
/// leave that it changes (the script's `t_drift`, apply part).
#[test]
fn apply_fixes_each_drift_or_leaves_it_and_changes_nothing() {
    assert!(
        STAYS
            .iter()
            .all(|s| DRIFTS.iter().any(|(i, w, ..)| (*i, *w) == *s))
    );
    let wrong: Vec<String> = DRIFTS
        .iter()
        .enumerate()
        .filter_map(|(n, (item, what, drift, _))| {
            let mut mac = FakeMac::converged(&format!("apply-drift-{n}"));
            drift(&mut mac);
            mac.log.borrow_mut().clear();
            let r = apply(&mac);
            let check_only = kbf_mac_provision::items::ITEMS
                .iter()
                .any(|i| i.key == *item && i.check_only());
            let stays = check_only || STAYS.contains(&(item, what));
            let ok = if stays {
                let failed = lines(&r, "FAIL");
                r.failed == 1
                    && failed.len() == 1
                    && failed[0].starts_with(&format!("FAIL {item}: "))
                    && mac.writes().is_empty()
            } else {
                r.failed == 0
                    && r.text.contains(&format!("CHANGE {item}: "))
                    && check(&mac).failed == 0
            };
            (!ok).then(|| format!("{item} ({what}, stays: {stays}):\n{}", r.text))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
