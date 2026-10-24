//! The standing VM user `kbf-vm` and automatic login as it, against the fake Mac: what
//! `check` reads, what `apply` makes, what it refuses, and that the password is in a
//! root-only file and on `dscl`'s standard input and nowhere else.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use support::vm::{
    KCPASSWORD, VM_HOME, VM_PASSWORD, VM_PASSWORD_FILE, VM_STATE_DIR, VM_UID, kcpassword_of,
};
use support::{
    FakeMac, INTENT, LOGINWINDOW, apply, apply_keys, calls, check, check_keys, lines, ls,
};

/// A converged Mac whose profile has a VM user, with none made yet.
fn without_user(name: &str) -> FakeMac {
    let mut mac = FakeMac::converged(name);
    let text = format!("{}vm_user_uid={VM_UID}\n", mac.profile_text());
    mac.set_profile(text);
    mac
}

fn password_file(mac: &FakeMac) -> String {
    fs::read_to_string(mac.path(VM_PASSWORD_FILE)).unwrap()
}

/// On `mac`, drifted by `drift`, check fails `item` and only it, saying `text`.
fn only_fails(mac: &FakeMac, item: &str, text: &str) {
    let r = check(mac);
    let failed = lines(&r, "FAIL");
    assert!(
        r.failed == 1
            && failed.len() == 1
            && failed[0].starts_with(&format!("FAIL {item}: "))
            && failed[0].contains(text),
        "want only FAIL {item}: ..{text}.., got:\n{}",
        r.text
    );
}

/// Catches: a profile with a VM user failing a Mac that has it as `apply` leaves it, or
/// the pass line saying less than what was read.
#[test]
fn a_mac_with_the_vm_user_passes_check() {
    let mac = FakeMac::converged_vm("vm-converged", false);
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains(&format!(
            "PASS vm_user_uid: kbf-vm (uid {VM_UID}, group staff, shell /bin/zsh): hidden, standard, password in {VM_PASSWORD_FILE}, home {VM_HOME}\n"
        )),
        "{}",
        r.text
    );
}

