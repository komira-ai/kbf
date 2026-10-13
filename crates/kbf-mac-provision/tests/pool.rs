//! The lease-user pool, its volume and the `daemon_user` gate against a fake Mac
//! (docs/design/macos-end-state.md, step 2): the cases `drift.rs`'s one-item table
//! cannot hold (an administrator also fails `admin_user`), what `apply` must not do
//! while the volume is not mounted, and the root daemon a profile opts in to.

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use kbf_mac_provision::launchd::render_plist;
use kbf_mac_provision::profile::Profile;
use support::{
    ERR_LOG, FakeMac, HOMES, KEY, PLIST, SCRATCH, apply, calls, check, golden_plist,
    golden_plist_as, lines, ls, sha256_hex,
};

/// The profile of `mac` with `extra` lines added (the property list follows it).
fn add_to_profile(mac: &mut FakeMac, extra: &str) {
    let text = format!("{}{extra}", mac.profile_text());
    mac.set_profile(text);
}

/// Catches: a pool user that is an administrator passing `pool_users` (the mutant:
/// drop the admin check; `admin_user` still fails, so this asserts the pool line), and
/// an `apply` that leaves it an administrator.
#[test]
fn a_pool_user_that_is_an_administrator_fails_and_apply_demotes_it() {
    let mut mac = FakeMac::converged("pool-admin");
    mac.state_mut().attr(
        "/Groups/admin",
        "GroupMembership",
        "root farmadmin kbf-pool-1",
    );
    let r = check(&mac);
    assert_eq!(
        lines(&r, "FAIL"),
        [
            "FAIL admin_user: other administrators: kbf-pool-1 [check only: apply does not change this]",
            "FAIL pool_users: kbf-pool-1: an administrator",
        ],
        "{}",
        r.text
    );
    mac.log.borrow_mut().clear();
    let r = apply(&mac);
    assert_eq!(
        (r.failed, r.text.as_str()),
        (0, "CHANGE pool_users: kbf-pool-1 removed from admin\n")
    );
    assert_eq!(
        calls(&mac, "dseditgroup -o edit"),
        ["dseditgroup -o edit -d kbf-pool-1 -t user admin"]
    );
}

/// Catches: a pool user that is an administrator by primary group 80 passing.
#[test]
fn a_pool_user_in_the_admin_group_by_primary_group_fails() {
    let mut mac = FakeMac::converged("pool-admin-primary");
    mac.state_mut()
        .user_attr("kbf-pool-0", "PrimaryGroupID", "80");
    let r = check(&mac);
    assert!(
        lines(&r, "FAIL").contains(
            &"FAIL pool_users: kbf-pool-0: PrimaryGroupID=80, want 700; kbf-pool-0: an administrator"
        ),
        "{}",
        r.text
    );
    assert_eq!(apply(&mac).failed, 0);
}

/// Catches: `apply` making the pool's homes while the volume is not mounted: they
/// would land on the system volume under `/Volumes`, where a later mount hides them.
#[test]
fn with_the_volume_not_mounted_apply_makes_the_users_but_no_home() {
    let mut mac = FakeMac::fresh("pool-unmounted");
    mac.state_mut().volumes.clear();
    let r = apply(&mac);
    assert!(
        r.text
            .contains("CHANGE pool_users: user kbf-pool-3, uid 703\n"),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("NOTE pool_users: /Volumes/kbf is not a mounted APFS volume: no home made\n"),
        "{}",
        r.text
    );
    assert!(
        r.text.contains(
            "NOTE kbf_volume: /Volumes/kbf is not a mounted APFS volume: make it with diskutil apfs addVolume (apply does not make volumes)\n"
        ),
        "{}",
        r.text
    );
    assert!(!mac.path(HOMES).exists());
    assert!(r.text.contains("FAIL pool_users: "), "{}", r.text);
}

