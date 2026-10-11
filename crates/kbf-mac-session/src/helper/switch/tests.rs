use crate::autologin::{CommandOutput, kcpassword, read_intent};
use crate::helper::tests::{Rig, fds, log, me, rig, rig_with};
use crate::helper::{Outcome, Reset};
use crate::lease::UidRange;
use crate::proto::{Reply, Request};

use kbf_types::LeaseId;

const RESTART: &str = "/bin/launchctl reboot userspace";

/// A rig whose fake host logs what the intent record holds at every command.
fn switch_rig(name: &str) -> Rig {
    let rig = rig(name, 4);
    rig.host.state().intent = Some(rig.dir.join("session-intent"));
    rig
}

fn intent(rig: &Rig) -> Option<LeaseId> {
    read_intent(&rig.dir.join("session-intent")).unwrap()
}

fn domain(rig: &Rig) -> String {
    rig.dir.join("loginwindow").display().to_string()
}

fn position(log: &[String], prefix: &str) -> usize {
    log.iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} in {log:#?}"))
}

/// Catches: `session-login` for a lease that has no user (or whose user was deleted)
/// setting auto-login to a name no account has, or to a user the ledger does not hold.
#[test]
fn session_login_needs_a_live_lease_user() {
    let rig = switch_rig("login-nouser");
    let error = rig.helper.session_login("1.1").unwrap_err();
    assert!(error.contains("lease 1.1 has no user"), "{error}");
    rig.helper.user_create("1.2", None).unwrap();
    rig.helper.user_delete("1.2").unwrap();
    let error = rig.helper.session_login("1.2").unwrap_err();
    assert!(error.contains("was deleted"), "{error}");
    assert!(rig.helper.session_login("1.x").is_err());
    let calls = log(&rig);
    assert!(
        calls.iter().all(|call| !call.starts_with("command")),
        "{calls:#?}"
    );
    assert_eq!(intent(&rig), None);
    assert!(!rig.dir.join("kcpassword").exists());
}

/// Catches: the restart asked for before the intent record is written (a power cut
/// between them boots into a lease user's session that no record names, so the boot
/// reset and `kbf-mac-provision check` cannot tell it from a leftover), auto-login set
/// before the record, or a `/etc/kcpassword` that does not hold the password the user
/// was just given.
#[test]
fn session_login_writes_the_intent_before_anything_and_restarts_last() {
    let rig = switch_rig("login-order");
    rig.helper.user_create("1.1", None).unwrap();
    log(&rig);
    rig.helper.session_login("1.1").unwrap();
    let calls = log(&rig);
    let domain = domain(&rig);
    assert_eq!(
        calls,
        [
            "command /usr/bin/fdesetup isactive (no intent)".to_owned(),
            "reset_password kbf-lease-1-1".to_owned(),
            format!(
                "command /usr/bin/defaults write {domain} autoLoginUser -string kbf-lease-1-1 (intent 1.1)"
            ),
            format!("command {RESTART} (intent 1.1)"),
        ]
    );
    assert_eq!(intent(&rig), Some(LeaseId::new(1, 1)));
    assert_eq!(
        std::fs::read(rig.dir.join("kcpassword")).unwrap(),
        kcpassword(b"pw-kbf-lease-1-1")
    );
    // Repeated for the same lease (a retry after a lost reply): the same switch.
    rig.helper.session_login("1.1").unwrap();
    assert!(
        log(&rig)
            .last()
            .unwrap()
            .starts_with(&format!("command {RESTART}"))
    );
}

/// Catches: a second lease taking the session from the first (the first's user would
/// stay logged in with no record of it), or a switch with FileVault on, which writes
/// nothing.
#[test]
fn session_login_is_refused_for_a_second_lease_and_with_filevault_on() {
    let rig = switch_rig("login-refused");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.user_create("1.2", None).unwrap();
    rig.host.state().answers.insert(
        "/usr/bin/fdesetup isactive".to_owned(),
        CommandOutput {
            success: true,
            stdout: "true\n".to_owned(),
            stderr: String::new(),
        },
    );
    let error = rig.helper.session_login("1.1").unwrap_err();
    assert!(error.contains("FileVault is on"), "{error}");
    assert_eq!(intent(&rig), None);
    assert!(!rig.dir.join("kcpassword").exists());
    rig.host.state().answers.clear();
    rig.helper.session_login("1.1").unwrap();
    log(&rig);
    let error = rig.helper.session_login("1.2").unwrap_err();
    assert!(error.contains("belongs to lease 1.1"), "{error}");
    assert_eq!(log(&rig), Vec::<String>::new());
    // The session's user cannot be deleted from under the login window.
    rig.helper.kill_uid("1.1").unwrap();
    let error = rig.helper.user_delete("1.1").unwrap_err();
    assert!(error.contains("holds the GUI session"), "{error}");
    assert!(rig.host.state().users.contains_key("kbf-lease-1-1"));
}

