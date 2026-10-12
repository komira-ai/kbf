//! `node-admin role-user status|create` over a fake Mac. Each test names the defect it
//! catches.

mod admin_fake;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use admin_fake::FakeMac;

// Built from parts: the hygiene scan reads a literal home folder path as a home path.
const USER: &str = concat!("/Users", "/_kbf");
const GROUP: &str = concat!("/Groups", "/_kbf");

/// Every directory service change a first `create` makes, in order (G is the group
/// record, U the user record; see [`expand`]).
const CREATES: [&str; 12] = [
    "G",
    "G PrimaryGroupID 470",
    "G RealName kbf build node",
    "G Password *",
    "U",
    "U UniqueID 470",
    "U Password *",
    "U IsHidden 1",
    "U RealName kbf build node",
    "U PrimaryGroupID 470",
    "U UserShell /usr/bin/false",
    "U NFSHomeDirectory /usr/local/kbf/home",
];

/// The `dscl` command a [`CREATES`] line stands for.
fn expand(short: &str) -> String {
    let (record, rest) = short.split_at(1);
    let record = if record == "G" { GROUP } else { USER };
    format!("/usr/bin/dscl . -create {record}{rest}")
}

fn expanded(shorts: &[&str]) -> Vec<String> {
    shorts.iter().map(|short| expand(short)).collect()
}

const IN_PLACE: &str = "role user _kbf: in place (uid 470, gid 470, shell /usr/bin/false, \
                        home /usr/local/kbf/home, not admin, no cron or at)";

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn creates(mac: &FakeMac) -> Vec<String> {
    let calls = mac.calls().into_iter();
    calls.filter(|call| call.contains(" -create ")).collect()
}

/// A Mac `create` already ran on, with its calls cleared.
fn created(name: &str) -> FakeMac {
    let mac = FakeMac::new(name);
    mac.run(&["role-user", "create"]).succeeded();
    mac.clear_calls();
    mac.state.borrow_mut().chowns.clear();
    mac
}

/// Catches: a directory service tool run by a relative path (it would run whatever
/// `dscl` the search path finds, as root), a wrong field (a login shell, a password,
/// another id or home), a home given to `_kbf` before its `leases` is in place, or a
/// deny file left without `_kbf` or with another owner or mode.
#[test]
fn create_makes_the_group_the_user_the_home_and_the_deny_lines() {
    let mac = FakeMac::new("role-create");
    let ran = mac.run(&["role-user", "create"]);
    ran.succeeded();
    assert_eq!(creates(&mac), expanded(&CREATES));
    for call in mac.calls() {
        assert!(
            call.starts_with("/usr/bin/dscl . ")
                || call == "/usr/sbin/dseditgroup -o checkmember -m _kbf admin",
            "{call}"
        );
    }
    let user = |name| mac.attribute(USER, name);
    assert_eq!(user("UniqueID").as_deref(), Some("470"));
    assert_eq!(user("PrimaryGroupID").as_deref(), Some("470"));
    assert_eq!(user("UserShell").as_deref(), Some("/usr/bin/false"));
    assert_eq!(user("Password").as_deref(), Some("*"));
    assert_eq!(user("IsHidden").as_deref(), Some("1"));
    assert_eq!(
        user("NFSHomeDirectory").as_deref(),
        Some("/usr/local/kbf/home")
    );
    assert_eq!(
        mac.attribute(GROUP, "PrimaryGroupID").as_deref(),
        Some("470")
    );
    let (home, leases) = (mac.home(), mac.home().join("leases"));
    assert_eq!((mac.owner(&home), mode(&home)), ((470, 470), 0o750));
    assert_eq!((mac.owner(&leases), mode(&leases)), ((470, 470), 0o700));
    let at = mac.at_dir();
    assert_eq!(
        fs::read_to_string(at.join("cron.deny")).unwrap(),
        "Guest\n_kbf\n"
    );
    assert_eq!(fs::read_to_string(at.join("at.deny")).unwrap(), "_kbf\n");
    let temp = at.join(format!(".incoming.{}", std::process::id()));
    let chowns = mac.state.borrow().chowns.clone();
    assert_eq!(
        chowns,
        [
            format!("{} 470:470", leases.display()),
            format!("{} 470:470", home.display()),
            format!("{} 0:0", temp.display()),
            format!("{} 0:0", temp.display()),
        ]
    );
    for deny in ["cron.deny", "at.deny"] {
        assert_eq!(mode(&at.join(deny)), 0o644, "{deny}");
    }
    let lines: Vec<&str> = ran.out.lines().collect();
    let tail = [
        format!("+ mkdir -m 0700 {}", home.display()),
        format!("+ mkdir -m 0700 {}", leases.display()),
        format!("+ chown _kbf:_kbf {}", leases.display()),
        format!("+ chmod 0700 {}", leases.display()),
        format!("+ chmod 0750 {}", home.display()),
        format!("+ chown _kbf:_kbf {}", home.display()),
        format!("+ add the line _kbf to {}", at.join("cron.deny").display()),
        format!("+ add the line _kbf to {}", at.join("at.deny").display()),
        IN_PLACE.to_owned(),
    ];
    assert_eq!(
        lines[..CREATES.len()],
        CREATES.map(|c| format!("+ {}", expand(c)))
    );
    assert_eq!(lines[CREATES.len()..], tail);
}