/// Catches: `apply` changing the owner or mode of what a pool home's path holds when
/// it is not a directory (a link would hand the uid whatever it names).
#[test]
fn a_pool_home_that_is_a_link_is_left() {
    let mac = FakeMac::converged("pool-home-link");
    let elsewhere = support::scratch("pool-home-link-target");
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o755)).unwrap();
    let home = format!("{HOMES}/kbf-pool-2");
    mac.remove(&home);
    mac.symlink(&elsewhere, &home);
    let r = check(&mac);
    assert!(
        lines(&r, "FAIL").iter().any(|l| l
            .starts_with("FAIL pool_users: kbf-pool-2: home /Volumes/kbf/home/kbf-pool-2 is lrwx")),
        "{}",
        r.text
    );
    mac.log.borrow_mut().clear();
    let r = apply(&mac);
    assert!(
        r.text.starts_with(
            "NOTE pool_users: /Volumes/kbf/home/kbf-pool-2 is not a directory; apply leaves it\n"
        ),
        "{}",
        r.text
    );
    assert_eq!(mac.writes(), Vec::<String>::new());
    assert_eq!(fs::metadata(&elsewhere).unwrap().mode() & 0o7777, 0o755);
}

/// Catches: `apply` making the pool's homes through a link at the homes directory
/// (the link target would hold directories owned by the pool uids). `dir_of` leaves
/// the link; the mutant is `fix_user` making the homes anyway, as it did before.
#[test]
fn a_homes_directory_that_is_a_link_gets_no_home() {
    let mac = FakeMac::converged("pool-homes-link");
    let elsewhere = support::scratch("pool-homes-link-target");
    mac.remove(HOMES);
    mac.symlink(&elsewhere, HOMES);
    let r = check(&mac);
    assert!(
        lines(&r, "FAIL")
            .iter()
            .any(|l| l.starts_with("FAIL pool_users: /Volumes/kbf/home is lrwx")),
        "{}",
        r.text
    );
    let r = apply(&mac);
    let made: Vec<_> = fs::read_dir(&elsewhere)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        made.is_empty(),
        "apply made {made:?} through {HOMES}:\n{}",
        r.text
    );
    assert!(
        r.text.starts_with(
            "NOTE pool_users: /Volumes/kbf/home is not a directory; apply leaves it\nNOTE pool_users: /Volumes/kbf/home is lrwx"
        ),
        "{}",
        r.text
    );
    assert!(
        r.text.contains(", not drwxr-xr-x 0 0: no home made\n"),
        "{}",
        r.text
    );
    assert_eq!(r.failed, 1, "{}", r.text);
}

/// Catches: the pool's size or first uid not read from the profile.
#[test]
fn the_pool_size_and_first_uid_come_from_the_profile() {
    let mut mac = FakeMac::converged("pool-size");
    add_to_profile(&mut mac, "pool_users=2\npool_first_uid=800\n");
    let r = check(&mac);
    assert!(
        r.text
            .contains("FAIL pool_users: group kbfpool: PrimaryGroupID=700, want 800;"),
        "{}",
        r.text
    );
    mac.converge_pool(2, 800);
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(
        r.text.contains(
            "PASS pool_users: 2 users kbf-pool-0..kbf-pool-1 (uids 800-801, group kbfpool 800)"
        ),
        "{}",
        r.text
    );
}

/// Catches: a default first uid that is not 600 when `lease_uids` ends below it (the
/// default is the uid after `lease_uids`, and never below 600).
#[test]
fn the_first_pool_uid_is_never_below_600() {
    let mut mac = FakeMac::converged("pool-600");
    let text = mac
        .profile_text()
        .replace("lease_uids=600-699", "lease_uids=501-550");
    mac.set_profile(text);
    let r = check(&mac);
    assert!(
        r.text
            .contains("FAIL pool_users: group kbfpool: PrimaryGroupID=700, want 600; kbf-pool-0: UniqueID=700, want 600;"),
        "{}",
        r.text
    );
}

