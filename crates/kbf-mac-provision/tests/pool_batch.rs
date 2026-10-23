//! The lease-user pool at its default size, made in one `dscl` process fed its
//! commands, and checked from one read of the directory, against a fake Mac.

mod support;

use support::{FakeMac, apply_keys, calls, check_keys, lines};

/// The directory of user records.
const USERS: &str = "/Users";

/// The commands that make pool user `i` of a pool from `first` that is not there.
fn make(i: u32, first: u32) -> String {
    let (name, uid) = (format!("kbf-pool-{i}"), first + i);
    let mut text = format!("create /Users/{name}\n");
    for (attr, value) in [
        ("UniqueID", uid.to_string()),
        ("PrimaryGroupID", first.to_string()),
        ("UserShell", "/usr/bin/false".to_owned()),
        ("NFSHomeDirectory", format!("/Volumes/kbf/home/{name}")),
        ("IsHidden", "1".to_owned()),
        ("Password", "*".to_owned()),
    ] {
        text.push_str(&format!("create /Users/{name} {attr} {value}\n"));
    }
    text
}

/// A converged Mac with no pool, whose profile has `pool`.
fn without_pool(name: &str, pool: Option<(u32, u32)>) -> FakeMac {
    let mut mac = FakeMac::converged(name);
    mac.remove_pool(4);
    mac.set_pool(pool);
    mac.log.borrow_mut().clear();
    mac
}

/// Catches: the default pool not being 100 users from uid 61000 (and group `kbfpool`
/// 61000), whatever `lease_uids` says: the check names the group and the last user.
#[test]
fn a_profile_without_pool_keys_wants_100_users_from_uid_61000() {
    let mut mac = FakeMac::converged("batch-defaults");
    mac.set_pool(None);
    let r = check_keys(&mac, Some("pool_users"));
    assert!(
        r.text.contains(
            "FAIL pool_users: group kbfpool: PrimaryGroupID=700, want 61000; kbf-pool-0: UniqueID=700, want 61000;"
        ),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("kbf-pool-99: UniqueID=absent, want 61099"),
        "{}",
        r.text
    );
    assert!(!r.text.contains("kbf-pool-100"), "{}", r.text);
    mac.converge_pool(100, 61000);
    let r = check_keys(&mac, Some("pool_users"));
    assert_eq!(
        lines(&r, "PASS pool_users: 100 users kbf-pool-0..kbf-pool-99 (uids 61000-61099, group kbfpool 61000)").len(),
        1,
        "{}",
        r.text
    );
}

/// Catches: the commands fed to `dscl` in another form or order than group first and
/// then, for each user, the record and each attribute: the batch text for 3 users.
#[test]
fn apply_feeds_dscl_the_commands_for_a_small_pool() {
    let mac = without_pool("batch-text", Some((3, 800)));
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    let want = format!(
        "create /Groups/kbfpool\ncreate /Groups/kbfpool PrimaryGroupID 800\ncreate /Groups/kbfpool Password *\n{}{}{}",
        make(0, 800),
        make(1, 800),
        make(2, 800)
    );
    assert_eq!(*mac.inputs.borrow(), [want]);
    assert!(
        mac.inputs.borrow()[0].contains(&format!("create {USERS}/kbf-pool-2 UniqueID 802\n")),
        "uid 802"
    );
    assert_eq!(calls(&mac, "dscl . <stdin").len(), 1);
    assert_eq!(check_keys(&mac, Some("pool_users")).failed, 0);
}

/// The number of `dscl` programs `apply` ran for a pool of `n` users from 61000.
fn dscl_runs(n: u32) -> usize {
    let mac = without_pool(&format!("batch-runs-{n}"), Some((n, 61000)));
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(mac.inputs.borrow().len(), 1);
    assert_eq!(
        mac.inputs.borrow()[0].lines().count(),
        3 + 7 * n as usize,
        "one group and {n} users"
    );
    calls(&mac, "dscl").len()
}

/// Catches: a `dscl` run for each user or attribute (26 s for 100 users measured): the
/// default pool of 100 is made by as many `dscl` runs as a pool of 3, and one of them
/// writes.
#[test]
fn the_default_pool_is_made_by_as_many_dscl_runs_as_a_pool_of_three() {
    assert_eq!(dscl_runs(100), dscl_runs(3));
    let mac = without_pool("batch-100", None);
    apply_keys(&mac, Some("pool_users"));
    assert_eq!(
        mac.writes()
            .iter()
            .filter(|w| w.starts_with("dscl"))
            .count(),
        1
    );
    assert_eq!(check_keys(&mac, Some("pool_users")).failed, 0);
    assert_eq!(
        lines(&check_keys(&mac, Some("pool_users")), "PASS").len(),
        1
    );
}