/// Catches: `session-idle` that leaves `/etc/kcpassword` (the lease user's password,
/// readable by anyone who can read the file), leaves `autoLoginUser`, removes the
/// intent record before auto-login is cleared, or restarts before clearing.
#[test]
fn session_idle_clears_auto_login_and_kcpassword_then_restarts() {
    let rig = switch_rig("idle");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.session_login("1.1").unwrap();
    log(&rig);
    rig.helper.session_idle().unwrap();
    let domain = domain(&rig);
    assert_eq!(
        log(&rig),
        [
            format!("command /usr/bin/defaults delete {domain} autoLoginUser (intent 1.1)"),
            format!("command {RESTART} (no intent)"),
        ]
    );
    assert!(!rig.dir.join("kcpassword").exists(), "kcpassword left");
    assert_eq!(intent(&rig), None);
    // Idle again: nothing to clear, and still the restart.
    rig.helper.session_idle().unwrap();
    assert!(
        log(&rig)
            .last()
            .unwrap()
            .starts_with(&format!("command {RESTART}"))
    );
    // Now the user may go.
    rig.helper.kill_uid("1.1").unwrap();
    assert_eq!(rig.helper.user_delete("1.1"), Ok(true));
}

/// Catches: a failed restart reported as done, or a switch whose clearing failed going
/// on to restart.
#[test]
fn a_failed_switch_is_reported_and_does_not_restart() {
    let rig = switch_rig("idle-fail");
    let domain = domain(&rig);
    let answer = |success| CommandOutput {
        success,
        stdout: String::new(),
        stderr: "not now".to_owned(),
    };
    // The delete fails and the key is still there.
    let mut state = rig.host.state();
    let delete = format!("/usr/bin/defaults delete {domain} autoLoginUser");
    state.answers.insert(delete, answer(false));
    let read = format!("/usr/bin/defaults read {domain} autoLoginUser");
    state.answers.insert(read, answer(true));
    drop(state);
    assert!(rig.helper.session_idle().is_err());
    assert!(log(&rig).iter().all(|call| !call.contains(RESTART)));
    rig.host.state().answers.clear();
    rig.host
        .state()
        .answers
        .insert(RESTART.to_owned(), answer(false));
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("not now"), "{error}");
}

/// A helper restarted over `rig`'s state directory (a boot), its reset not yet run.
fn rebooted(rig: &Rig, name: &str) -> Rig {
    let range = UidRange::new(me(), me() + 3).unwrap();
    let booted = rig_with(name, range, Some(rig.dir.clone()));
    booted.host.state().users = rig.host.state().users.clone();
    booted.host.state().intent = Some(rig.dir.join("session-intent"));
    booted
}

/// Catches: a boot reset that keeps auto-login for a lease the server no longer holds
/// (the Mac stays logged in as a dead lease user), skips deleting the leftover users,
/// or leaves `/etc/kcpassword` or the record behind.
#[test]
fn the_boot_reset_clears_a_stale_intent_and_deletes_its_user() {
    let rig = switch_rig("reset-stale");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.user_create("1.2", None).unwrap();
    rig.helper.session_login("1.1").unwrap();
    // Power is lost; at boot the lease user is logged in again by the login window.
    let booted = rebooted(&rig, "reset-stale");
    booted.host.state().procs.insert(me(), 3);
    let reset = booted.helper.boot_reset(&["7.7".to_owned()]).unwrap();
    assert_eq!(
        reset,
        Reset {
            kept: None,
            deleted: vec![LeaseId::new(1, 1), LeaseId::new(1, 2)],
        }
    );
    let calls = log(&booted);
    let domain = domain(&booted);
    let cleared = position(
        &calls,
        &format!("command /usr/bin/defaults delete {domain}"),
    );
    let killed = position(&calls, &format!("bootout gui/{}", me()));
    let deleted = position(&calls, "delete_user kbf-lease-1-1");
    assert!(cleared < killed && killed < deleted, "{calls:#?}");
    assert!(calls.contains(&"delete_user kbf-lease-1-2".to_owned()));
    assert!(booted.host.state().users.is_empty());
    assert!(!booted.dir.join("kcpassword").exists());
    assert_eq!(intent(&booted), None);
    assert!(
        calls.iter().all(|call| !call.contains(RESTART)),
        "{calls:#?}"
    );
}