/// Catches: `UserName` kept in the property list of a profile that sets
/// `daemon_user=root`, or dropped from one that does not (the golden test in
/// `check.rs` holds the default).
#[test]
fn a_root_daemon_renders_without_user_name() {
    let mac = FakeMac::fresh("pool-root-plist");
    let text = format!("{}daemon_user=root\n", mac.profile_text());
    let p = Profile::parse(text.as_bytes()).unwrap();
    assert_eq!(render_plist(&p), golden_plist_as(p.short_sha(), None));
    let text = format!("{}daemon_user=role\n", mac.profile_text());
    let p = Profile::parse(text.as_bytes()).unwrap();
    assert_eq!(render_plist(&p), golden_plist(p.short_sha()));
}

/// Catches: a root-daemon Mac as `apply` leaves it failing an item (the error log
/// still wanted as the role account's, the key or the roots read wrong).
#[test]
fn a_converged_root_mac_passes_every_item() {
    let mac = FakeMac::converged_root("pool-root");
    let r = check(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    for want in [
        "PASS daemon_user: root: /usr/local/kbf/etc/node.key -rw------- 0 0; /Volumes/kbf/scratch drwxr-xr-x 0 0\n",
        "PASS launchd_label: system/org.example.kbf-daemon loaded, as root, ProcessType Interactive\n",
    ] {
        assert!(r.text.contains(want), "lacks {want:?}:\n{}", r.text);
    }
}

type Drift = (&'static str, fn(&mut FakeMac), &'static str, bool);

/// The root-daemon drifts: what changed, the change, the text of the FAIL line, and
/// whether `apply` fixes it.
const ROOT_DRIFTS: &[Drift] = &[
    (
        "key readable by others",
        |m| m.chmod(KEY, 0o644),
        "/usr/local/kbf/etc/node.key is -rw-r--r-- 0 0, want -rw------- 0 0 with one link",
        true,
    ),
    (
        "key of the role account",
        |m| m.own(KEY, 480, 480),
        "/usr/local/kbf/etc/node.key is -rw------- 480 480, want",
        true,
    ),
    (
        "key missing",
        |m| m.remove(KEY),
        "/usr/local/kbf/etc/node.key is missing, want",
        false,
    ),
    (
        "key with two links",
        |m| fs::hard_link(m.path(KEY), m.path("/usr/local/kbf/etc/copy")).unwrap(),
        "/usr/local/kbf/etc/node.key is -rw------- 0 0, want -rw------- 0 0 with one link",
        false,
    ),
    (
        "key a link",
        |m| {
            m.remove(KEY);
            m.symlink(&m.path("/usr/local/kbf/etc/ca.pem"), KEY);
        },
        "/usr/local/kbf/etc/node.key is lrwx",
        false,
    ),
    (
        "scratch root of the role account",
        |m| m.own(SCRATCH, 480, 480),
        "/Volumes/kbf/scratch is drwxr-xr-x 480 480, want drwxr-xr-x 0 0",
        true,
    ),
    (
        "scratch root group-writable",
        |m| m.chmod(SCRATCH, 0o775),
        "/Volumes/kbf/scratch is drwxrwxr-x 0 0, want drwxr-xr-x 0 0",
        true,
    ),
    (
        "scratch root missing",
        |m| m.remove(SCRATCH),
        "/Volumes/kbf/scratch is missing, want drwxr-xr-x 0 0",
        true,
    ),
];

/// Catches: a node key of the wrong mode, owner or link count passing (mutant: drop
/// the key mode check), a scratch root a lease uid could write, and a fix that does
/// not take or that changes a key it cannot make safe.
#[test]
fn each_root_drift_fails_daemon_user_and_apply_fixes_it_or_leaves_it() {
    let wrong: Vec<String> = ROOT_DRIFTS
        .iter()
        .enumerate()
        .filter_map(|(n, (what, drift, text, fixed))| {
            let mut mac = FakeMac::converged_root(&format!("root-drift-{n}"));
            drift(&mut mac);
            let r = check(&mac);
            let failed = lines(&r, "FAIL");
            let fails = failed.len() == 1
                && failed[0].starts_with("FAIL daemon_user: ")
                && failed[0].contains(text);
            mac.log.borrow_mut().clear();
            let a = apply(&mac);
            let applied = if *fixed {
                a.failed == 0 && a.text.contains("CHANGE daemon_user: ") && check(&mac).failed == 0
            } else {
                a.failed == 1 && a.text.contains("NOTE daemon_user: ") && mac.writes().is_empty()
            };
            (!(fails && applied)).then(|| format!("{what}:\n{}\n{}", r.text, a.text))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Catches: the flip to a root daemon not taking on a Mac that runs it as the role
/// account: the key and the roots left to the role account, the property list kept
/// with `UserName`, the error log left the role account's, or the job restarted as
/// root before the key is root's.
#[test]
fn setting_daemon_user_root_on_a_role_mac_moves_the_daemon_to_root() {
    let mut mac = FakeMac::converged("pool-flip");
    let old = mac.profile_text();
    mac.write(KEY, "key\n");
    mac.chmod(KEY, 0o640);
    mac.own(KEY, 480, 480);
    mac.mkdir(SCRATCH);
    mac.own(SCRATCH, 480, 480);
    mac.chmod(SCRATCH, 0o755);
    add_to_profile(&mut mac, "daemon_user=root\n");
    mac.write(PLIST, &golden_plist(&sha256_hex(old.as_bytes())[..12]));
    let failed: Vec<String> = lines(&check(&mac), "FAIL")
        .iter()
        .map(|l| l.split(':').next().unwrap().to_owned())
        .collect();
    assert_eq!(failed, ["FAIL daemon_user", "FAIL launchd_label"]);
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        r.text.lines().collect::<Vec<_>>(),
        [
            "CHANGE daemon_user: /usr/local/kbf/etc/node.key root's, mode 600",
            "CHANGE daemon_user: /Volumes/kbf/scratch root's, mode 755",
            "CHANGE launchd_label: wrote /Library/LaunchDaemons/org.example.kbf-daemon.plist and restarted the job (error log truncated)",
        ]
    );
    let sha = sha256_hex(mac.profile_text().as_bytes());
    assert_eq!(
        fs::read_to_string(mac.path(PLIST)).unwrap(),
        golden_plist_as(&sha[..12], None)
    );
    assert_eq!(ls(&mac, KEY), "-rw------- 0 0");
    assert_eq!(ls(&mac, ERR_LOG), "-rw-r--r-- 0 0");
    assert_eq!(ls(&mac, SCRATCH), "drwxr-xr-x 0 0");
}

/// Catches: `kbf_cache` not checked or not made root's when it is set.
#[test]
fn the_cache_root_is_made_roots_when_set() {
    let mut mac = FakeMac::converged_root("pool-cache");
    add_to_profile(&mut mac, "kbf_cache=/Volumes/kbf/cache\n");
    let r = check(&mac);
    assert_eq!(
        lines(&r, "FAIL"),
        ["FAIL daemon_user: /Volumes/kbf/cache is missing, want drwxr-xr-x 0 0"]
    );
    let r = apply(&mac);
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(ls(&mac, "/Volumes/kbf/cache"), "drwxr-xr-x 0 0");
}

/// Catches: a root made on the system volume while the volume is not mounted, and a
/// root path that is not a directory changed (a link would make what it names root's).
#[test]
fn a_root_is_made_only_on_the_mounted_volume_and_never_through_a_link() {
    let mut mac = FakeMac::converged_root("pool-root-unmounted");
    mac.remove(SCRATCH);
    mac.state_mut().volumes.clear();
    let r = apply(&mac);
    assert!(
        r.text.contains(
            "NOTE daemon_user: /Volumes/kbf is not a mounted APFS volume: no root made\n"
        ),
        "{}",
        r.text
    );
    assert!(!mac.path(SCRATCH).exists());

    let mac = FakeMac::converged_root("pool-root-link");
    mac.remove(SCRATCH);
    mac.symlink(&mac.path("/Library"), SCRATCH);
    mac.log.borrow_mut().clear();
    let r = apply(&mac);
    assert_eq!(
        (r.failed, r.text.lines().next()),
        (
            1,
            Some("NOTE daemon_user: /Volumes/kbf/scratch is not a directory; apply leaves it")
        )
    );
    assert_eq!(mac.writes(), Vec::<String>::new());
}
