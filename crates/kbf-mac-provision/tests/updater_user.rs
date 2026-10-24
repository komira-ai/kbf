//! The updater's service account `_kbf-updater` and its two directories, against the fake
//! Mac: what `check` reads, what `apply` makes, what it refuses, and what the profile
//! refuses.

mod support;

use kbf_mac_provision::profile::Profile;
use support::{FakeMac, apply, apply_keys, calls, check, check_keys, lines, ls};

const USER: &str = "/Users/_kbf-updater";
const GROUP: &str = "/Groups/_kbf-updater";
const STATE_DIR: &str = "/var/db/kbf-updater";
const LOG_DIR: &str = "/Library/Logs/kbf-updater";
/// The updater's id in the tests' profile (the role account's is 480).
const ID: u32 = 470;

/// A converged Mac whose profile adds `updater_user` and `updater_id`, with none made.
fn without_account(name: &str) -> FakeMac {
    let mut mac = FakeMac::converged(name);
    let text = format!(
        "{}updater_user=_kbf-updater\nupdater_id={ID}\n",
        mac.profile_text()
    );
    mac.set_profile(text);
    mac
}

/// The same Mac with the account and directories as `apply` leaves them.
fn converged_updater(name: &str) -> FakeMac {
    let mut mac = without_account(name);
    let id = ID.to_string();
    let s = mac.state_mut();
    for (a, v) in [
        ("UniqueID", id.as_str()),
        ("PrimaryGroupID", id.as_str()),
        ("UserShell", "/usr/bin/false"),
        ("NFSHomeDirectory", "/var/empty"),
        ("IsHidden", "1"),
        ("Password", "*"),
    ] {
        s.user_attr("_kbf-updater", a, v);
    }
    s.attr(GROUP, "PrimaryGroupID", &id);
    s.attr(GROUP, "Password", "*");
    for (dir, mode) in [(STATE_DIR, 0o700), (LOG_DIR, 0o755)] {
        mac.mkdir(dir);
        mac.chmod(dir, mode);
        mac.own(dir, ID, ID);
    }
    mac
}

/// On `mac`, check fails `item` and only it, saying `text`.
fn only_fails(mac: &FakeMac, text: &str) {
    let r = check(mac);
    let failed = lines(&r, "FAIL");
    assert!(
        r.failed == 1
            && failed.len() == 1
            && failed[0].starts_with("FAIL updater_user: ")
            && failed[0].contains(text),
        "want only FAIL updater_user: ..{text}.., got:\n{}",
        r.text
    );
}

fn refusal(text: &str) -> String {
    match Profile::parse(text.as_bytes()) {
        Ok(_) => "accepted".into(),
        Err(e) => e.to_string(),
    }
}

fn profile_with(extra: &str) -> String {
    format!(
        "{}{extra}",
        FakeMac::fresh("updater-profile").profile_text()
    )
}

/// Catches: a Mac with the account as `apply` leaves it failing, or a pass line that
/// says less than what was read.
#[test]
fn a_mac_with_the_updater_account_passes_check() {
    let mac = converged_updater("updater-converged");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains(&format!(
            "PASS updater_user: _kbf-updater (uid and gid {ID}, hidden, no shell, no password): {STATE_DIR} mode 700, {LOG_DIR} mode 755\n"
        )),
        "{}",
        r.text
    );
}

/// Catches: a profile without `updater_user` managing an account anyway.
#[test]
fn a_profile_with_no_updater_user_leaves_the_item_alone() {
    let mac = FakeMac::converged("updater-unmanaged");
    let r = check_keys(&mac, Some("updater_user"));
    assert_eq!(
        r.text,
        "PASS updater_user: not managed\ncheck: 1 pass, 0 fail\n"
    );
    let r = apply_keys(&mac, Some("updater_user"));
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes(), Vec::<String>::new());
}

/// Catches: the account made with another id, a visible or shelled or password account,
/// a group that does not match the id, and directories that are root's or the wrong
/// mode: the state directory holds the agent's key and must be the account's, 700; the
/// log directory the account's, 755.
#[test]
fn apply_makes_the_hidden_account_with_the_configured_id_and_its_directories() {
    let mac = without_account("updater-apply");
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(check(&mac).failed, 0);
    {
        let s = mac.state.borrow();
        let user = &s.records[USER];
        for (attr, want) in [
            ("UniqueID", "470"),
            ("PrimaryGroupID", "470"),
            ("UserShell", "/usr/bin/false"),
            ("NFSHomeDirectory", "/var/empty"),
            ("IsHidden", "1"),
            ("Password", "*"),
        ] {
            assert!(
                user.contains(&(attr.to_owned(), want.to_owned())),
                "{attr}: {user:?}"
            );
        }
        assert!(
            !user.iter().any(|(a, _)| a == "AuthenticationAuthority"),
            "{user:?}"
        );
        assert!(s.records[GROUP].contains(&("PrimaryGroupID".to_owned(), "470".to_owned())));
    }
    assert_eq!(ls(&mac, STATE_DIR), "drwx------ 470 470");
    assert_eq!(ls(&mac, LOG_DIR), "drwxr-xr-x 470 470");
    assert_eq!(
        r.text.lines().collect::<Vec<_>>(),
        [
            "CHANGE updater_user: account and group _kbf-updater, id 470",
            "CHANGE updater_user: /var/db/kbf-updater, _kbf-updater's, mode 700",
            "CHANGE updater_user: /Library/Logs/kbf-updater, _kbf-updater's, mode 755",
        ]
    );
}

/// Catches: `apply` changing a Mac that already passes: a second run prints nothing and
/// runs no program that writes.
#[test]
fn a_second_apply_changes_nothing() {
    let mac = without_account("updater-twice");
    assert_eq!(apply(&mac).failed, 0);
    let before = mac.writes().len();
    let r = apply(&mac);
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.writes().len(), before, "{:?}", mac.writes());
}