/// Catches: a check that asks `dscl` or `dseditgroup` for each user: for 4 users or
/// 100, it reads `/Users` and `/Groups` once each and runs nothing else.
#[test]
fn the_check_reads_the_directory_twice_whatever_the_pool_size() {
    for (n, first) in [(4, 700), (100, 61000)] {
        let mut mac = FakeMac::converged(&format!("batch-read-{n}"));
        mac.set_pool(Some((n, first)));
        mac.converge_pool(n, first);
        mac.log.borrow_mut().clear();
        let r = check_keys(&mac, Some("pool_users"));
        assert_eq!(r.failed, 0, "{}", r.text);
        assert_eq!(
            *mac.log.borrow(),
            [
                "dscl . -readall /Users GeneratedUID RecordName UniqueID PrimaryGroupID UserShell NFSHomeDirectory IsHidden Password AuthenticationAuthority",
                "dscl . -readall /Groups RecordName PrimaryGroupID GroupMembership GroupMembers"
            ],
            "{n} users"
        );
    }
}

/// Catches: an apply that is not idempotent (a second one feeds `dscl` again or
/// prints), and one that is not resumable: on a pool with two users missing, one with
/// a wrong shell and one with a password hash, it feeds `dscl` those four users' commands
/// and no others.
#[test]
fn a_second_apply_does_nothing_and_a_partial_pool_gets_only_what_is_missing() {
    let mut mac = without_pool("batch-resume", Some((100, 61000)));
    assert_eq!(apply_keys(&mac, Some("pool_users")).failed, 0);
    mac.log.borrow_mut().clear();
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!((r.failed, r.text.as_str()), (0, ""));
    assert_eq!(mac.inputs.borrow().len(), 1, "no second batch");
    assert_eq!(calls(&mac, "dscl . <stdin"), Vec::<String>::new());

    let s = mac.state_mut();
    s.remove_user("kbf-pool-10");
    s.remove_user("kbf-pool-11");
    s.user_attr("kbf-pool-20", "UserShell", "/usr/bin/login");
    s.user_attr("kbf-pool-30", "AuthenticationAuthority", ";ShadowHash;");
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    let want = format!(
        "{}{}create {USERS}/kbf-pool-20 UserShell /usr/bin/false\ndelete {USERS}/kbf-pool-30 AuthenticationAuthority\n",
        make(10, 61000),
        make(11, 61000)
    );
    assert_eq!(mac.inputs.borrow()[1], want);
    assert_eq!(mac.inputs.borrow().len(), 2);
}

/// Catches: a block that overlaps a record outside the pool being made over: a user
/// that is not one of the pool (named like one, but past `pool_users`) holds a uid of
/// the block, and a pool user is also wrong. Nothing is written, not even for the
/// users that are not in the way, and the failure names the user.
#[test]
fn a_block_that_overlaps_a_user_outside_the_pool_is_refused_whole() {
    let mut mac = FakeMac::converged("batch-overlap");
    let s = mac.state_mut();
    s.user("kbf-pool-9", 702);
    s.user_attr("kbf-pool-0", "UserShell", "/usr/bin/login");
    s.remove_user("kbf-pool-3");
    mac.log.borrow_mut().clear();
    let r = check_keys(&mac, Some("pool_users"));
    assert!(
        r.text.contains("kbf-pool-2: uid 702 belongs to kbf-pool-9"),
        "{}",
        r.text
    );
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .contains("NOTE pool_users: uid 702 belongs to kbf-pool-9: no pool user changed"),
        "{}",
        r.text
    );
    assert!(
        r.text
            .contains("kbf-pool-0: UserShell=/usr/bin/login, want /usr/bin/false"),
        "{}",
        r.text
    );
    assert!(mac.inputs.borrow().is_empty());
    assert!(mac.writes().is_empty(), "{:?}", mac.writes());
}

/// Catches: a `dscl` that fails or reads nothing being counted as having made the
/// users: the item still fails and names the user that is missing.
#[test]
fn a_batch_that_dscl_refuses_leaves_the_item_failing() {
    let mut mac = FakeMac::converged("batch-refused");
    mac.state_mut().remove_user("kbf-pool-3");
    mac.state_mut().stdin_fails = true;
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .contains("FAIL pool_users: kbf-pool-3: UniqueID=absent, want 703"),
        "{}",
        r.text
    );
    assert_eq!(mac.inputs.borrow().len(), 1);
}