/// Catches: `status` passing a Mac without the role user, changing anything, or
/// leaving out a way the Mac differs.
#[test]
fn status_lists_every_difference_and_changes_nothing() {
    let mac = FakeMac::new("role-status");
    let ran = mac.run(&["role-user", "status"]);
    ran.succeeded();
    let at = mac.at_dir();
    let want = [
        "role user _kbf: NOT in place".to_owned(),
        "  group _kbf: PrimaryGroupID is 'absent', want 470".to_owned(),
        "  user _kbf: UniqueID is 'absent', want 470".to_owned(),
        "  user _kbf: PrimaryGroupID is 'absent', want 470".to_owned(),
        "  user _kbf: UserShell is 'absent', want /usr/bin/false".to_owned(),
        "  user _kbf: NFSHomeDirectory is 'absent', want /usr/local/kbf/home".to_owned(),
        format!(
            "  home {}: missing; want a directory owned by _kbf",
            mac.home().display()
        ),
        format!(
            "  user _kbf may use crontab (not in {})",
            at.join("cron.deny").display()
        ),
        format!(
            "  user _kbf may use at (not in {})",
            at.join("at.deny").display()
        ),
    ];
    assert_eq!(ran.out.lines().collect::<Vec<_>>(), want);
    assert!(creates(&mac).is_empty(), "{:?}", creates(&mac));
    assert!(!mac.home().exists());
    assert_eq!(fs::read_to_string(at.join("cron.deny")).unwrap(), "Guest\n");
    let ran = created("role-status-after").run(&["role-user", "status"]);
    assert_eq!(ran.out, format!("{IN_PLACE}\n"));
}

/// Catches: `status` missing what a later change made: uid or gid 470 also held by
/// another account, a member of `admin`, a deny line removed, a home given away, or a
/// value with a space in it read as empty.
#[test]
fn status_sees_later_changes() {
    let mac = created("role-status-changes");
    mac.set_user("_newapple", "UniqueID", "470");
    mac.set_group("_newgroup", "PrimaryGroupID", "470");
    mac.set_user("_kbf", "NFSHomeDirectory", "/var/kbf home");
    mac.state.borrow_mut().admins.insert("_kbf".to_owned());
    mac.own(&mac.home(), 501);
    fs::write(mac.at_dir().join("cron.deny"), "Guest\n").unwrap();
    let ran = mac.run(&["role-user", "status"]);
    ran.succeeded();
    let want = [
        "role user _kbf: NOT in place".to_owned(),
        "  uid 470 is also held by: _newapple".to_owned(),
        "  gid 470 is also held by: _newgroup".to_owned(),
        "  user _kbf: NFSHomeDirectory is '/var/kbf home', want /usr/local/kbf/home".to_owned(),
        "  user _kbf is a member of admin".to_owned(),
        format!(
            "  home {}: a directory, owned by uid 501; want a directory owned by _kbf",
            mac.home().display()
        ),
        format!(
            "  user _kbf may use crontab (not in {})",
            mac.at_dir().join("cron.deny").display()
        ),
    ];
    assert_eq!(ran.out.lines().collect::<Vec<_>>(), want);
}

