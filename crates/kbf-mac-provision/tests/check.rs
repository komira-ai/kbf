//! `check` against a fake Mac: a fresh Mac fails every item `apply` would set; a
//! converged Mac passes every item; the cases that must pass are not drift. The named
//! tests are the script's defects, one test per defect (`drift.rs` holds the
//! script's drift table).

mod support;

use std::fs;

use kbf_mac_provision::cli;
use support::{
    DROPIN, ERR_LOG, FakeMac, INTENT, LOGINWINDOW, PLIST, assert_only_fails, check, check_keys,
    lines,
};

/// Catches: an item dropped from the table (the script's defects "check skips a key"
/// for `power_womp` and `updates_auto_download`), an item `apply` sets that a fresh Mac
/// passes, or a check that runs a program that changes the Mac (the script's
/// `t_converge`, check part).
#[test]
fn a_fresh_mac_fails_every_item_apply_sets() {
    let mac = FakeMac::fresh("fresh");
    let r = check(&mac);
    let failed: Vec<&str> = lines(&r, "FAIL")
        .iter()
        .map(|l| l["FAIL ".len()..].split(':').next().unwrap())
        .collect();
    assert_eq!(
        failed,
        [
            "hostname",
            "role_user",
            "kbf_volume",
            "pool_users",
            "power_sleep",
            "power_standby",
            "power_powernap",
            "power_autorestart",
            "power_womp",
            "updates_auto_download",
            "updates_auto_install_macos",
            "updates_auto_install_security",
            "updates_config_data",
            "appstore_auto_update",
            "xcode_disable_manifest_sandbox",
            "ssh_password_auth",
            "firewall",
            "time_server",
            "xcode",
            "default_developer_dir",
            "launchd_label",
        ],
        "{}",
        r.text
    );
    assert!(r.text.ends_with("check: 12 pass, 21 fail\n"), "{}", r.text);
    assert_eq!(r.text.lines().count(), 34);
    assert_eq!(mac.writes(), Vec::<String>::new());
}

/// Catches: a converged Mac failing an item: a check reading the wrong program, path or
/// format (the script's defects "pmset keys match in any case", "IsHidden read without
/// the dsAttrTypeNative: prefix", "the sshd drop-in sorts after the system one").
#[test]
fn a_converged_mac_passes_every_item() {
    let mac = FakeMac::converged("converged");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(r.text.ends_with("check: 33 pass, 0 fail\n"), "{}", r.text);
    for want in [
        "PASS updates_auto_download: AutomaticDownload = 0 (the SoftwareUpdate payload is removed in macOS 27",
        "PASS filevault: off",
        "PASS role_user: _kbf (uid and gid 480, hidden, no shell, no password)",
        "PASS kbf_volume: /Volumes/kbf: APFS, Spotlight off, excluded from Time Machine",
        "PASS pool_users: 4 users kbf-pool-0..kbf-pool-3 (uids 700-703, group kbfpool 700): hidden, standard, shell /usr/bin/false, no password, homes in /Volumes/kbf/home",
        "PASS daemon_user: role: kbf-daemon runs as _kbf; the node key and the scratch and cache roots are checked once daemon_user=root",
        "PASS autologin: off",
        "PASS power_sleep: sleep 0",
        "PASS xcode: pinned: 17A000 18A000",
        "PASS probe: host_identity",
        "PASS ssh_password_auth: PasswordAuthentication no;KbdInteractiveAuthentication no;PermitRootLogin no (sshd -T agrees)",
        "PASS launchd_label: system/org.example.kbf-daemon loaded, as _kbf, ProcessType Interactive",
        "PASS time_max_offset_ms: 4 ms from time.example.net",
    ] {
        assert!(r.text.contains(want), "lacks {want:?}:\n{}", r.text);
    }
    assert_eq!(mac.writes(), Vec::<String>::new());
}

