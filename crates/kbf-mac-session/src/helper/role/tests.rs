//! The VM user's verbs, and the session switch and boot reset on a Mac that has one.

use crate::autologin::{CommandOutput, Intent, kcpassword, read_intent};
use crate::helper::tests::{Rig, fds, log, me, my_gid, rig_vm_with};
use crate::helper::{NewUser, Outcome, Reset};
use crate::lease::UidRange;
use crate::proto::{Reply, Request};

use kbf_types::LeaseId;

const RESTART: &str = "/bin/launchctl reboot userspace";
const VM: &str = "kbf-vm";

/// The VM user's uid: an ordinary one above the rig's lease range (`me()` to `me()+3`).
fn vm_uid() -> u32 {
    me() + 10
}

fn range() -> UidRange {
    UidRange::new(me(), me() + 3).unwrap()
}

fn add_account(rig: &Rig, uid: u32, gid: u32) {
    rig.host.state().users.insert(
        VM.to_owned(),
        NewUser {
            name: VM.to_owned(),
            uid,
            gid,
            home: rig.dir.join("Users").join(VM),
            admin: false,
        },
    );
}

/// A rig on a Mac with the VM user `kbf-vm` and its account, its boot reset done, the
/// fake host logging the intent record at every command.
fn vm_rig(name: &str) -> Rig {
    let rig = rig_vm_with(name, range(), None, Some(VM));
    rig.helper.boot_reset(&[]).unwrap();
    rig.host.state().intent = Some(rig.dir.join("session-intent"));
    add_account(&rig, vm_uid(), my_gid());
    log(&rig);
    rig
}

/// A helper restarted over `rig`'s state (a boot), its reset not yet run; `vm` is its
/// VM user.
fn rebooted(rig: &Rig, name: &str, vm: Option<&str>) -> Rig {
    let booted = rig_vm_with(name, range(), Some(rig.dir.clone()), vm);
    booted.host.state().users = rig.host.state().users.clone();
    booted.host.state().answers = rig.host.state().answers.clone();
    booted.host.state().intent = Some(rig.dir.join("session-intent"));
    booted
}

fn intent(rig: &Rig) -> Option<Intent> {
    read_intent(&rig.dir.join("session-intent")).unwrap()
}

fn domain(rig: &Rig) -> String {
    rig.dir.join("loginwindow").display().to_string()
}

fn answer(rig: &Rig, command: String, success: bool, stdout: &str, stderr: &str) {
    rig.host.state().answers.insert(
        command,
        CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        },
    );
}

/// What the login window's `autoLoginUser` reads as.
fn auto_login_is(rig: &Rig, user: &str) {
    let read = format!("/usr/bin/defaults read {} autoLoginUser", domain(rig));
    answer(rig, read, true, &format!("{user}\n"), "");
}

fn argv() -> Vec<String> {
    vec!["/usr/bin/true".to_owned()]
}

fn gui_up(rig: &Rig) {
    rig.host.state().domains.insert(format!("gui/{}", vm_uid()));
}

fn no_calls_to_act(calls: &[String]) {
    assert!(
        calls
            .iter()
            .all(|call| !call.starts_with("kill_all") && !call.starts_with("bootout")),
        "{calls:#?}"
    );
}

/// Catches: a role verb on a Mac with no VM user acting on some account anyway (there
/// is nothing to name it by), or asking the directory or launchd first.
#[test]
fn the_role_verbs_need_a_configured_vm_user() {
    let rig = rig_vm_with("role-none", range(), None, None);
    rig.helper.boot_reset(&[]).unwrap();
    log(&rig);
    let none = "no VM user is configured";
    assert!(rig.helper.role_session_ready().unwrap_err().contains(none));
    assert!(rig.helper.role_kill_uid().unwrap_err().contains(none));
    let run = rig.helper.role_run(argv(), Vec::new(), fds(&rig.dir));
    assert!(run.unwrap_err().contains(none));
    assert_eq!(log(&rig), Vec::<String>::new());
}