/// Catches: each part of the account left unchecked: a check that passes a visible
/// user, another shell, a user with no password, an administrator, a member of the
/// groups for Remote Login or Screen Sharing, a home that is another's or open, a state
/// directory that others can enter, and a password file that is readable, a link, not a
/// password, or missing.
#[test]
fn check_names_each_way_the_vm_user_is_wrong() {
    type Drift = fn(&mut FakeMac);
    let cases: [(&str, Drift, &str); 15] = [
        (
            "no user",
            |m| m.state_mut().remove_user("kbf-vm"),
            "no user kbf-vm",
        ),
        (
            "visible",
            |m| m.state_mut().user_attr("kbf-vm", "IsHidden", "0"),
            "IsHidden=0, want 1",
        ),
        (
            "shell",
            |m| {
                m.state_mut()
                    .user_attr("kbf-vm", "UserShell", "/usr/bin/false")
            },
            "UserShell=/usr/bin/false, want /bin/zsh",
        ),
        (
            "group",
            |m| m.state_mut().user_attr("kbf-vm", "PrimaryGroupID", "80"),
            "PrimaryGroupID=80, want 20; in admin",
        ),
        (
            "home attribute",
            |m| {
                m.state_mut()
                    .user_attr("kbf-vm", "NFSHomeDirectory", "/var/empty")
            },
            concat!("NFSHomeDirectory=/var/empty, want /Users", "/kbf-vm"),
        ),
        (
            "no password",
            |m| {
                let s = m.state_mut();
                s.records
                    .get_mut(VM_HOME)
                    .unwrap()
                    .retain(|(a, _)| a != "AuthenticationAuthority");
            },
            "no password (no AuthenticationAuthority)",
        ),
        (
            "admin member",
            |m| {
                m.state_mut()
                    .attr("/Groups/admin", "GroupMembership", "root farmadmin kbf-vm");
            },
            "in admin",
        ),
        (
            "ssh member",
            |m| {
                m.state_mut()
                    .attr("/Groups/com.apple.access_ssh", "GroupMembership", "kbf-vm");
            },
            "in com.apple.access_ssh",
        ),
        (
            "screen sharing member",
            |m| {
                m.state_mut().attr(
                    "/Groups/com.apple.access_screensharing",
                    "GroupMembership",
                    "kbf-vm",
                );
            },
            "in com.apple.access_screensharing",
        ),
        (
            "open home",
            |m| m.chmod(VM_HOME, 0o755),
            concat!(
                "home /Users",
                "/kbf-vm is drwxr-xr-x 460 20, want drwx------ 460 20"
            ),
        ),
        (
            "home of another",
            |m| m.own(VM_HOME, 501, 20),
            concat!("home /Users", "/kbf-vm is drwx------ 501 20"),
        ),
        (
            "open state directory",
            |m| m.chmod(VM_STATE_DIR, 0o755),
            "/var/db/kbf-mac-session is drwxr-xr-x 0 0, want drwx------ 0 0",
        ),
        (
            "readable password file",
            |m| m.chmod(VM_PASSWORD_FILE, 0o644),
            "vm-user-password is -rw-r--r-- 0 0, want -r-------- 0 0",
        ),
        (
            "password file of another",
            |m| m.own(VM_PASSWORD_FILE, 501, 20),
            "vm-user-password is -r-------- 501 20",
        ),
        (
            "password file that is no password",
            |m| m.set_vm_password_file("hunter2\n"),
            "does not hold 64 hex digits",
        ),
    ];
    for (name, drift, text) in cases {
        let mut mac = FakeMac::converged_vm(&format!("vm-drift-{}", name.replace(' ', "-")), false);
        drift(&mut mac);
        let r = check(&mac);
        let failed = lines(&r, "FAIL vm_user_uid: ");
        // An administrator also fails `admin_user`, which counts every one.
        let others = usize::from(matches!(name, "group" | "admin member"));
        assert!(
            r.failed == 1 + others && failed.len() == 1 && failed[0].contains(text),
            "{name}: want FAIL vm_user_uid: ..{text}..\n{}",
            r.text
        );
        assert!(!r.text.contains("hunter2"), "{name}: {}", r.text);
    }
    let mac = FakeMac::converged_vm("vm-drift-missing-file", false);
    mac.remove(VM_PASSWORD_FILE);
    only_fails(
        &mac,
        "vm_user_uid",
        "vm-user-password is missing, want -r-------- 0 0",
    );
    let mac = FakeMac::converged_vm("vm-drift-linked-file", false);
    mac.remove(VM_PASSWORD_FILE);
    mac.write("/var/db/elsewhere", VM_PASSWORD);
    mac.symlink(&mac.path("/var/db/elsewhere"), VM_PASSWORD_FILE);
    only_fails(&mac, "vm_user_uid", "vm-user-password is lrwx");
}

/// Catches: another account holding the uid or the name passing check, which would leave
/// two accounts one file owner or a login that is another's.
#[test]
fn check_fails_a_uid_or_a_name_another_account_holds() {
    let mut mac = FakeMac::converged_vm("vm-uid-held", false);
    mac.state_mut().user("builder", VM_UID);
    only_fails(&mac, "vm_user_uid", "uid 460 belongs to builder");
    let mut mac = FakeMac::converged_vm("vm-name-held", false);
    mac.state_mut().user("builder", 900);
    mac.state_mut().user_attr("builder", "RecordName", "kbf-vm");
    only_fails(&mac, "vm_user_uid", "also a name of user builder");
}

/// Catches: a profile with no VM user managing one anyway: check names nothing and
/// apply runs no program for the item.
#[test]
fn a_profile_with_no_vm_user_leaves_the_item_alone() {
    let mac = FakeMac::converged("vm-unmanaged");
    let r = check_keys(&mac, Some("vm_user_uid"));
    assert_eq!(
        r.text,
        "PASS vm_user_uid: not managed\ncheck: 1 pass, 0 fail\n"
    );
    let r = apply_keys(&mac, Some("vm_user_uid"));
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert!(!mac.path(VM_PASSWORD_FILE).exists());
}