/// Catches: a rendering defect in the job's property list: another `ProcessType` (the
/// script's defect "the job runs as a Standard process"), or no `UserName` ("the job runs
/// as root").
#[test]
fn the_job_renders_as_the_golden_property_list() {
    let mac = FakeMac::fresh("golden");
    let text = mac.profile_text();
    let p = kbf_mac_provision::profile::Profile::parse(text.as_bytes()).unwrap();
    assert_eq!(
        kbf_mac_provision::launchd::render_plist(&p),
        support::golden_plist(p.short_sha())
    );
    assert_eq!(kbf_mac_provision::launchd::plist_path(&p), PLIST);
}

/// Catches: a rendering defect in the node's `kbf.toml`: a key in the wrong table, a
/// missing `mode` or `driver`, a `label` key back, a value of another profile
/// key. The golden is written by hand (`support/golden.rs`), not by the renderer.
#[test]
fn the_node_config_renders_as_the_golden_file() {
    let mac = FakeMac::fresh("golden-config");
    let text = mac.profile_text();
    let p = kbf_mac_provision::profile::Profile::parse(text.as_bytes()).unwrap();
    assert_eq!(
        kbf_mac_provision::launchd::render_config(&p),
        support::golden_config()
    );
    assert_eq!(kbf_mac_provision::launchd::NODE_CONFIG, support::CONFIG);
}

/// Catches: the node's `kbf.toml` carrying a node label again, from any profile: a
/// `label` key in any table (the next daemon release removes the key, and one that refuses
/// it would then fail to start), for the test profile, the example, a root daemon and a smaller pool.
/// Read as lines, not as the golden, so a label put back with a golden updated to match
/// still fails here.
#[test]
fn the_node_config_has_no_label_key_for_any_profile() {
    let mac = FakeMac::fresh("no-label-key");
    let base = mac.profile_text();
    let texts = [
        base.clone(),
        format!("{base}daemon_user=root\n"),
        base.replace("pool_users=4", "pool_users=2"),
        support::EXAMPLE.to_owned(),
    ];
    for text in texts {
        let p = kbf_mac_provision::profile::Profile::parse(text.as_bytes()).unwrap();
        let config = kbf_mac_provision::launchd::render_config(&p);
        for line in config.lines() {
            let key = line.split('=').next().unwrap().trim();
            assert_ne!(key, "label", "a label key in\n{config}");
        }
        assert!(
            !config.contains("profile=") && !config.contains("pool=mac"),
            "{config}"
        );
    }
}

/// Catches: entries that are not drift reported as drift: an images list beside the
/// runtime images, a link to a pinned Xcode (the script's defect "a link to an Xcode
/// counts as unpinned"), a commented keyword before the Include line, a drop-in that
/// sorts after ours, a dangling link among the drop-ins (the script's
/// `t_quiet_entries`).
#[test]
fn entries_that_are_not_drift_pass() {
    let mac = FakeMac::converged("quiet");
    mac.write("/Library/Developer/CoreSimulator/Images/images.plist", "");
    mac.write("/Library/Developer/CoreSimulator/Images/.hidden.dmg", "");
    mac.symlink(
        &mac.path("/Applications/Xcode-27.0.app"),
        "/Applications/Xcode.app",
    );
    mac.write("/Applications/Xcode-notes.app", "");
    mac.write("/Applications/Xcode.zip", "");
    mac.mkdir("/Applications/Safari.app");
    mac.write(
        "/etc/ssh/sshd_config",
        "#PasswordAuthentication yes\nInclude /etc/ssh/sshd_config.d/*\n",
    );
    mac.write(
        "/etc/ssh/sshd_config.d/100-macos.conf",
        "PasswordAuthentication yes\n",
    );
    mac.symlink(&mac.path("/nowhere"), "/etc/ssh/sshd_config.d/00-gone.conf");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
}

/// Catches: a lease user's auto-login failed, so `apply` would clear it mid-lease (the
/// script's defect "a lease user in range fails"; `t_lease_autologin`, check part).
#[test]
fn a_lease_user_named_by_the_intent_record_passes() {
    let mut mac = FakeMac::converged("lease-autologin");
    mac.state_mut().user("kbf-lease-4-2", 642);
    mac.write(INTENT, "4.2\n");
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
    mac.write("/etc/kcpassword", "x");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains("PASS autologin: lease user kbf-lease-4-2 (uid 642), set by kbf-mac-session for a whole_machine lease"),
        "{}",
        r.text
    );
    let mut lease0 = FakeMac::converged("lease-autologin-0");
    lease0.state_mut().user("kbf-lease-0-0", 600);
    lease0.write(INTENT, "0.0\n");
    lease0
        .state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-0-0");
    assert_eq!(check(&lease0).failed, 0);
}