/// Catches: a role verb acting on an account that is missing, is root, a system
/// account, a lease user's uid (a kill would end a lease's processes), or has gid 0; or
/// a directory failure taken as "no account".
#[test]
fn the_role_verbs_refuse_an_account_they_must_not_act_on() {
    let rig = vm_rig("role-account");
    rig.host.state().users.clear();
    let error = rig.helper.role_session_ready().unwrap_err();
    assert!(error.contains("kbf-vm has no account"), "{error}");
    for (uid, gid) in [
        (0, my_gid()),
        (1, my_gid()),
        (499, my_gid()),
        (me(), my_gid()),
        (me() + 3, my_gid()),
        (60_001, my_gid()),
        (vm_uid(), 0),
    ] {
        add_account(&rig, uid, gid);
        for error in [
            rig.helper.role_session_ready().unwrap_err(),
            rig.helper.role_kill_uid().unwrap_err(),
            rig.helper
                .role_run(argv(), Vec::new(), fds(&rig.dir))
                .unwrap_err(),
        ] {
            assert!(
                error.contains(&format!("uid {uid} and gid {gid}")),
                "{uid}:{gid}: {error}"
            );
        }
    }
    no_calls_to_act(&log(&rig));
    rig.host.state().fail.insert("account");
    let error = rig.helper.role_session_ready().unwrap_err();
    assert!(error.contains("looking up the VM user kbf-vm"), "{error}");
    // The edges of the ordinary uids are accepted.
    rig.host.state().fail.clear();
    for uid in [500, 60_000, me() + 4] {
        add_account(&rig, uid, my_gid());
        assert_eq!(rig.helper.role_session_ready(), Ok(false), "{uid}");
    }
}

/// Catches: a readiness answer that does not follow the VM user's `gui/<uid>` domain,
/// that is "ready" while a lease holds the session (a VM would start in a lease user's
/// login), or that stops being an answer when the session switch has never run.
#[test]
fn the_role_session_is_ready_when_its_gui_domain_is_up_and_no_lease_holds_it() {
    let rig = vm_rig("role-ready");
    // No record yet (provisioning set auto-login): the domain decides.
    assert_eq!(rig.helper.role_session_ready(), Ok(false));
    assert_eq!(
        log(&rig),
        [
            "account kbf-vm".to_owned(),
            format!("domain_exists gui/{}", vm_uid())
        ]
    );
    gui_up(&rig);
    assert_eq!(rig.helper.role_session_ready(), Ok(true));
    // A lease user's gui domain is not the VM user's.
    rig.helper.user_create("1.1", None).unwrap();
    rig.host.state().domains.insert(format!("gui/{}", me()));
    rig.helper.session_login("1.1").unwrap();
    log(&rig);
    assert_eq!(rig.helper.role_session_ready(), Ok(false));
    assert_eq!(log(&rig), ["account kbf-vm".to_owned()]);
    // Rested at the VM user.
    rig.helper.session_idle().unwrap();
    assert_eq!(rig.helper.role_session_ready(), Ok(true));
    rig.host.state().domains.clear();
    assert_eq!(rig.helper.role_session_ready(), Ok(false));
    // launchd and the record failing are errors, not "not ready".
    rig.host.state().fail.insert("domain_exists");
    let error = rig.helper.role_session_ready().unwrap_err();
    assert!(error.contains("asking launchd for gui/"), "{error}");
    rig.host.state().fail.clear();
    std::fs::write(rig.dir.join("session-intent"), "garbage").unwrap();
    assert!(rig.helper.role_session_ready().is_err());
}