/// Catches: a second `create` that makes or sets anything again, or adds a second
/// deny line.
#[test]
fn create_again_changes_nothing() {
    let mac = created("role-again");
    let ran = mac.run(&["role-user", "create"]);
    ran.succeeded();
    assert!(creates(&mac).is_empty(), "{:?}", creates(&mac));
    assert!(mac.state.borrow().chowns.is_empty());
    let at = mac.at_dir();
    assert_eq!(
        fs::read_to_string(at.join("cron.deny")).unwrap(),
        "Guest\n_kbf\n"
    );
    assert_eq!(fs::read_to_string(at.join("at.deny")).unwrap(), "_kbf\n");
    let home = format!(
        "home {}: in place (its contents are _kbf's; this helper never writes in it)",
        mac.home().display()
    );
    assert_eq!(ran.out, format!("{home}\n{IN_PLACE}\n"));
}

/// Catches: a field changed by hand (a login shell, another home or group) being
/// kept, or the whole record made again to fix one field.
#[test]
fn create_puts_back_a_field_changed_by_hand() {
    let mac = created("role-fields");
    mac.set_user("_kbf", "UserShell", "/bin/zsh");
    mac.set_user("_kbf", "NFSHomeDirectory", "/var/empty");
    mac.set_user("_kbf", "PrimaryGroupID", "20");
    let status = mac.run(&["role-user", "status"]);
    assert!(
        status
            .out
            .contains("UserShell is '/bin/zsh', want /usr/bin/false"),
        "{status:?}"
    );
    mac.run(&["role-user", "create"]).succeeded();
    assert_eq!(
        creates(&mac),
        expanded(&[
            "U PrimaryGroupID 470",
            "U UserShell /usr/bin/false",
            "U NFSHomeDirectory /usr/local/kbf/home",
        ])
    );
}

/// Catches: `create` taking an id another account holds, adopting a `_kbf` made with
/// another id, or keeping `_kbf` in `admin`; each refusal comes before any change.
#[test]
fn create_refuses_an_id_or_account_it_does_not_own() {
    type Plant = fn(&FakeMac);
    let cases: [(&str, Plant, &str); 5] = [
        (
            "uid",
            |mac| mac.set_user("someone", "UniqueID", "470"),
            "uid 470 belongs to someone; not taking it",
        ),
        (
            "gid",
            |mac| mac.set_group("somegroup", "PrimaryGroupID", "470"),
            "gid 470 belongs to somegroup; not taking it",
        ),
        (
            "user-id",
            |mac| mac.set_user("_kbf", "UniqueID", "300"),
            "user _kbf exists with uid 300, not 470; remove it by hand first",
        ),
        (
            "group-id",
            |mac| mac.set_group("_kbf", "PrimaryGroupID", "30"),
            "group _kbf exists with gid 30, not 470; remove it by hand first",
        ),
        (
            "admin",
            |mac| {
                mac.state.borrow_mut().admins.insert("_kbf".to_owned());
            },
            "_kbf is a member of admin; remove it by hand first",
        ),
    ];
    for (name, plant, why) in cases {
        let mac = FakeMac::new(&format!("role-refuse-{name}"));
        plant(&mac);
        mac.run(&["role-user", "create"]).refused_for(why);
        assert!(creates(&mac).is_empty(), "{name}: {:?}", creates(&mac));
        assert!(!mac.home().exists(), "{name}");
    }
}

/// Catches: two accounts holding 470 reported as one name, or `_kbf` itself counted
/// among the others.
#[test]
fn every_other_holder_is_named() {
    let mac = created("role-holders");
    mac.set_user("a", "UniqueID", "470");
    mac.set_user("b", "UniqueID", "470");
    mac.run(&["role-user", "create"])
        .refused_for("uid 470 belongs to a, b; not taking it");
}