/// Catches: a directory that could not be read being taken for an empty one, so that
/// the overlap guard passes and a batch is written over a user outside the pool. With
/// `dscl -readall` failing, or printing nothing, `check` fails saying so, and `apply`
/// writes nothing and fails, though a user outside the pool holds a pool uid.
#[test]
fn a_directory_that_cannot_be_read_is_not_an_empty_one() {
    for (fails, empty) in [(true, false), (false, true)] {
        let mut mac = FakeMac::converged(&format!("batch-unread-{fails}"));
        let s = mac.state_mut();
        s.user("kbf-pool-9", 702);
        s.remove_user("kbf-pool-3");
        s.readall_fails = fails;
        s.readall_empty = empty;
        mac.log.borrow_mut().clear();
        let r = check_keys(&mac, Some("pool_users"));
        assert_eq!(r.failed, 1, "{}", r.text);
        assert!(
            r.text
                .contains("FAIL pool_users: could not read the directory: "),
            "{}",
            r.text
        );
        let r = apply_keys(&mac, Some("pool_users"));
        assert_eq!(r.failed, 1, "{}", r.text);
        assert!(
            r.text.contains("could not read the directory: "),
            "{}",
            r.text
        );
        assert!(mac.inputs.borrow().is_empty());
        assert!(mac.writes().is_empty(), "{:?}", mac.writes());
    }
}

/// Catches: a gid held by another group not stopping `apply`, and not named by
/// `check`: nothing is written, not even for a user that is missing.
#[test]
fn a_gid_held_by_another_group_refuses_the_block_whole() {
    let mut mac = FakeMac::converged("batch-gid");
    let s = mac.state_mut();
    s.attr("/Groups/staff2", "PrimaryGroupID", "700");
    s.remove_user("kbf-pool-3");
    mac.log.borrow_mut().clear();
    let r = check_keys(&mac, Some("pool_users"));
    assert!(r.text.contains("gid 700 belongs to staff2"), "{}", r.text);
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .contains("NOTE pool_users: gid 700 belongs to staff2: no pool user changed"),
        "{}",
        r.text
    );
    assert!(mac.inputs.borrow().is_empty());
    assert!(mac.writes().is_empty(), "{:?}", mac.writes());
}

/// Runs `apply` on the pool with `refuse` done to a converged Mac that also lacks
/// `kbf-pool-3` (so that `apply` would write if it went on), and requires it to fail
/// with a note containing `why` and to write nothing at all.
fn refused(name: &str, refuse: impl FnOnce(&mut support::State), why: &str) {
    let mut mac = FakeMac::converged(name);
    let s = mac.state_mut();
    s.remove_user("kbf-pool-3");
    refuse(s);
    mac.log.borrow_mut().clear();
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(r.text.contains(why), "want {why:?} in {}", r.text);
    assert!(r.text.contains("no pool user changed"), "{}", r.text);
    assert!(mac.inputs.borrow().is_empty());
    assert!(mac.writes().is_empty(), "{:?}", mac.writes());
}

/// Catches: `apply` rewriting an existing account that has a pool user's name but a
/// uid outside the pool's block (someone's account, handed to the pool): nothing is
/// written, not even for the pool user that is missing, and the note names the record.
#[test]
fn a_user_named_like_a_pool_user_with_a_uid_outside_the_block_is_refused() {
    refused(
        "batch-foreign-uid",
        |s| s.user_attr("kbf-pool-1", "UniqueID", "501"),
        "user kbf-pool-1 has uid 501, outside the pool's block 700-703",
    );
}

/// Catches: `apply` rewriting a `kbfpool` group whose gid is not the pool's.
#[test]
fn a_kbfpool_group_with_another_gid_is_refused() {
    refused(
        "batch-foreign-gid",
        |s| s.attr("/Groups/kbfpool", "PrimaryGroupID", "20"),
        "group kbfpool has gid 20, not the pool's 700",
    );
}

/// Catches: a refusal that also stops the finish of a half-run: the group and a user
/// were created, their ids not yet set, and a user's uid is in the block with its
/// other attributes wrong. `apply` sets what is missing and `check` then passes.
#[test]
fn a_half_made_pool_with_our_names_is_finished_not_refused() {
    let mut mac = FakeMac::converged("batch-half-names");
    let s = mac.state_mut();
    s.records.remove("/Groups/kbfpool");
    s.attr("/Groups/kbfpool", "Password", "*");
    s.records.remove("/Users/kbf-pool-3");
    s.user_attr("kbf-pool-3", "UserShell", "/usr/bin/false");
    s.user_attr("kbf-pool-1", "UserShell", "/bin/zsh");
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert!(!r.text.contains("no pool user changed"), "{}", r.text);
    assert_eq!(mac.inputs.borrow().len(), 1);
    assert_eq!(check_keys(&mac, Some("pool_users")).failed, 0);
}