/// Catches: any `kbf-lease-*` user passing whatever its uid (the script's defect), here
/// one past each end of `lease_uids`.
#[test]
fn a_lease_user_outside_lease_uids_fails() {
    assert_only_fails(
        "autologin",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 599);
            m.write(INTENT, "4.2\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "auto-login is kbf-lease-4-2 (uid 599); only a kbf-lease-* user",
    );
    assert_only_fails(
        "autologin",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 900);
            m.write(INTENT, "4.2\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "(uid 900); only",
    );
}

/// Catches: any auto-login user in range passing whatever its name (the script's
/// defect), even one the intent record would name if it were a lease user.
#[test]
fn an_auto_login_user_not_named_kbf_lease_fails() {
    assert_only_fails(
        "autologin",
        |m| {
            m.state_mut().user("lease-4-2", 642);
            m.write(INTENT, "4.2\n");
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "lease-4-2");
        },
        "auto-login is lease-4-2 (uid 642); only",
    );
}

/// Catches: a lease user in range passing with no intent record (the script's defect
/// "with or without an intent record").
#[test]
fn a_lease_user_no_intent_record_names_fails() {
    assert_only_fails(
        "autologin",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    );
}

/// Catches: the intent record read through a symbolic link (the script's defect), so a
/// planted link names a lease.
#[test]
fn an_intent_record_that_is_a_link_is_ignored() {
    assert_only_fails(
        "autologin",
        |m| {
            m.state_mut().user("kbf-lease-4-2", 642);
            let target = m.path("/var/db/record");
            fs::write(&target, "4.2\n").unwrap();
            m.symlink(&target, INTENT);
            m.state_mut()
                .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        },
        "a lease user left behind",
    );
}

/// Catches: a `/etc/kcpassword` left with auto-login off passing (the script's defect
/// "a stale /etc/kcpassword is ignored").
#[test]
fn a_kcpassword_left_with_auto_login_off_fails() {
    assert_only_fails(
        "autologin",
        |m| m.write("/etc/kcpassword", "x"),
        "off, but /etc/kcpassword is left over",
    );
}

/// Catches: the property list's content not compared (the script's defect "the job
/// file is not compared").
#[test]
fn a_property_list_that_differs_fails() {
    assert_only_fails(
        "launchd_label",
        |m| {
            let t = fs::read_to_string(m.path(PLIST)).unwrap();
            m.write(
                PLIST,
                &t.replace("<integer>2147483647</integer>", "<integer>5</integer>"),
            );
        },
        "is missing or differs from the profile",
    );
}

/// Catches: the job not checked as loaded (the script's defect).
#[test]
fn a_job_that_is_not_loaded_fails() {
    assert_only_fails(
        "launchd_label",
        |m| m.state_mut().loaded.clear(),
        "system/org.example.kbf-daemon is not loaded",
    );
}

/// Catches: a simulator runtime image ignored (the script's defect).
#[test]
fn a_simulator_runtime_image_fails() {
    assert_only_fails(
        "simulator_runtimes",
        |m| m.write("/Library/Developer/CoreSimulator/Images/iOS.dmg", ""),
        "/Library/Developer/CoreSimulator/Images/iOS.dmg",
    );
}

/// Catches: a mounted simulator runtime ignored (the script's defect).
#[test]
fn a_mounted_simulator_runtime_fails() {
    assert_only_fails(
        "simulator_runtimes",
        |m| m.mkdir("/Library/Developer/CoreSimulator/Volumes/iOS_23A"),
        "/Library/Developer/CoreSimulator/Volumes/iOS_23A",
    );
}

/// Catches: an Xcode of another build passing (the script's defect).
#[test]
fn an_xcode_of_another_build_fails() {
    assert_only_fails(
        "xcode",
        |m| m.install_xcode("Xcode-27.0.app", "18A001"),
        "/Applications/Xcode-27.0.app has 18A001, want 18A000;",
    );
}