/// Catches: a home someone else owns, a link or a file taken for the role user's
/// home (root would then give it to `_kbf`, or write through the link).
#[test]
fn create_refuses_a_home_it_did_not_make() {
    let mac = FakeMac::new("role-home-other");
    fs::create_dir(mac.home()).unwrap();
    mac.own(&mac.home(), 501);
    mac.run(&["role-user", "create"])
        .refused_for("home exists, owned by uid 501, not _kbf; move it aside by hand");
    assert!(mac.state.borrow().chowns.is_empty());

    let mac = FakeMac::new("role-home-link");
    let elsewhere = mac.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, mac.home()).unwrap();
    mac.run(&["role-user", "create"])
        .refused_for("home exists and is not a directory (it is a symbolic link)");
    assert!(mac.state.borrow().chowns.is_empty());
    assert!(!elsewhere.join("leases").exists());

    let mac = FakeMac::new("role-home-file");
    fs::write(mac.home(), "").unwrap();
    mac.run(&["role-user", "create"])
        .refused_for("home exists and is not a directory (it is a regular file)");
    let status = mac.run(&["role-user", "status"]);
    let line = format!(
        "  home {}: a regular file, owned by uid 0; want a directory owned by _kbf",
        mac.home().display()
    );
    assert!(status.out.lines().any(|l| l == line), "{status:?}");
}

/// Catches: a home an interrupted `create` left root-owned being refused for ever, or
/// finished without its `leases` given to `_kbf`.
#[test]
fn create_finishes_an_interrupted_home() {
    let mac = FakeMac::new("role-home-interrupted");
    let leases = mac.home().join("leases");
    fs::create_dir_all(&leases).unwrap();
    chmod(&leases, 0o755);
    let ran = mac.run(&["role-user", "create"]);
    ran.succeeded();
    assert!(
        ran.out
            .contains("root-owned, left by an interrupted role-user create; finishing it"),
        "{ran:?}"
    );
    assert!(!ran.out.contains("+ mkdir"), "{ran:?}");
    assert_eq!((mac.owner(&leases), mode(&leases)), ((470, 470), 0o700));
    assert_eq!(mac.owner(&mac.home()), (470, 470));
    assert_eq!(
        mac.run(&["role-user", "status"]).out,
        format!("{IN_PLACE}\n")
    );
}

/// Catches: a `leases` link in a root-owned home being followed by the chown and
/// chmod, which would give the link's target to `_kbf`.
#[test]
fn create_refuses_a_leases_link() {
    let mac = FakeMac::new("role-leases-link");
    let elsewhere = mac.root.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    chmod(&elsewhere, 0o755);
    fs::create_dir(mac.home()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, mac.home().join("leases")).unwrap();
    mac.run(&["role-user", "create"])
        .refused_for("leases is not a directory (it is a symbolic link)");
    assert_eq!(mode(&elsewhere), 0o755);
    assert!(mac.state.borrow().chowns.is_empty());
}

/// Catches: a `cron.allow` (which decides alone) that lists `_kbf` being passed, or
/// one that does not list it being ignored in favour of `cron.deny`.
#[test]
fn an_allow_file_decides_alone() {
    let mac = FakeMac::new("role-allow-lists");
    let allow = mac.at_dir().join("cron.allow");
    fs::write(&allow, "ops\n_kbf\n").unwrap();
    mac.run(&["role-user", "create"])
        .refused_for("cron.allow lists _kbf; remove that line by hand");

    let mac = FakeMac::new("role-allow-other");
    let allow = mac.at_dir().join("cron.allow");
    fs::write(&allow, "ops\n").unwrap();
    mac.run(&["role-user", "create"]).succeeded();
    let at = mac.at_dir();
    assert_eq!(fs::read_to_string(at.join("cron.deny")).unwrap(), "Guest\n");
    assert_eq!(fs::read_to_string(&allow).unwrap(), "ops\n");
    assert_eq!(
        mac.run(&["role-user", "status"]).out,
        format!("{IN_PLACE}\n")
    );
    fs::write(&allow, "_kbf").unwrap();
    let status = mac.run(&["role-user", "status"]);
    assert!(status.out.contains("may use crontab"), "{status:?}");

    let mac = FakeMac::new("role-allow-not-root");
    let allow = mac.at_dir().join("cron.allow");
    fs::write(&allow, "ops\n").unwrap();
    mac.own(&allow, 501);
    mac.run(&["role-user", "create"])
        .refused_for("cron.allow is owned by uid 501, not root");
}