/// Catches: a boot reset that clears the session of a lease the server still holds
/// (the GUI test it was prepared for would find no session), deletes its user, or
/// keeps other leftover users with it.
#[test]
fn the_boot_reset_keeps_a_live_intent() {
    let rig = switch_rig("reset-live");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.user_create("1.2", None).unwrap();
    rig.helper.session_login("1.1").unwrap();
    let before = std::fs::read(rig.dir.join("kcpassword")).unwrap();
    let booted = rebooted(&rig, "reset-live");
    let held = ["1.1".to_owned(), "1.2".to_owned()];
    let reset = booted.helper.boot_reset(&held).unwrap();
    assert_eq!(
        reset,
        Reset {
            kept: Some(LeaseId::new(1, 1)),
            deleted: vec![LeaseId::new(1, 2)],
        }
    );
    let calls = log(&booted);
    assert!(
        calls.iter().all(|call| !call.contains("defaults")),
        "{calls:#?}"
    );
    assert!(calls.iter().all(|call| call != "delete_user kbf-lease-1-1"));
    assert!(booted.host.state().users.contains_key("kbf-lease-1-1"));
    assert!(!booted.host.state().users.contains_key("kbf-lease-1-2"));
    assert_eq!(
        std::fs::read(booted.dir.join("kcpassword")).unwrap(),
        before
    );
    assert_eq!(intent(&booted), Some(LeaseId::new(1, 1)));
}

/// Catches: an unreadable record, or one naming a lease whose user is gone, taken as
/// live; or a bad held lease id accepted.
#[test]
fn the_boot_reset_treats_a_broken_record_as_stale() {
    let rig = switch_rig("reset-broken");
    rig.helper.user_create("1.1", None).unwrap();
    std::fs::write(rig.dir.join("session-intent"), "garbage").unwrap();
    std::fs::write(rig.dir.join("kcpassword"), "x").unwrap();
    let booted = rebooted(&rig, "reset-broken");
    assert!(booted.helper.boot_reset(&["1.01".to_owned()]).is_err());
    let reset = booted.helper.boot_reset(&["1.1".to_owned()]).unwrap();
    assert_eq!(reset.kept, None);
    assert!(!booted.dir.join("kcpassword").exists());
    assert!(!booted.dir.join("session-intent").exists());

    // A record naming a held lease whose user was deleted.
    let rig = switch_rig("reset-gone");
    std::fs::write(rig.dir.join("session-intent"), "4.4\n").unwrap();
    let booted = rebooted(&rig, "reset-gone");
    let reset = booted.helper.boot_reset(&["4.4".to_owned()]).unwrap();
    assert_eq!(reset.kept, None);
    assert_eq!(intent(&booted), None);
}

/// Catches: a boot reset that fails part-way yet lets the other verbs in, or one that
/// can run twice (a second one would delete the users of leases started since).
#[test]
fn the_boot_reset_runs_once_and_must_succeed() {
    let rig = switch_rig("reset-once");
    rig.helper.user_create("1.1", None).unwrap();
    let booted = rebooted(&rig, "reset-once");
    booted.host.state().stubborn.insert(me(), 100);
    booted.host.state().procs.insert(me(), 1);
    let error = booted.helper.boot_reset(&[]).unwrap_err();
    assert!(error.contains("lease 1.1:"), "{error}");
    assert!(!booted.helper.reset_done());
    booted.host.state().stubborn.clear();
    booted.helper.boot_reset(&[]).unwrap();
    assert!(booted.helper.reset_done());
    let again = booted.helper.boot_reset(&[]).unwrap_err();
    assert!(again.contains("runs once"), "{again}");
}