/// Catches: a role run that starts before the VM user's `gui/<uid>` exists (outside the
/// session, with no window server), in a session a lease holds, or with the wrong
/// descriptors or an empty argument vector; and requests routed to the wrong verb.
#[test]
fn a_role_run_waits_for_the_vm_user_s_session() {
    let rig = vm_rig("role-run");
    let uid = vm_uid();
    let refused = |rig: &Rig| {
        rig.helper
            .role_run(argv(), Vec::new(), fds(&rig.dir))
            .unwrap_err()
    };
    let error = refused(&rig);
    assert!(
        error.starts_with(&format!(
            "gui/{uid} does not exist: the login of the VM user kbf-vm"
        )),
        "{error}"
    );
    // Malformed requests are refused before anything is asked.
    log(&rig);
    let few = rig.helper.role_run(argv(), Vec::new(), Vec::new());
    assert!(few.unwrap_err().contains("got 0 descriptors"));
    let empty = rig.helper.role_run(Vec::new(), Vec::new(), fds(&rig.dir));
    assert!(empty.unwrap_err().contains("empty"));
    assert_eq!(log(&rig), Vec::<String>::new());

    // A lease holds the session.
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.session_login("1.1").unwrap();
    gui_up(&rig);
    let error = refused(&rig);
    assert!(
        error.contains("lease 1.1 holds the GUI session; session-idle first"),
        "{error}"
    );

    // Rested at the VM user with its session up: past every check. Off macOS there is
    // nothing to start in it.
    rig.helper.session_idle().unwrap();
    #[cfg(not(target_os = "macos"))]
    {
        let error = refused(&rig);
        assert!(
            error.starts_with("starting /usr/bin/true: a GUI session exists only on macOS"),
            "{error}"
        );
    }

    // The wire: descriptors belong to run and role-run only.
    let reply =
        |request: Request, fds: Vec<std::os::fd::OwnedFd>| match rig.helper.handle(request, fds) {
            Outcome::Reply(reply) => reply,
            Outcome::Running(child) => panic!("a process started: {child:?}"),
        };
    assert_eq!(
        reply(Request::RoleSessionReady, Vec::new()),
        Reply::SessionReady { ready: true }
    );
    assert_eq!(
        reply(Request::RoleSessionReady, fds(&rig.dir)),
        Reply::Refused {
            reason: "only run and role-run take descriptors".to_owned()
        }
    );
    rig.host.state().domains.clear();
    let run = Request::RoleRun {
        argv: argv(),
        env: Vec::new(),
    };
    let reply = reply(run, fds(&rig.dir));
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.contains("does not exist")),
        "{reply:?}"
    );
}