/// Catches: an unpinned Xcode ignored (the script's defect).
#[test]
fn an_unpinned_xcode_fails() {
    assert_only_fails(
        "xcode",
        |m| m.mkdir("/Applications/Xcode.app"),
        "/Applications/Xcode.app is not pinned;",
    );
}

/// Catches: one of the three names scutil keeps not checked (the script's defect drops
/// `ComputerName`): each name drifts alone.
#[test]
fn each_of_the_three_host_names_is_checked() {
    assert_only_fails(
        "hostname",
        |m| {
            m.state_mut().scutil.insert("HostName".into(), "x".into());
        },
        "want kbf-mac-07, found HostName=x",
    );
    assert_only_fails(
        "hostname",
        |m| {
            m.state_mut().scutil.remove("LocalHostName");
        },
        "found LocalHostName=LocalHostName: not set",
    );
    assert_only_fails(
        "hostname",
        |m| {
            m.state_mut()
                .scutil
                .insert("ComputerName".into(), String::new());
        },
        "found ComputerName=unset",
    );
}

/// Catches: another administrator ignored (the script's defect), whether listed in the
/// group or an administrator by primary group (the defect that skips the user list).
#[test]
fn another_administrator_fails() {
    assert_only_fails(
        "admin_user",
        |m| {
            m.state_mut().attr(
                "/Groups/admin",
                "GroupMembership",
                "root farmadmin intruder intruder",
            )
        },
        "other administrators: intruder [check only",
    );
    assert_only_fails(
        "admin_user",
        |m| {
            m.state_mut().user("hidden", 502);
            m.state_mut().user_attr("hidden", "PrimaryGroupID", "80");
        },
        "other administrators: hidden",
    );
}

/// Catches: the administrator checked by the `GroupMembership` list only (the script's
/// defect): an admin by primary group alone passes, one in no group fails.
#[test]
fn the_administrator_is_checked_through_dseditgroup() {
    let mut mac = FakeMac::converged("admin-primary");
    mac.state_mut()
        .attr("/Groups/admin", "GroupMembership", "root");
    mac.state_mut()
        .user_attr("farmadmin", "PrimaryGroupID", "80");
    assert_eq!(check(&mac).failed, 0);
    assert_only_fails(
        "admin_user",
        |m| {
            m.state_mut()
                .attr("/Groups/admin", "GroupMembership", "root")
        },
        "farmadmin is not in the admin group",
    );
}

/// Catches: the role account's `Password` not checked (the script's defect).
#[test]
fn a_role_account_with_a_password_fails() {
    assert_only_fails(
        "role_user",
        |m| m.state_mut().user_attr("_kbf", "Password", "secret"),
        "Password=secret",
    );
}

/// Catches: `AuthenticationAuthority` read as one line, where dscl prints its value on
/// the next (the script's defect).
#[test]
fn an_authentication_authority_fails_the_role_account() {
    assert_only_fails(
        "role_user",
        |m| {
            m.state_mut()
                .user_attr("_kbf", "AuthenticationAuthority", ";ShadowHash;")
        },
        "AuthenticationAuthority=present",
    );
}

/// Catches: a group holding the role id ignored (the script's defect).
#[test]
fn a_group_holding_the_role_id_fails() {
    assert_only_fails(
        "role_user",
        |m| m.state_mut().attr("/Groups/staff", "PrimaryGroupID", "480"),
        "gid 480 belongs to staff",
    );
}

/// Catches: the role account's group not checked: its id differs.
#[test]
fn a_role_group_with_another_id_fails() {
    assert_only_fails(
        "role_user",
        |m| m.state_mut().attr("/Groups/_kbf", "PrimaryGroupID", "481"),
        "group=481",
    );
}

/// Catches: the sshd `Include` line not required (the script's defect).
#[test]
fn an_sshd_config_without_the_include_line_fails() {
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "Include /etc/ssh/sshd_config.d/*.conf\n",
            )
        },
        "lacks the line",
    );
}