/// Catches: the VM user made wrong or in the wrong order: the account without its uid,
/// shell, hidden flag or password, a home that is not the user's with mode 700, a state
/// directory that is not root's with mode 700, a password file that is not root's with
/// mode 400 or not 64 hex digits. And a password that reaches an argument list, a
/// report, the log or `/etc/kcpassword`'s source: it is on `dscl`'s standard input once,
/// and it is the file's.
#[test]
fn apply_makes_the_vm_user_and_keeps_its_password_in_the_file_only() {
    let mac = without_user("vm-apply");
    mac.mkdir(VM_STATE_DIR);
    mac.chmod(VM_STATE_DIR, 0o755);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    let after = check(&mac);
    assert_eq!(after.failed, 0, "{}", after.text);
    let password = password_file(&mac);
    assert_eq!(password.len(), 64);
    assert!(
        password.bytes().all(|b| b.is_ascii_hexdigit()),
        "{password}"
    );
    assert_eq!(ls(&mac, VM_PASSWORD_FILE), "-r-------- 0 0");
    assert_eq!(ls(&mac, VM_STATE_DIR), "drwx------ 0 0");
    assert_eq!(ls(&mac, VM_HOME), "drwx------ 460 20");
    assert_eq!(
        r.text.lines().collect::<Vec<_>>(),
        [
            "CHANGE vm_user_uid: /var/db/kbf-mac-session, root's, mode 700".to_owned(),
            format!(
                "CHANGE vm_user_uid: {VM_PASSWORD_FILE}: a new random password, root's, mode 400"
            ),
            "CHANGE vm_user_uid: user kbf-vm, uid 460".to_owned(),
            "CHANGE vm_user_uid: password of kbf-vm set (not shown)".to_owned(),
            concat!("CHANGE vm_user_uid: home /Users", "/kbf-vm, mode 700").to_owned(),
        ]
    );
    {
        let s = mac.state.borrow();
        assert_eq!(
            s.passwords[VM_HOME], password,
            "the account has the file's password"
        );
        let user = &s.records[VM_HOME];
        for (attr, want) in [
            ("UniqueID", "460"),
            ("PrimaryGroupID", "20"),
            ("UserShell", "/bin/zsh"),
            ("NFSHomeDirectory", VM_HOME),
            ("IsHidden", "1"),
        ] {
            assert!(
                user.contains(&(attr.to_owned(), want.to_owned())),
                "{attr}: {user:?}"
            );
        }
    }
    // Where the password is: the file, and one piece of standard input. Nowhere else.
    let inputs = mac.inputs.borrow();
    let with_password: Vec<&String> = inputs.iter().filter(|i| i.contains(&password)).collect();
    assert_eq!(with_password, [&format!("passwd {VM_HOME} {password}\n")]);
    for call in mac.log.borrow().iter() {
        assert!(
            !call.contains(&password),
            "an argument list holds the password: {call}"
        );
    }
    assert!(!r.text.contains(&password) && !after.text.contains(&password));
    assert!(
        !inputs
            .iter()
            .any(|i| i.contains(concat!("create /Users", "/kbf-vm")) && i.contains(&password))
    );
    assert_eq!(mode_of(&mac, VM_PASSWORD_FILE), 0o400);
}

fn mode_of(mac: &FakeMac, path: &str) -> u32 {
    fs::metadata(mac.path(path)).unwrap().permissions().mode() & 0o7777
}

/// Catches: a second apply that changes anything or reads the random source (a new
/// password would lock the root helper out of the account), and an apply on a Mac
/// that has the VM user touching the password file.
#[test]
fn a_second_apply_changes_nothing_and_keeps_the_password() {
    let mac = without_user("vm-twice");
    assert_eq!(apply(&mac).failed, 0);
    let (password, reads) = (password_file(&mac), mac.state.borrow().random_reads);
    let inputs = mac.inputs.borrow().len();
    mac.log.borrow_mut().clear();
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert_eq!(password_file(&mac), password);
    assert_eq!(mac.state.borrow().random_reads, reads);
    assert_eq!(mac.inputs.borrow().len(), inputs);
}