/// Catches: a role kill that leaves the VM user's processes, that boots out the wrong
/// domains, that kills a lease user's processes with them, or that reports success when
/// processes survive.
#[test]
fn role_kill_uid_ends_the_vm_user_s_processes_and_no_one_else_s() {
    let rig = vm_rig("role-kill");
    let uid = vm_uid();
    rig.helper.user_create("1.1", None).unwrap();
    {
        let mut state = rig.host.state();
        state.procs.insert(uid, 5);
        state.procs.insert(me(), 2);
    }
    log(&rig);
    rig.helper.role_kill_uid().unwrap();
    let calls = log(&rig);
    let at = |prefix: &str| {
        calls
            .iter()
            .position(|call| call.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {calls:#?}"))
    };
    assert!(
        at(&format!("bootout gui/{uid}")) < at(&format!("kill_all {uid}")),
        "{calls:#?}"
    );
    assert!(at(&format!("bootout user/{uid}")) < at(&format!("kill_all {uid}")));
    assert!(
        calls
            .iter()
            .all(|call| call != &format!("kill_all {}", me()))
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.contains(&format!("/{}", me())))
    );
    assert!(!rig.host.state().procs.contains_key(&uid));
    assert_eq!(rig.host.state().procs[&me()], 2);

    rig.host.state().procs.insert(uid, 1);
    rig.host.state().stubborn.insert(uid, 100);
    let error = rig.helper.role_kill_uid().unwrap_err();
    assert!(
        error.contains(&format!("processes of uid {uid} remain")),
        "{error}"
    );
    // Over the wire.
    rig.host.state().stubborn.clear();
    let reply = rig.helper.handle(Request::RoleKillUid, Vec::new());
    assert!(matches!(reply, Outcome::Reply(Reply::Killed)), "{reply:?}");
}

/// Catches: `session-idle` on a Mac with a VM user that clears auto-login (the Mac
/// would rest at the login window and VMs could not start), writes the record after the
/// password, sets auto-login before the record, restarts first, or writes a
/// `/etc/kcpassword` that is not the VM user's fresh password.
#[test]
fn session_idle_rests_at_the_vm_user_in_the_order_of_session_login() {
    let rig = vm_rig("idle-vm");
    rig.helper.session_idle().unwrap();
    let domain = domain(&rig);
    assert_eq!(
        log(&rig),
        [
            "account kbf-vm".to_owned(),
            "command /usr/bin/fdesetup isactive (no intent)".to_owned(),
            "reset_password kbf-vm".to_owned(),
            format!(
                "command /usr/bin/defaults write {domain} autoLoginUser -string kbf-vm (intent vm)"
            ),
            format!("command {RESTART} (intent vm)"),
        ]
    );
    assert_eq!(intent(&rig), Some(Intent::Vm));
    assert_eq!(
        std::fs::read(rig.dir.join("kcpassword")).unwrap(),
        kcpassword(b"pw-kbf-vm")
    );
    // Repeating it changes nothing but the password.
    rig.helper.session_idle().unwrap();
    assert_eq!(intent(&rig), Some(Intent::Vm));
    assert!(
        log(&rig)
            .iter()
            .all(|call| !call.contains("defaults delete"))
    );
}

/// Catches: the whole-machine lane broken by a VM user: a lease's session not replaced
/// by `session-idle`, a session switch away from the VM user refused, a second lease
/// taking a lease's session, or a lease user that cannot be deleted after it.
#[test]
fn whole_machine_leases_still_switch_through_the_vm_user() {
    let rig = vm_rig("lane");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.user_create("1.2", None).unwrap();
    rig.helper.session_idle().unwrap();
    assert_eq!(intent(&rig), Some(Intent::Vm));
    // The VM user's session gives way to a lease.
    rig.helper.session_login("1.1").unwrap();
    assert_eq!(intent(&rig), Some(Intent::Lease(LeaseId::new(1, 1))));
    assert_eq!(
        std::fs::read(rig.dir.join("kcpassword")).unwrap(),
        kcpassword(b"pw-kbf-lease-1-1")
    );
    let error = rig.helper.session_login("1.2").unwrap_err();
    assert!(error.contains("belongs to lease 1.1"), "{error}");
    let error = rig.helper.user_delete("1.1").unwrap_err();
    assert!(error.contains("holds the GUI session"), "{error}");
    // Idle: back to the VM user, never the login window.
    log(&rig);
    rig.helper.session_idle().unwrap();
    assert_eq!(intent(&rig), Some(Intent::Vm));
    assert_eq!(
        std::fs::read(rig.dir.join("kcpassword")).unwrap(),
        kcpassword(b"pw-kbf-vm")
    );
    assert!(
        log(&rig)
            .iter()
            .all(|call| !call.contains("defaults delete"))
    );
    // The lease user may go, and the next lease takes the session from the VM user.
    rig.helper.kill_uid("1.1").unwrap();
    assert_eq!(rig.helper.user_delete("1.1"), Ok(true));
    rig.helper.session_login("1.2").unwrap();
    assert_eq!(intent(&rig), Some(Intent::Lease(LeaseId::new(1, 2))));
    // The VM user's own account was never touched.
    assert!(rig.host.state().users.contains_key(VM));
}

/// Catches: `session-idle` that rests at the VM user after a step failed (auto-login
/// naming a user with no password written), or with FileVault on (a restart to the
/// unlock screen), or with no VM account (auto-login naming no one), and one that
/// clears the lease session it could not replace.
#[test]
fn session_idle_at_the_vm_user_stops_at_the_first_step_that_fails() {
    let rig = vm_rig("idle-vm-fail");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.session_login("1.1").unwrap();
    let lease = Some(Intent::Lease(LeaseId::new(1, 1)));
    let kc = std::fs::read(rig.dir.join("kcpassword")).unwrap();
    let untouched = |rig: &Rig| {
        let calls = log(rig);
        assert!(
            calls
                .iter()
                .all(|call| !call.contains("defaults") && !call.contains(RESTART)),
            "{calls:#?}"
        );
    };
    log(&rig);

    // FileVault on.
    answer(
        &rig,
        "/usr/bin/fdesetup isactive".to_owned(),
        true,
        "true\n",
        "",
    );
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("FileVault is on"), "{error}");
    assert!(!log(&rig).iter().any(|call| call.contains("reset_password")));
    assert_eq!(intent(&rig), lease);
    rig.host.state().answers.clear();

    // No VM account.
    let account = rig.host.state().users.remove(VM).unwrap();
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("kbf-vm has no account"), "{error}");
    untouched(&rig);
    assert_eq!(intent(&rig), lease);
    assert_eq!(std::fs::read(rig.dir.join("kcpassword")).unwrap(), kc);
    rig.host.state().users.insert(VM.to_owned(), account);

    // The record, the password, `/etc/kcpassword`, `defaults` and the restart, each
    // failing alone.
    let blocker = |name: &str| std::fs::create_dir_all(rig.dir.join(name).join("inside")).unwrap();
    let unblock = |name: &str| std::fs::remove_dir_all(rig.dir.join(name)).unwrap();
    blocker("session-intent.new");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("writing"), "{error}");
    untouched(&rig);
    unblock("session-intent.new");
    rig.host.state().fail.insert("reset_password");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("setting the password of kbf-vm"), "{error}");
    untouched(&rig);
    rig.host.state().fail.clear();
    blocker("kcpassword.new");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("kcpassword"), "{error}");
    untouched(&rig);
    unblock("kcpassword.new");
    let write = format!(
        "/usr/bin/defaults write {} autoLoginUser -string kbf-vm",
        domain(&rig)
    );
    answer(&rig, write, false, "", "no");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("no"), "{error}");
    assert!(log(&rig).iter().all(|call| !call.contains(RESTART)));
    rig.host.state().answers.clear();
    answer(&rig, RESTART.to_owned(), false, "", "not now");
    let error = rig.helper.session_idle().unwrap_err();
    assert!(error.contains("not now"), "{error}");
    // The lease's password file was only replaced by the idle's own steps.
    assert_ne!(std::fs::read(rig.dir.join("kcpassword")).unwrap(), kc);
}