/// Catches: a drop-in that sorts before ours ignored (the script's defect).
#[test]
fn a_drop_in_read_before_ours_fails() {
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.write(
                "/etc/ssh/sshd_config.d/0-first.conf",
                "PermitRootLogin yes\n",
            )
        },
        "sshd reads 0-first.conf before 000-kbf.conf (apply does not remove them)",
    );
}

/// Catches: a keyword before the Include line ignored, matched in one case only, or
/// missed when written `Keyword=value` (three of the script's defects).
#[test]
fn a_keyword_before_the_include_line_fails() {
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "ChallengeResponseAuthentication yes\nInclude /etc/ssh/sshd_config.d/*\n",
            )
        },
        "sets ChallengeResponseAuthentication before its Include line (apply does not edit it)",
    );
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "KBDINTERACTIVEAUTHENTICATION yes\nInclude /etc/ssh/sshd_config.d/*\n",
            )
        },
        "sets KBDINTERACTIVEAUTHENTICATION before",
    );
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.write(
                "/etc/ssh/sshd_config",
                "PasswordAuthentication=yes\nInclude /etc/ssh/sshd_config.d/*\n",
            )
        },
        "sets PasswordAuthentication=yes before",
    );
}

/// Catches: `sshd -T` not consulted (the script's defect): what sshd uses differs
/// from every file the check reads.
#[test]
fn what_sshd_reports_is_checked() {
    assert_only_fails(
        "ssh_password_auth",
        |m| {
            m.state_mut().sshd_effective = Some("passwordauthentication no\nkbdinteractiveauthentication no\npermitrootlogin yes\nport 22\n".into())
        },
        "sshd -T reports 'kbdinteractiveauthentication no;passwordauthentication no;permitrootlogin yes', want 'kbdinteractiveauthentication no;passwordauthentication no;permitrootlogin no'",
    );
}