/// Catches: a repair that makes a new password for an account that has one (the file
/// stays what the root helper reads), a repair of one attribute that remakes the whole
/// user, and a user left in `admin` or in the Remote Login group.
#[test]
fn apply_repairs_what_drifted_and_keeps_the_password() {
    let mut mac = FakeMac::converged_vm("vm-repair", false);
    mac.state_mut()
        .user_attr("kbf-vm", "UserShell", "/usr/bin/false");
    mac.state_mut().user_attr("kbf-vm", "IsHidden", "0");
    mac.state_mut()
        .attr("/Groups/admin", "GroupMembership", "root farmadmin kbf-vm");
    mac.state_mut()
        .attr("/Groups/com.apple.access_ssh", "GroupMembership", "kbf-vm");
    mac.chmod(VM_HOME, 0o755);
    mac.chmod(VM_PASSWORD_FILE, 0o644);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        r.text.lines().collect::<Vec<_>>(),
        [
            "CHANGE vm_user_uid: /var/db/kbf-mac-session/vm-user-password, root's, mode 400",
            "CHANGE vm_user_uid: user kbf-vm, uid 460",
            "CHANGE vm_user_uid: kbf-vm removed from admin",
            "CHANGE vm_user_uid: kbf-vm removed from com.apple.access_ssh",
            concat!("CHANGE vm_user_uid: home /Users", "/kbf-vm, mode 700"),
        ]
    );
    assert_eq!(password_file(&mac), VM_PASSWORD);
    assert_eq!(mac.state.borrow().random_reads, 0);
    assert!(mac.inputs.borrow().iter().all(|i| !i.contains("passwd")));
    assert_eq!(ls(&mac, VM_PASSWORD_FILE), "-r-------- 0 0");
}

/// Catches: a lost password file leaving the account with a password nobody knows: a
/// new password is made, written to the file, and set on the account.
#[test]
fn a_missing_password_file_is_made_again_and_set_on_the_account() {
    let mac = FakeMac::converged_vm("vm-lost-file", false);
    mac.remove(VM_PASSWORD_FILE);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    let password = password_file(&mac);
    assert_ne!(password, VM_PASSWORD);
    assert_eq!(mac.state.borrow().passwords[VM_HOME], password);
    assert_eq!(
        r.text.lines().next(),
        Some(
            format!(
                "CHANGE vm_user_uid: {VM_PASSWORD_FILE}: a new random password, root's, mode 400"
            )
            .as_str()
        )
    );
}

/// Catches: an account made but not given its password (a run that stopped between the
/// two) left without one: the next run sets the file's password and does not make
/// another.
#[test]
fn an_account_without_a_password_gets_the_files() {
    let mut mac = FakeMac::converged_vm("vm-half-run", false);
    mac.state_mut()
        .records
        .get_mut(VM_HOME)
        .unwrap()
        .retain(|(a, _)| a != "AuthenticationAuthority");
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        r.text,
        "CHANGE vm_user_uid: password of kbf-vm set (not shown)\n"
    );
    assert_eq!(password_file(&mac), VM_PASSWORD);
    assert_eq!(mac.state.borrow().passwords[VM_HOME], VM_PASSWORD);
}

/// Catches: a password dscl refuses leaving a file that no account has (the next
/// run would trust it): it is removed, the item says so, and the next run makes a
/// password and sets it.
#[test]
fn a_password_dscl_refuses_removes_the_file_it_made() {
    let mac = without_user("vm-refused");
    mac.state.borrow_mut().stdin_rejects = vec!["passwd".into()];
    let r = apply(&mac);
    assert!(
        r.text.contains("NOTE vm_user_uid: dscl did not set the password of kbf-vm; the password file was removed, the next apply makes another"),
        "{}",
        r.text
    );
    assert!(!mac.path(VM_PASSWORD_FILE).exists());
    assert_eq!(r.failed, 1, "{}", r.text);
    mac.state.borrow_mut().stdin_rejects.clear();
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(mac.state.borrow().passwords[VM_HOME], password_file(&mac));
}

/// Catches: an unreadable random source ending in a password made of nothing (zero
/// bytes) or an account made without one.
#[test]
fn no_random_source_makes_no_user_and_no_file() {
    let mac = without_user("vm-no-random");
    mac.state.borrow_mut().random_fails = true;
    let r = apply(&mac);
    assert!(
        r.text.contains("NOTE vm_user_uid: no random source"),
        "{}",
        r.text
    );
    assert!(!mac.path(VM_PASSWORD_FILE).exists());
    assert!(!mac.state.borrow().records.contains_key(VM_HOME));
}