/// Catches: a boot reset that deletes or kills the VM user (it deletes every lease
/// user the ledger holds and none else), clears the VM user's auto-login, or leaves a
/// leftover lease user standing beside it.
#[test]
fn the_boot_reset_deletes_lease_users_and_never_the_vm_user() {
    let rig = vm_rig("reset-vm");
    rig.helper.user_create("1.1", None).unwrap();
    rig.helper.user_create("1.2", None).unwrap();
    rig.helper.session_idle().unwrap();
    let before = std::fs::read(rig.dir.join("kcpassword")).unwrap();
    {
        let mut state = rig.host.state();
        state.procs.insert(vm_uid(), 4);
        state.procs.insert(me(), 1);
    }
    let booted = rebooted(&rig, "reset-vm", Some(VM));
    booted.host.state().procs.insert(vm_uid(), 4);
    booted.host.state().procs.insert(me(), 1);
    // A lease the server holds, whose record is the VM user's: not kept.
    let reset = booted.helper.boot_reset(&["1.1".to_owned()]).unwrap();
    assert_eq!(
        reset,
        Reset {
            kept: None,
            deleted: vec![LeaseId::new(1, 1), LeaseId::new(1, 2)],
        }
    );
    let calls = log(&booted);
    assert!(
        calls
            .iter()
            .all(|call| !call.contains(VM) || call == "account kbf-vm"),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .all(|call| call != &format!("kill_all {}", vm_uid()))
    );
    assert!(
        calls
            .iter()
            .all(|call| !call.contains(&format!("/{}", vm_uid())))
    );
    assert!(calls.iter().all(|call| !call.contains("defaults")));
    assert_eq!(booted.host.state().procs[&vm_uid()], 4);
    let users = booted.host.state().users.clone();
    assert_eq!(users.keys().collect::<Vec<_>>(), [VM]);
    assert_eq!(intent(&booted), Some(Intent::Vm));
    assert_eq!(
        std::fs::read(booted.dir.join("kcpassword")).unwrap(),
        before
    );
}