/// Catches: only the first name of a record being compared with the pool's names: a
/// user whose second name is `kbf-pool-3`, and a group whose second name is `kbfpool`,
/// each make `check` fail and `apply` write nothing. The directory read has the record
/// names on one line, as `dscl -readall` prints them.
#[test]
fn a_record_with_a_pool_name_as_another_name_is_refused() {
    let mut mac = FakeMac::converged("batch-alias-user");
    let s = mac.state_mut();
    s.remove_user("kbf-pool-3");
    s.user("someone", 5000);
    s.user_attr("someone", "RecordName", "kbf-pool-3");
    let r = check_keys(&mac, Some("pool_users"));
    assert!(
        r.text.contains("kbf-pool-3: also a name of user someone"),
        "{}",
        r.text
    );
    refused(
        "batch-alias-user-apply",
        |s| {
            s.user("someone", 5000);
            s.user_attr("someone", "RecordName", "kbf-pool-3");
        },
        "user someone is also named kbf-pool-3",
    );
    refused(
        "batch-alias-group",
        |s| {
            s.attr("/Groups/staff2", "PrimaryGroupID", "5000");
            s.attr("/Groups/staff2", "RecordName", "kbfpool");
        },
        "group staff2 is also named kbfpool",
    );
}

/// Catches: an administrator listed in `admin` only by `GeneratedUID` (in
/// `GroupMembers`, not by name) passing the check, and `apply` not removing it.
#[test]
fn a_member_listed_by_generated_uid_is_seen_and_removed() {
    let mut mac = FakeMac::converged("batch-guid");
    let s = mac.state_mut();
    s.user_attr("kbf-pool-1", "GeneratedUID", "ABCD-1234");
    s.user_attr("kbf-pool-2", "GeneratedUID", "EFGH-5678");
    s.attr("/Groups/admin", "GroupMembers", "ABCD-1234");
    s.attr("/Groups/wheel", "GroupMembers", "EFGH-5678");
    let r = check_keys(&mac, Some("pool_users"));
    assert!(
        r.text.contains("kbf-pool-1: an administrator"),
        "{}",
        r.text
    );
    assert!(r.text.contains("kbf-pool-2: in wheel"), "{}", r.text);
    assert!(!r.text.contains("kbf-pool-0:"), "{}", r.text);
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        calls(&mac, "dseditgroup"),
        [
            "dseditgroup -o edit -d kbf-pool-1 -t user admin",
            "dseditgroup -o edit -d kbf-pool-2 -t user wheel"
        ]
    );
    assert_eq!(check_keys(&mac, Some("pool_users")).failed, 0);
}

/// Catches: a batch in which `dscl` refuses some commands being counted as made, and
/// a resumed `apply` that redoes more than what is left: two users missing and a
/// refusal of one shell, the check names exactly the refused value, and the next
/// `apply` feeds only that command.
#[test]
fn a_batch_dscl_half_runs_is_finished_by_the_next_apply() {
    let mut mac = FakeMac::converged("batch-half");
    let s = mac.state_mut();
    s.remove_user("kbf-pool-1");
    s.remove_user("kbf-pool-2");
    s.stdin_rejects = vec!["kbf-pool-1 UserShell".to_owned()];
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(
        r.text
            .contains("FAIL pool_users: kbf-pool-1: UserShell=absent, want /usr/bin/false"),
        "{}",
        r.text
    );
    assert!(!r.text.contains("kbf-pool-2:"), "{}", r.text);
    mac.state_mut().stdin_rejects.clear();
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 0, "{}", r.text);
    assert_eq!(
        mac.inputs.borrow()[1],
        format!("create {USERS}/kbf-pool-1 UserShell /usr/bin/false\n")
    );
}

/// Catches: a home that cannot be made stopping the batch, so that the users after it
/// are not made: the home of `kbf-pool-1` is the wrong owner and cannot be made again,
/// and `kbf-pool-3` is missing; `apply` fails on the home and still makes the user.
#[test]
fn a_home_that_cannot_be_made_does_not_stop_the_users_after_it() {
    let mut mac = FakeMac::converged("batch-home");
    let home = "/Volumes/kbf/home/kbf-pool-1";
    let s = mac.state_mut();
    s.owners.insert(home.to_owned(), (0, 0));
    s.mkdir_fails.push(home.to_owned());
    s.remove_user("kbf-pool-3");
    let r = apply_keys(&mac, Some("pool_users"));
    assert_eq!(r.failed, 1, "{}", r.text);
    assert!(r.text.contains("cannot make it"), "{}", r.text);
    assert_eq!(mac.inputs.borrow().len(), 1, "{}", r.text);
    assert!(
        mac.inputs.borrow()[0].starts_with(&format!("create {USERS}/kbf-pool-3\n")),
        "{:?}",
        mac.inputs.borrow()
    );
}