/// Catches: `sshd -T` run as another user, which it refuses (the script's defect "sshd
/// -T runs without root"; `t_sshd_not_root`).
#[test]
fn sshd_is_not_asked_without_root() {
    let mut mac = FakeMac::converged("sshd-not-root");
    mac.state_mut().euid = 501;
    mac.state_mut().sshd_effective = Some("passwordauthentication yes\n".into());
    let r = check_keys(&mac, Some("ssh_password_auth"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains("(sshd -T not run: needs root)"),
        "{}",
        r.text
    );
    assert!(
        !mac.log.borrow().iter().any(|c| c.starts_with("sshd")),
        "{:?}",
        mac.log.borrow()
    );
}

/// Catches: a large clock offset passing, or a negative one counted as in range (two
/// of the script's defects); and the boundary, an offset equal to the maximum, failing.
#[test]
fn the_clock_offset_is_bounded_both_ways() {
    assert_only_fails(
        "time_max_offset_ms",
        |m| m.state_mut().sntp_offset = "+0.251".into(),
        "251 ms from time.example.net, more than 250",
    );
    assert_only_fails(
        "time_max_offset_ms",
        |m| m.state_mut().sntp_offset = "-0.251".into(),
        "251 ms",
    );
    let mut mac = FakeMac::converged("offset-at-max");
    mac.state_mut().sntp_offset = "-0.250".into();
    assert_eq!(check(&mac).failed, 0);
}

/// Catches: `sntp`'s offset parsed wrongly: its sign, its fraction, or a word that is
/// not a number.
#[test]
fn offsets_parse_to_whole_milliseconds() {
    use kbf_mac_provision::items::offset_ms;
    for (word, ms) in [
        ("+0.004000", Some(4)),
        ("-1.5", Some(1500)),
        ("2", Some(2000)),
        ("0.0009", Some(0)),
        ("1.005", Some(1005)),
        ("+", None),
        ("1.", None),
        (".5", None),
        ("1.5x", None),
        ("fail", None),
        ("99999999999999999999", None),
        ("18446744073709552", None),
    ] {
        assert_eq!(offset_ms(word), ms, "{word}");
    }
}

/// Catches: a probe with another SHA-256 passing (the script's defect).
#[test]
fn a_probe_with_another_sha256_fails() {
    assert_only_fails(
        "probe",
        |m| m.write("/usr/local/kbf/probes/host_identity.sh", "echo other\n"),
        "host_identity: /usr/local/kbf/probes/host_identity.sh is missing or has another SHA-256;",
    );
}

/// Catches: an error log that is not safe to empty as root passing check: a symbolic
/// or a hard link to a root file (the script's defects "check ignores the error log",
/// "a hard link at the error log path counts as safe"; `t_err_log`, check part).
#[test]
fn an_error_log_that_is_a_link_fails() {
    for symbolic in [true, false] {
        let mut mac = FakeMac::converged(&format!("err-log-{symbolic}"));
        let precious = mac.path("/etc/precious");
        fs::write(&precious, "root: precious\n").unwrap();
        mac.remove(ERR_LOG);
        if symbolic {
            mac.symlink(&precious, ERR_LOG);
        } else {
            fs::hard_link(&precious, mac.path(ERR_LOG)).unwrap();
        }
        mac.own(ERR_LOG, 480, 480);
        let r = check_keys(&mac, Some("launchd_label"));
        assert_eq!(r.failed, 1, "{}", r.text);
        assert!(
            r.text
                .contains("FAIL launchd_label: /Library/Logs/kbf/kbf-daemon.err.log is ")
                && r.text
                    .contains("want a regular file with one link, -rw-r--r-- 480 480"),
            "{}",
            r.text
        );
    }
}

/// Catches: a missing error log counted as safe (the script's defect).
#[test]
fn a_missing_error_log_fails() {
    assert_only_fails(
        "launchd_label",
        |m| m.remove(ERR_LOG),
        "kbf-daemon.err.log is missing, want a regular file with one link, -rw-r--r-- 480 480",
    );
}

/// Catches: a log directory that is not root's passing (the script's defect "the log
/// directory is the role account's"): the role account could swap the error log.
#[test]
fn a_log_directory_that_is_not_roots_fails() {
    assert_only_fails(
        "launchd_label",
        |m| m.own("/Library/Logs/kbf", 0, 480),
        "/Library/Logs/kbf is drwxr-xr-x 0 480, want drwxr-xr-x 0 0",
    );
    assert_only_fails(
        "launchd_label",
        |m| m.chmod("/Library/Logs/kbf", 0o3775),
        "/Library/Logs/kbf is drwxrwsr-t 0 0",
    );
}

/// Catches: `--keys` checking other items than it names, or reporting a wrong count
/// (the script's `t_keys`, check part).
#[test]
fn keys_limits_check_to_the_named_items() {
    let mac = FakeMac::fresh("keys");
    let r = check_keys(&mac, Some("firewall,filevault"));
    assert_eq!(
        r.text,
        "FAIL firewall: firewall off, want on\nPASS filevault: off\ncheck: 1 pass, 1 fail\n"
    );
    assert_eq!(r.failed, 1);
    assert_eq!(check_keys(&mac, Some(",firewall,")).text.lines().count(), 2);
}

/// Catches: `--keys` ignoring an unknown item (the script's defect), so a typo checks
/// nothing and passes.
#[test]
fn keys_refuses_an_unknown_item() {
    let mac = FakeMac::fresh("keys-unknown");
    let path = mac.profile_file(&mac.profile_text());
    let e = cli::check(&path, Some("firewall,bogus"), &mac).unwrap_err();
    assert!(
        e.to_string()
            .starts_with("--keys: unknown item: bogus (items: macos_version macos_build hostname "),
        "{e}"
    );
    assert!(e.to_string().ends_with(" launchd_label)"), "{e}");
}

/// Catches: the other value of the on/off keys checked wrongly: Remote Login off and
/// passwords allowed (the script's `t_other_values`, check part).
#[test]
fn the_other_values_of_remote_login_and_ssh_pass() {
    let mut mac = FakeMac::converged("other-values");
    mac.state_mut().remote_login = false;
    mac.write(
        DROPIN,
        "PasswordAuthentication yes\nKbdInteractiveAuthentication yes\nPermitRootLogin no\n",
    );
    let text = mac
        .profile_text()
        .replace("remote_login=1", "remote_login=0")
        .replace("ssh_password_auth=0", "ssh_password_auth=1");
    let path = mac.profile_file(&text);
    let r = cli::check(&path, Some("remote_login,ssh_password_auth"), &mac).unwrap();
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains("PASS remote_login: Remote Login: Off"),
        "{}",
        r.text
    );
}