/// Catches: a boot reset that undoes what provisioning set (auto-login at the VM user,
/// no record yet), adopts any other auto-login, keeps a stale lease session or a stale
/// `vm` record on a Mac with no usable VM user, or loses the adoption when the record
/// cannot be written.
#[test]
fn the_boot_reset_keeps_only_a_rest_at_a_usable_vm_user() {
    let reset = |rig: &Rig, name: &str, vm: Option<&str>| {
        let booted = rebooted(rig, name, vm);
        let reset = booted.helper.boot_reset(&[]).unwrap();
        assert_eq!(reset.kept, None);
        let calls = log(&booted);
        let cleared = calls.iter().any(|call| call.contains("defaults delete"));
        (booted, cleared)
    };
    let kcpassword_file = |rig: &Rig| std::fs::write(rig.dir.join("kcpassword"), "x").unwrap();

    // Provisioning set auto-login to the VM user and wrote no record: adopted.
    let rig = vm_rig("adopt");
    auto_login_is(&rig, VM);
    kcpassword_file(&rig);
    let (booted, cleared) = reset(&rig, "adopt", Some(VM));
    assert!(!cleared);
    assert_eq!(intent(&booted), Some(Intent::Vm));
    assert!(booted.dir.join("kcpassword").exists());

    // Auto-login names someone else, a stale lease user, or cannot be read.
    for (name, says) in [
        ("other", Some("someone\n")),
        ("stale", Some("kbf-lease-4-4\n")),
    ] {
        let rig = vm_rig(name);
        auto_login_is(&rig, says.unwrap().trim());
        kcpassword_file(&rig);
        let (booted, cleared) = reset(&rig, name, Some(VM));
        assert!(cleared, "{name}");
        assert_eq!(intent(&booted), None);
        assert!(!booted.dir.join("kcpassword").exists());
    }
    let rig = vm_rig("unreadable");
    let read = format!("/usr/bin/defaults read {} autoLoginUser", domain(&rig));
    answer(&rig, read, false, "", "boom");
    let (booted, cleared) = reset(&rig, "unreadable", Some(VM));
    assert!(cleared);
    assert_eq!(intent(&booted), None);

    // The record cannot be written: not adopted.
    let rig = vm_rig("adopt-fail");
    auto_login_is(&rig, VM);
    std::fs::create_dir_all(rig.dir.join("session-intent.new/inside")).unwrap();
    let (booted, cleared) = reset(&rig, "adopt-fail", Some(VM));
    assert!(cleared);
    assert_eq!(intent(&booted), None);

    // A stale lease record does not make the VM user the rest, even with auto-login
    // naming it.
    let rig = vm_rig("lease-record");
    std::fs::write(rig.dir.join("session-intent"), "4.4\n").unwrap();
    auto_login_is(&rig, VM);
    let (booted, cleared) = reset(&rig, "lease-record", Some(VM));
    assert!(cleared);
    assert_eq!(intent(&booted), None);

    // A `vm` record with no VM user configured, or no account: stale.
    let rig = vm_rig("vm-record");
    std::fs::write(rig.dir.join("session-intent"), "vm\n").unwrap();
    kcpassword_file(&rig);
    let (booted, cleared) = reset(&rig, "vm-record", None);
    assert!(cleared);
    assert_eq!(intent(&booted), None);
    assert!(!booted.dir.join("kcpassword").exists());
    let rig = vm_rig("vm-noaccount");
    std::fs::write(rig.dir.join("session-intent"), "vm\n").unwrap();
    rig.host.state().users.clear();
    let (booted, cleared) = reset(&rig, "vm-noaccount", Some(VM));
    assert!(cleared);
    assert_eq!(intent(&booted), None);
}