/// Catches: `apply` taking over an account that is not the VM user's: one that holds
/// the uid, one that answers to the name `kbf-vm`, one named `kbf-vm` with another
/// uid. Each makes no change at all (no program writes, no file) and says why.
#[test]
fn apply_refuses_a_record_that_collides() {
    for (name, setup, why) in [
        (
            "uid",
            (|m: &mut FakeMac| m.state_mut().user("builder", VM_UID)) as fn(&mut FakeMac),
            "uid 460 belongs to builder: kbf-vm not changed",
        ),
        (
            "alias",
            |m| {
                m.state_mut().user("builder", 900);
                m.state_mut().user_attr("builder", "RecordName", "kbf-vm");
            },
            "user builder is also named kbf-vm: kbf-vm not changed",
        ),
        (
            "other uid",
            |m| m.state_mut().user("kbf-vm", 501),
            "user kbf-vm has uid 501, not the profile's 460: it is not the VM user: kbf-vm not changed",
        ),
    ] {
        let mut mac = without_user(&format!("vm-collide-{name}"));
        setup(&mut mac);
        let before = mac.state.borrow().records.clone();
        let r = apply(&mac);
        assert!(
            r.text.contains(&format!("NOTE vm_user_uid: {why}")),
            "{name}: {}",
            r.text
        );
        assert_eq!(r.failed, 1, "{name}: {}", r.text);
        assert_eq!(mac.writes(), Vec::<String>::new(), "{name}");
        assert_eq!(mac.state.borrow().records, before, "{name}");
        assert!(!mac.path(VM_PASSWORD_FILE).exists(), "{name}");
    }
}

/// Catches: a link at the state directory followed (the password written wherever it
/// points) or a file there overwritten.
#[test]
fn apply_writes_no_password_through_a_link_at_the_state_directory() {
    let mac = without_user("vm-state-link");
    mac.remove(VM_STATE_DIR);
    mac.mkdir("/var/db/elsewhere");
    mac.symlink(&mac.path("/var/db/elsewhere"), VM_STATE_DIR);
    let r = apply(&mac);
    assert!(
        r.text
            .contains("NOTE vm_user_uid: /var/db/kbf-mac-session is lrwx"),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("not a directory: no password written"),
        "{}",
        r.text
    );
    assert_eq!(
        fs::read_dir(mac.path("/var/db/elsewhere")).unwrap().count(),
        0
    );
}

/// Catches: auto-login as the VM user refused by an `autologin=off` profile (the
/// auto-login rule only knew lease users), accepted for another account, or accepted
/// with a `/etc/kcpassword` that is not the password of the file (the login window
/// would fail every boot).
#[test]
fn autologin_accepts_the_configured_vm_user_and_no_other() {
    let mut mac = FakeMac::converged_vm("vm-autologin-off", false);
    mac.converge_vm_autologin();
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains("PASS autologin: kbf-vm (uid 460), the standing VM user; /etc/kcpassword holds its password\n"),
        "{}",
        r.text
    );
    // A password the file does not hold.
    mac.write_bytes(KCPASSWORD, &kcpassword_of("not the password"));
    only_fails(
        &mac,
        "autologin",
        "does not hold the password in /var/db/kbf-mac-session/vm-user-password",
    );
    // No file at all.
    mac.remove(KCPASSWORD);
    only_fails(
        &mac,
        "autologin",
        "/etc/kcpassword is missing or not a file",
    );
    // A password file that holds no password.
    mac.converge_vm_autologin();
    mac.set_vm_password_file("short");
    let r = check(&mac);
    assert!(
        r.text.contains("FAIL autologin: auto-login is kbf-vm, but /var/db/kbf-mac-session/vm-user-password holds no valid password"),
        "{}",
        r.text
    );
}

/// Catches: the VM user accepted by uid alone or by name alone: an account of that name
/// with another uid, and the same uid under another name, both fail; a profile with no
/// VM user still refuses an auto-login as `kbf-vm`; and a lease user in the lease range
/// still fails with no intent record, with the VM user on the profile.
#[test]
fn autologin_refuses_what_is_not_exactly_the_vm_user() {
    let mut mac = FakeMac::converged_vm("vm-autologin-uid", false);
    mac.converge_vm_autologin();
    mac.state_mut().user("kbf-vm", 501);
    let r = check(&mac);
    assert!(
        r.text.contains(
            "FAIL autologin: auto-login is kbf-vm (uid 501), not the profile's VM user (uid 460)"
        ),
        "{}",
        r.text
    );

    let mut mac = FakeMac::converged_vm("vm-autologin-name", false);
    mac.state_mut().user("builder", VM_UID);
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "builder");
    mac.write_bytes(KCPASSWORD, &kcpassword_of(VM_PASSWORD));
    let r = check(&mac);
    assert!(
        r.text.contains("FAIL autologin: auto-login is builder (uid 460); only a kbf-lease-* user in uids 600-699 or kbf-vm (uid 460) may be"),
        "{}",
        r.text
    );

    // No VM user in the profile: the name alone is not enough.
    let mut mac = FakeMac::converged("vm-autologin-unconfigured");
    mac.state_mut().user("kbf-vm", VM_UID);
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-vm");
    mac.write_bytes(KCPASSWORD, &kcpassword_of(VM_PASSWORD));
    only_fails(
        &mac,
        "autologin",
        "auto-login is kbf-vm (uid 460); only a kbf-lease-* user in uids 600-699 may be",
    );

    // A lease user is still held to its intent record.
    let mut mac = FakeMac::converged_vm("vm-autologin-lease", false);
    mac.state_mut().user("kbf-lease-4-2", 642);
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
    only_fails(&mac, "autologin", "a lease user left behind");
    mac.write(INTENT, "4.2\n");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
}