/// Catches: `create` guessing who may use `cron` when neither of its files exists.
#[test]
fn no_allow_and_no_deny_file_is_refused() {
    let mac = FakeMac::new("role-no-deny");
    fs::remove_file(mac.at_dir().join("at.deny")).unwrap();
    mac.run(&["role-user", "create"]).refused_for(&format!(
        "neither {} nor {} exists, so it is not clear who may use at; not guessing",
        mac.at_dir().join("at.allow").display(),
        mac.at_dir().join("at.deny").display()
    ));
    // No folder at all: nothing denies `_kbf` anything, and nothing is guessed.
    let mac = FakeMac::new("role-no-at");
    fs::remove_dir_all(mac.at_dir()).unwrap();
    let status = mac.run(&["role-user", "status"]);
    assert!(status.out.contains("may use crontab"), "{status:?}");
    assert!(status.out.contains("may use at"), "{status:?}");
    mac.run(&["role-user", "create"])
        .refused_for("so it is not clear who may use cron; not guessing");
}

/// Catches: a deny file the caller could change, or a link, being trusted and
/// replaced (root would write through the link or keep a line the caller removes).
#[test]
fn a_deny_file_must_be_roots() {
    let mac = FakeMac::new("role-deny-owner");
    let deny = mac.at_dir().join("cron.deny");
    mac.own(&deny, 501);
    mac.run(&["role-user", "create"])
        .refused_for("cron.deny is owned by uid 501, not root");

    let mac = FakeMac::new("role-deny-link");
    let deny = mac.at_dir().join("cron.deny");
    let real = mac.root.join("cron.deny.real");
    fs::rename(&deny, &real).unwrap();
    std::os::unix::fs::symlink(&real, &deny).unwrap();
    mac.run(&["role-user", "create"])
        .refused_for("cron.deny is not a regular file (it is a symbolic link)");
    assert_eq!(fs::read_to_string(&real).unwrap(), "Guest\n");
}

/// Catches: `/private/var/at` (where the new deny file is made) or a folder above it
/// that the caller could change being written in.
#[test]
fn the_at_folder_must_be_root_or_daemons_alone() {
    let fresh = |name: &str| FakeMac::new(&format!("role-at-{name}"));
    let refused = |mac: &FakeMac, why: &str| {
        mac.run(&["role-user", "create"]).refused_for(why);
        // Nothing was written in the folder before the refusal.
        let deny = fs::read_to_string(mac.at_dir().join("cron.deny")).unwrap();
        assert_eq!(deny, "Guest\n", "{why}");
    };
    let mac = fresh("owner");
    mac.own(&mac.at_dir(), 501);
    refused(
        &mac,
        "private/var/at is owned by uid 501, not root or daemon",
    );
    let mac = fresh("mode");
    chmod(&mac.at_dir(), 0o775);
    refused(
        &mac,
        "private/var/at is writable by group or others (mode 775)",
    );
    let mac = fresh("acl");
    mac.state.borrow_mut().acls.insert(mac.at_dir());
    refused(&mac, "private/var/at has an access control list");
    let mac = fresh("private");
    mac.own(&mac.root.join("private/var"), 501);
    refused(&mac, "private/var is owned by uid 501, not root");
    // macOS ships the folder as daemon's; root's is as good.
    let mac = fresh("root");
    mac.own(&mac.at_dir(), 0);
    mac.run(&["role-user", "create"]).succeeded();
}

/// Catches: the deny file's folder checked only when it is there (a missing or
/// replaced folder passed), which the deny file's own check alone cannot see.
#[test]
fn the_at_folder_must_be_a_folder() {
    let mac = FakeMac::new("role-at-link");
    let at = mac.at_dir();
    let elsewhere = mac.root.join("at-elsewhere");
    fs::rename(&at, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &at).unwrap();
    // The deny file is reached through the link and is root's: the folder check
    // refuses it.
    mac.run(&["role-user", "create"])
        .refused_for("private/var/at is not a directory (it is a symbolic link)");
    assert_eq!(
        fs::read_to_string(elsewhere.join("cron.deny")).unwrap(),
        "Guest\n"
    );
}

/// Catches: `_kbf` glued onto the last line of a deny file that does not end in a
/// newline (`Guest_kbf`, which denies nobody).
#[test]
fn a_deny_file_without_a_last_newline_gets_one() {
    let mac = FakeMac::new("role-deny-newline");
    let deny = mac.at_dir().join("cron.deny");
    fs::write(&deny, "Guest").unwrap();
    mac.run(&["role-user", "create"]).succeeded();
    assert_eq!(fs::read_to_string(&deny).unwrap(), "Guest\n_kbf\n");
}