/// Catches: a verb served before the boot reset (the daemon could start a lease on a
/// Mac still logged in as a dead lease user), or the new verbs routed wrongly.
#[test]
fn nothing_but_the_boot_reset_is_served_before_it() {
    let rig = switch_rig("gate");
    let booted = rebooted(&rig, "gate");
    let reply = |outcome: Outcome| match outcome {
        Outcome::Reply(reply) => reply,
        Outcome::Running(child) => panic!("a process started: {child:?}"),
    };
    let lease = || "2.1".to_owned();
    let create = Request::UserCreate {
        lease: lease(),
        grant: None,
    };
    for request in [
        create.clone(),
        Request::KillUid { lease: lease() },
        Request::UserDelete { lease: lease() },
        Request::SessionLogin { lease: lease() },
        Request::SessionIdle,
    ] {
        let refused = reply(booted.helper.handle(request.clone(), Vec::new()));
        assert_eq!(
            refused,
            Reply::Refused {
                reason: "the boot reset has not run since the helper started".to_owned()
            },
            "{request:?}"
        );
    }
    let run = Request::Run {
        lease: lease(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: Vec::new(),
    };
    assert!(matches!(
        booted.helper.handle(run, fds(&booted.dir)),
        Outcome::Reply(Reply::Refused { .. })
    ));
    assert!(booted.host.state().users.is_empty());
    let reset = Request::BootReset { held: Vec::new() };
    assert_eq!(
        reply(booted.helper.handle(reset, Vec::new())),
        Reply::Reset {
            kept: None,
            deleted: Vec::new()
        }
    );
    assert_eq!(
        reply(booted.helper.handle(create, Vec::new())),
        Reply::Created { uid: me() }
    );
    let login = Request::SessionLogin { lease: lease() };
    assert_eq!(
        reply(booted.helper.handle(login, Vec::new())),
        Reply::Restarting
    );
    assert_eq!(
        reply(booted.helper.handle(Request::SessionIdle, Vec::new())),
        Reply::Restarting
    );
    let booted_again = rebooted(&booted, "gate");
    let held = Request::BootReset {
        held: vec![lease()],
    };
    assert_eq!(
        reply(booted_again.helper.handle(held, Vec::new())),
        Reply::Reset {
            kept: None,
            deleted: vec![lease()]
        }
    );
}

/// Catches: a `session-login` that goes on to set auto-login (or restart) after the
/// intent record, the password or `/etc/kcpassword` could not be written, or a
/// `session-idle` that restarts although the record could not be removed.
#[test]
fn a_switch_stops_at_the_first_write_that_fails() {
    let rig = switch_rig("write-fail");
    rig.helper.user_create("1.1", None).unwrap();
    let blocker = |name: &str| std::fs::create_dir_all(rig.dir.join(name).join("inside")).unwrap();
    let unblock = |name: &str| std::fs::remove_dir_all(rig.dir.join(name)).unwrap();
    let no_defaults = |rig: &Rig| {
        let calls = log(rig);
        assert!(
            calls
                .iter()
                .all(|call| !call.contains("defaults") && !call.contains(RESTART)),
            "{calls:#?}"
        );
    };
    blocker("session-intent.new");
    let error = rig.helper.session_login("1.1").unwrap_err();
    assert!(error.contains("writing"), "{error}");
    no_defaults(&rig);
    unblock("session-intent.new");
    rig.host.state().fail.insert("reset_password");
    let error = rig.helper.session_login("1.1").unwrap_err();
    assert!(
        error.contains("setting the password of kbf-lease-1-1"),
        "{error}"
    );
    no_defaults(&rig);
    rig.host.state().fail.clear();
    blocker("kcpassword.new");
    let error = rig.helper.session_login("1.1").unwrap_err();
    assert!(error.contains("kcpassword"), "{error}");
    no_defaults(&rig);
    unblock("kcpassword.new");
    // The record cannot be removed: no restart.
    std::fs::remove_file(rig.dir.join("session-intent")).unwrap();
    blocker("session-intent");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("removing"), "{error}");
    assert!(log(&rig).iter().all(|call| !call.contains(RESTART)));
}