/// Catches: `autologin=vm` passing while auto-login is off, or failing a Mac that a
/// whole_machine lease has moved to a lease user (the lease ends and the rest state
/// returns).
#[test]
fn autologin_vm_wants_the_vm_user_and_allows_a_lease_in_between() {
    let mut mac = FakeMac::converged_vm("vm-autologin-want", true);
    assert_eq!(check(&mac).failed, 0);
    mac.clear_auto_login();
    mac.remove(KCPASSWORD);
    only_fails(
        &mac,
        "autologin",
        "off, but the profile wants auto-login as kbf-vm: apply sets it",
    );
    let mut mac = FakeMac::converged_vm("vm-autologin-lease-between", true);
    mac.state_mut().user("kbf-lease-4-2", 642);
    mac.write(INTENT, "4.2\n");
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
    assert_eq!(check(&mac).failed, 0);
}

/// Catches: auto-login not set by `apply`, set with a `/etc/kcpassword` that is not
/// the password of the file's (root only, mode 600), set while FileVault is on, an
/// apply that is not idempotent, or one that clears the VM user's auto-login.
#[test]
fn apply_sets_auto_login_to_the_vm_user_and_a_second_apply_changes_nothing() {
    let mut mac = FakeMac::converged_vm("vm-enable", true);
    mac.clear_auto_login();
    mac.remove(KCPASSWORD);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        r.text,
        "CHANGE autologin: wrote /etc/kcpassword for kbf-vm (mode 600)\nCHANGE autologin: auto-login is kbf-vm (from the next login window; nothing is restarted)\n"
    );
    assert_eq!(
        fs::read(mac.path(KCPASSWORD)).unwrap(),
        kcpassword_of(VM_PASSWORD)
    );
    assert_eq!(ls(&mac, KCPASSWORD), "-rw------- 0 0");
    assert_eq!(
        support::preference(&mac, LOGINWINDOW, "autoLoginUser").as_deref(),
        Some("kbf-vm")
    );
    // The login window gets a name, and the password is nowhere in a program's arguments.
    assert!(
        calls(&mac, "defaults write")
            .iter()
            .any(|c| c.ends_with("autoLoginUser -string kbf-vm"))
    );
    assert!(mac.log.borrow().iter().all(|c| !c.contains(VM_PASSWORD)));
    let again = apply(&mac);
    assert_eq!((again.failed, again.text.as_str()), (0, ""));
}

/// Catches: auto-login set as a stale lease user's replacement without the password of
/// the file, and a `/etc/kcpassword` of another password rewritten without being
/// asked for the right one: both are made the VM user's.
#[test]
fn apply_replaces_a_stale_auto_login_and_a_wrong_kcpassword() {
    let mut mac = FakeMac::converged_vm("vm-replace", true);
    mac.state_mut().user("kbf-lease-4-2", 642);
    mac.state_mut()
        .default_set(LOGINWINDOW, "autoLoginUser", "kbf-lease-4-2");
    mac.write_bytes(KCPASSWORD, &kcpassword_of("stale"));
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        fs::read(mac.path(KCPASSWORD)).unwrap(),
        kcpassword_of(VM_PASSWORD)
    );
    assert_eq!(
        support::preference(&mac, LOGINWINDOW, "autoLoginUser").as_deref(),
        Some("kbf-vm")
    );
}