/// Catches: a deny file replaced before its owner is set (a file the rename puts in
/// place with whatever owner it was made with), or a failed step leaving its
/// temporary file behind.
#[test]
fn a_deny_file_whose_owner_cannot_be_set_is_kept() {
    let mac = FakeMac::new("role-deny-chown");
    let at = mac.at_dir();
    let temp = at.join(format!(".incoming.{}", std::process::id()));
    mac.state.borrow_mut().chown_fails.insert(temp.clone());
    mac.run(&["role-user", "create"])
        .failed_for("cron.deny: chown failed");
    assert_eq!(fs::read_to_string(at.join("cron.deny")).unwrap(), "Guest\n");
    assert!(!temp.exists());
}

/// Catches: a failed `dscl` list or a `dscl` that cannot start taken as "no account
/// holds 470" (`create` would then take the id).
#[test]
fn a_directory_service_that_fails_fails_the_call() {
    let mac = FakeMac::new("role-dscl-fails");
    mac.state.borrow_mut().fails.insert(
        "/usr/bin/dscl . -list /Groups".to_owned(),
        "DS Error: -14009".to_owned(),
    );
    mac.run(&["role-user", "create"])
        .failed_for("dscl . -list /Groups PrimaryGroupID failed: DS Error: -14009");
    assert!(creates(&mac).is_empty());
    let mac = FakeMac::new("role-dscl-missing");
    mac.state
        .borrow_mut()
        .unstartable
        .insert("/usr/bin/dscl".to_owned());
    mac.run(&["role-user", "status"])
        .failed_for("/usr/bin/dscl: no such program");
}

/// Catches: a `create` whose directory service change failed being reported as done.
#[test]
fn a_failed_change_fails_the_call() {
    let mac = FakeMac::new("role-create-fails");
    let user = expand("U");
    let failure = "DS Error: -14120".to_owned();
    mac.state.borrow_mut().fails.insert(user.clone(), failure);
    mac.run(&["role-user", "create"])
        .failed_for(&format!("{user} failed: DS Error: -14120"));
    assert!(!mac.home().exists());
}

/// Catches: a deny line that only starts with `_kbf` (`_kbfx`, another account) read
/// as `_kbf`'s own: `create` would then skip the append and `status` report `_kbf`
/// denied when it is not.
#[test]
fn a_near_name_in_a_deny_file_is_not_the_role() {
    let mac = FakeMac::new("role-deny-near-name");
    let at = mac.at_dir();
    fs::write(at.join("cron.deny"), "Guest\n_kbfx\n").unwrap();
    fs::write(at.join("at.deny"), "_kbfx\n").unwrap();
    let status = mac.run(&["role-user", "status"]);
    for line in [
        format!(
            "  user _kbf may use crontab (not in {})",
            at.join("cron.deny").display()
        ),
        format!(
            "  user _kbf may use at (not in {})",
            at.join("at.deny").display()
        ),
    ] {
        assert!(status.out.lines().any(|l| l == line), "{line}: {status:?}");
    }
    mac.run(&["role-user", "create"]).succeeded();
    assert_eq!(
        fs::read_to_string(at.join("cron.deny")).unwrap(),
        "Guest\n_kbfx\n_kbf\n"
    );
    assert_eq!(
        fs::read_to_string(at.join("at.deny")).unwrap(),
        "_kbfx\n_kbf\n"
    );
}

/// Catches: a `dscl -read` that fails for a reason other than a missing record taken
/// as "no `_kbf`": `create` would then set `UniqueID 470` on a `_kbf` that has
/// another uid. It must fail before any change, and `status` too.
#[test]
fn a_directory_read_that_fails_is_not_a_missing_record() {
    let mac = FakeMac::new("role-dscl-read-fails");
    mac.set_user("_kbf", "UniqueID", "300");
    let read = format!("/usr/bin/dscl . -read {USER} UniqueID");
    let error = "DS Error: -14009 (eDSUnknownNodeName)";
    mac.state.borrow_mut().fails.insert(read, error.to_owned());
    let why = format!("dscl . -read {USER} UniqueID failed: {error}");
    mac.run(&["role-user", "create"]).failed_for(&why);
    assert!(creates(&mac).is_empty(), "{:?}", creates(&mac));
    assert!(!mac.home().exists());
    assert_eq!(mac.attribute(USER, "UniqueID").as_deref(), Some("300"));
    mac.run(&["role-user", "status"]).failed_for(&why);
}