/// Catches: with no Xcode pinned, an Xcode found not reported, or the default directory
/// failed for want of one (the script's `t_xcode_sources`, check part).
#[test]
fn with_no_xcode_pinned_every_xcode_is_drift() {
    let mac = FakeMac::fresh("no-xcode");
    mac.mkdir("/Applications/Xcode.app");
    let text: String = mac
        .profile_text()
        .lines()
        .filter(|l| {
            !l.starts_with("xcode=")
                && !l.starts_with("expect_probe=")
                && !l.starts_with("default_developer_dir=")
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let path = mac.profile_file(&text);
    let r = cli::check(&path, Some("xcode,default_developer_dir"), &mac).unwrap();
    assert_eq!(
        r.text,
        "FAIL xcode: /Applications/Xcode.app is not pinned;\nPASS default_developer_dir: no Xcode is pinned\ncheck: 1 pass, 1 fail\n"
    );
    std::fs::remove_dir_all(mac.path("/Applications/Xcode.app")).unwrap();
    let r = cli::check(&path, Some("xcode,probe"), &mac).unwrap();
    assert!(
        r.text.starts_with("PASS xcode: pinned: none\n"),
        "{}",
        r.text
    );
}

/// Catches: a profile without probes failing or printing nothing for the probe item.
#[test]
fn with_no_probe_pinned_the_probe_item_passes() {
    let mac = FakeMac::fresh("no-probe");
    let text: String = mac
        .profile_text()
        .lines()
        .filter(|l| !l.starts_with("probe=") && !l.starts_with("expect_probe="))
        .map(|l| format!("{l}\n"))
        .collect();
    let path = mac.profile_file(&text);
    let r = cli::check(&path, Some("probe"), &mac).unwrap();
    assert_eq!(r.text, "PASS probe: none\ncheck: 1 pass, 0 fail\n");
}

/// Catches: a check-only item's failure not saying that `apply` leaves it (the
/// script's defect "apply turns FileVault off" relies on the item being check only),
/// or a changeable item's failure saying so.
#[test]
fn only_check_only_items_say_apply_leaves_them() {
    let mac = FakeMac::fresh("check-only");
    let r = check(&mac);
    let check_only: Vec<&str> = kbf_mac_provision::items::ITEMS
        .iter()
        .filter(|i| i.check_only())
        .map(|i| i.key)
        .collect();
    assert_eq!(
        check_only,
        [
            "macos_version",
            "macos_build",
            "admin_user",
            "filevault",
            "probe",
            "simulator_runtimes"
        ]
    );
    assert!(!r.text.contains("[check only"), "{}", r.text);
    let mut mac = FakeMac::converged("check-only-fail");
    mac.state_mut().filevault = "On".into();
    mac.state_mut().sw_vers.insert("version", String::new());
    let r = check(&mac);
    assert!(
        r.text.contains(
            "FAIL filevault: FileVault on, want off [check only: apply does not change this]\n"
        ),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("FAIL macos_version: macOS unknown, the profile pins 27.0.1 [check only"),
        "{}",
        r.text
    );
}

/// Catches: a malformed intent record read as naming a lease: an empty term or
/// sequence, a non-digit, a leading zero, or no final newline.
#[test]
fn a_malformed_intent_record_names_no_lease() {
    for record in [".2\n", "4.\n", "4a.2\n", "4.02\n", "4.2", "4-2\n", ""] {
        let mut mac = FakeMac::converged("intent-malformed");
        mac.state_mut().user("kbf-lease-4-2", 642);
        mac.state_mut()
            .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
        mac.write(INTENT, record);
        let r = check_keys(&mac, Some("autologin"));
        assert!(
            r.text.contains("a lease user left behind"),
            "{record:?}: {}",
            r.text
        );
    }
}