/// Catches: auto-login enabled where it cannot work or without what it needs: with
/// FileVault on (the Mac would stop at the unlock screen), with no VM user, with a
/// password file that is no password. Each refuses, writes no `/etc/kcpassword` and
/// sets no auto-login.
#[test]
fn apply_does_not_set_auto_login_without_what_it_needs() {
    type Setup = fn(&mut FakeMac);
    let cases: [(&str, Setup, &str); 3] = [
        (
            "filevault",
            |m| m.state_mut().filevault = "On".into(),
            "NOTE autologin: FileVault is not off (FileVault is On.): automatic login cannot work, so it is not set",
        ),
        (
            "no-user",
            |m| m.state_mut().remove_user("kbf-vm"),
            "NOTE autologin: no user kbf-vm with uid 460: apply vm_user_uid first",
        ),
        (
            "bad-file",
            |m| m.set_vm_password_file("nope"),
            "NOTE autologin: /var/db/kbf-mac-session/vm-user-password holds no valid password: apply vm_user_uid first",
        ),
    ];
    for (name, setup, note) in cases {
        let mut mac = FakeMac::converged_vm(&format!("vm-enable-{name}"), true);
        mac.clear_auto_login();
        mac.remove(KCPASSWORD);
        setup(&mut mac);
        let r = apply_keys(&mac, Some("autologin"));
        assert!(r.text.contains(note), "{name}: {}", r.text);
        assert!(!mac.path(KCPASSWORD).exists(), "{name}");
        assert_eq!(
            support::preference(&mac, LOGINWINDOW, "autoLoginUser"),
            None,
            "{name}"
        );
        assert_eq!(r.failed, 1, "{name}: {}", r.text);
    }
}

/// Catches: `autologin=off` apply clearing the VM user's auto-login, which the auto-login
/// rule accepts (the Mac would lose its session at every apply).
#[test]
fn apply_leaves_the_vm_users_auto_login_when_the_profile_says_off() {
    let mut mac = FakeMac::converged_vm("vm-leave", false);
    mac.converge_vm_autologin();
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert!(mac.path(KCPASSWORD).exists());
}

/// Catches: a FileVault that is on passing for a profile that asks for a VM user (the
/// profile itself refuses `filevault=on` with a VM user, so the Mac must say off).
#[test]
fn filevault_on_fails_a_profile_that_wants_vms() {
    let mut mac = FakeMac::converged_vm("vm-filevault", false);
    mac.state_mut().filevault = "On".into();
    only_fails(&mac, "filevault", "FileVault on, want off");
}

/// Catches: a random source that gives fewer bytes than asked making a shorter
/// password (a weaker one the file check then refuses): nothing is written.
#[test]
fn a_short_read_of_the_random_source_makes_no_password() {
    let mac = without_user("vm-short-random");
    mac.state.borrow_mut().random_short = true;
    let r = apply(&mac);
    assert!(
        r.text
            .contains("NOTE vm_user_uid: the random source gave too few bytes"),
        "{}",
        r.text
    );
    assert!(!mac.path(VM_PASSWORD_FILE).exists());
    assert!(!mac.state.borrow().records.contains_key(VM_HOME));
}

/// Catches: a missing state directory not made (the password file has nowhere to go),
/// or made with a mode others can enter.
#[test]
fn apply_makes_a_missing_state_directory_root_only() {
    let mac = without_user("vm-no-state-dir");
    mac.remove(VM_STATE_DIR);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(ls(&mac, VM_STATE_DIR), "drwx------ 0 0");
    assert_eq!(mode_of(&mac, VM_STATE_DIR), 0o700);
    assert_eq!(
        r.text.lines().next(),
        Some("CHANGE vm_user_uid: /var/db/kbf-mac-session, root's, mode 700")
    );
}

/// Catches: auto-login as the VM user failed when the password file cannot be read
/// (a check run by a user that is not root cannot compare it, and must not fail what
/// root's check passes). The item for the file reports its mode instead.
#[test]
fn a_check_that_cannot_read_the_password_file_does_not_fail_auto_login() {
    let mut mac = FakeMac::converged_vm("vm-unreadable", false);
    mac.converge_vm_autologin();
    mac.chmod(VM_PASSWORD_FILE, 0o000);
    let r = check(&mac);
    // Running as root reads it anyway and finds it valid: either way auto-login passes.
    assert!(
        r.text.contains("PASS autologin: kbf-vm (uid 460)"),
        "{}",
        r.text
    );
    mac.chmod(VM_PASSWORD_FILE, 0o400);
}