/// Catches: the account's id left to chance when the profile does not fix it: it is the
/// highest free id below 500, skipping the role account's, and it stays on a second run.
#[test]
fn an_unset_id_is_the_highest_free_one_and_is_kept() {
    let mut mac = FakeMac::converged("updater-auto-id");
    let text = format!("{}updater_user=_kbf-updater\n", mac.profile_text());
    mac.set_profile(text);
    mac.state_mut().user("builder", 499);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(ls(&mac, STATE_DIR), "drwx------ 498 498");
    assert_eq!(apply(&mac).text, "");
}

/// Catches: the account passing with an attribute drifted, or an extra password, or
/// directories of the wrong owner or mode: each fails this item and no other.
#[test]
fn check_fails_each_drift_of_the_account_and_its_directories() {
    let drifted = |name: &str, drift: fn(&mut FakeMac)| {
        let mut mac = converged_updater(name);
        drift(&mut mac);
        mac
    };
    only_fails(
        &drifted("updater-visible", |m| {
            m.state_mut().user_attr("_kbf-updater", "IsHidden", "0");
        }),
        "IsHidden=0, want 1",
    );
    only_fails(
        &drifted("updater-shell", |m| {
            m.state_mut()
                .user_attr("_kbf-updater", "UserShell", "/bin/sh");
        }),
        "UserShell=/bin/sh, want /usr/bin/false",
    );
    only_fails(
        &drifted("updater-password", |m| {
            m.state_mut().user_attr(
                "_kbf-updater",
                "AuthenticationAuthority",
                ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>",
            );
        }),
        "has a password",
    );
    only_fails(
        &drifted("updater-group", |m| {
            m.state_mut().attr(GROUP, "PrimaryGroupID", "471");
        }),
        "group _kbf-updater has gid 471, want 470",
    );
    only_fails(
        &drifted("updater-state-owner", |m| m.own(STATE_DIR, 0, 0)),
        "/var/db/kbf-updater is drwx------ 0 0",
    );
    only_fails(
        &drifted("updater-state-mode", |m| m.chmod(STATE_DIR, 0o755)),
        "/var/db/kbf-updater is drwxr-xr-x 470 470",
    );
    only_fails(
        &drifted("updater-log-missing", |m| m.remove(LOG_DIR)),
        "/Library/Logs/kbf-updater is missing",
    );
    only_fails(
        &drifted("updater-uid-held", |m| {
            m.state_mut().user("builder", ID);
        }),
        "uid 470 belongs to builder",
    );
    only_fails(
        &drifted("updater-gid-held", |m| {
            m.state_mut()
                .attr("/Groups/builders", "PrimaryGroupID", "470");
        }),
        "gid 470 belongs to builders",
    );
    only_fails(&without_account("updater-absent"), "no user _kbf-updater");
}

/// Catches: `apply` taking an id another user or group holds, which would hand that
/// account's files to the updater: nothing is created and the item says why.
#[test]
fn apply_refuses_an_id_another_account_holds() {
    let mut mac = without_account("updater-held");
    mac.state_mut().user("builder", ID);
    let r = apply(&mac);
    assert!(
        r.text
            .contains("NOTE updater_user: uid 470 belongs to builder: _kbf-updater not changed"),
        "{}",
        r.text
    );
    assert_eq!(r.failed, 1);
    assert!(!mac.state.borrow().records.contains_key(USER));
    assert!(!mac.path(STATE_DIR).exists());
    let mut mac = without_account("updater-gid-held-apply");
    mac.state_mut()
        .attr("/Groups/builders", "PrimaryGroupID", "470");
    let r = apply(&mac);
    assert!(
        r.text
            .contains("NOTE updater_user: gid 470 belongs to builders"),
        "{}",
        r.text
    );
    assert!(!mac.state.borrow().records.contains_key(USER));
}

/// Catches: an id equal to the role account's accepted, which would make two accounts
/// of one uid; and the other malformed keys accepted.
#[test]
fn the_profile_refuses_a_clash_with_the_role_id_and_other_bad_keys() {
    for (extra, said) in [
        (
            "updater_user=_kbf-updater\nupdater_id=480\n",
            "updater_id: 480 is role_id",
        ),
        (
            "vm_user_uid=460\nupdater_user=_kbf-updater\nupdater_id=460\n",
            "updater_id: 460 is vm_user_uid",
        ),
        (
            "updater_user=_kbf-updater\nupdater_id=199\n",
            "updater_id: 199 is not a number from 200 to 499",
        ),
        (
            "updater_user=_kbf-updater\nupdater_id=500\n",
            "updater_id: 500 is not a number from 200 to 499",
        ),
        ("updater_id=470\n", "updater_id: needs updater_user"),
        (
            "updater_user=_other\n",
            "updater_user: _other is not _kbf-updater, the account the updater agent's property list runs as",
        ),
    ] {
        assert_eq!(refusal(&profile_with(extra)), said, "{extra}");
    }
    assert_eq!(
        refusal(&profile_with("updater_user=_kbf-updater\nupdater_id=470\n")),
        "accepted"
    );
    assert_eq!(
        refusal(&profile_with("updater_user=_kbf-updater\n")),
        "accepted"
    );
}

/// Catches: the read-only verb running a program that changes the Mac, or making the
/// account or a directory: `check` on a Mac with none made only reads.
#[test]
fn check_runs_no_command_that_changes_the_mac() {
    let mac = without_account("updater-check-only");
    let r = check_keys(&mac, Some("updater_user"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert_eq!(calls(&mac, "dscl . <stdin"), Vec::<String>::new());
    assert!(!mac.path(STATE_DIR).exists());
}
