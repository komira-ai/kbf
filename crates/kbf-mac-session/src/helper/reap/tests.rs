//! `role-reap`: which process groups the helper ends.

use crate::helper::role::tests::{vm_rig, vm_uid};
use crate::helper::tests::{Rig, log, me};
use crate::helper::{Outcome, Process};
use crate::proto::{Reply, Request};

use super::REAP_ROUNDS;

const BOOT: &str = "boot-2";
const PGID: i32 = 4242;
const START: u64 = 1_700_000_000_123_456;

fn process(pid: i32, pgid: i32, uid: u32, start: u64) -> Process {
    Process {
        pid,
        pgid,
        uid,
        ruid: uid,
        start,
        zombie: false,
    }
}

/// The run's leader and two children, all the VM user's.
fn run(rig: &Rig) {
    let uid = vm_uid();
    rig.host.state().table = vec![
        process(PGID, PGID, uid, START),
        process(PGID + 1, PGID, uid, START + 10),
        process(PGID + 2, PGID, uid, START + 20),
        process(100, 100, 0, 5),
    ];
}

fn killed(calls: &[String]) -> usize {
    calls
        .iter()
        .filter(|call| call.starts_with("kill_group"))
        .count()
}

/// Catches: a reap that leaves part of the run (the leader alone killed), kills a group
/// the record does not name (the unrelated process 100 is in the table), or answers
/// before the group is empty.
#[test]
fn the_recorded_run_s_group_is_killed_and_no_other() {
    let rig = vm_rig("reap-ok");
    run(&rig);
    rig.helper.role_reap(PGID, START, BOOT).unwrap();
    let calls = log(&rig);
    assert_eq!(killed(&calls), 1, "{calls:#?}");
    assert!(calls.contains(&format!("kill_group {PGID}")), "{calls:#?}");
    assert_eq!(rig.host.state().table, vec![process(100, 100, 0, 5)]);
}

/// Catches: a pid reused by another process (a different start time) being killed with
/// its group, and a record of an earlier boot or of a leader that is gone acting on
/// whatever has the pid now. Each is a success: the run is not there.
#[test]
fn a_run_that_is_not_the_recorded_one_is_left_alone() {
    let rig = vm_rig("reap-other");
    run(&rig);
    let before = rig.host.state().table.clone();
    for (pgid, start, boot) in [
        (PGID, START + 1, BOOT),
        (PGID, START - 1, BOOT),
        (PGID, START, "boot-1"),
        (PGID + 500, START, BOOT),
    ] {
        rig.helper.role_reap(pgid, start, boot).unwrap();
        assert_eq!(killed(&log(&rig)), 0, "{pgid} {start} {boot}");
    }
    assert_eq!(rig.host.state().table, before);
}

/// Catches: a record naming a process the VM user does not own (a lease user's, root's)
/// or one that is no group leader, being killed on the strength of its start time.
#[test]
fn a_process_that_is_not_the_vm_user_s_leader_is_refused() {
    let rig = vm_rig("reap-owner");
    for (leader, says) in [
        (process(PGID, PGID, me(), START), "is not the VM user's"),
        (process(PGID, PGID, 0, START), "is not the VM user's"),
        (
            Process {
                ruid: 0,
                ..process(PGID, PGID, vm_uid(), START)
            },
            "is not the VM user's",
        ),
        (
            Process {
                uid: 0,
                ..process(PGID, PGID, vm_uid(), START)
            },
            "is not the VM user's",
        ),
        (process(PGID, PGID - 1, vm_uid(), START), "does not lead"),
    ] {
        rig.host.state().table = vec![leader, process(PGID + 1, PGID, vm_uid(), START)];
        let error = rig.helper.role_reap(PGID, START, BOOT).unwrap_err();
        assert!(error.contains(says), "{leader:?}: {error}");
        assert_eq!(killed(&log(&rig)), 0, "{leader:?}");
    }
}

/// Catches: a group id that means more than one group (`kill(-1)` style: 0, 1, negative)
/// reaching the kill, and any host call being made for it.
#[test]
fn a_group_id_of_one_or_less_is_refused_before_anything_is_read() {
    let rig = vm_rig("reap-ids");
    for pgid in [i32::MIN, -1, 0, 1] {
        let error = rig.helper.role_reap(pgid, START, BOOT).unwrap_err();
        assert!(
            error.contains("is not one role-reap will signal"),
            "{error}"
        );
    }
    assert_eq!(log(&rig), Vec::<String>::new());
}

/// Catches: a reap that stops at one kill (a member forks as it dies), one that reports
/// success while the group has live members, and one that loops for ever.
#[test]
fn the_kill_repeats_until_the_group_is_empty_and_gives_up() {
    let rig = vm_rig("reap-rounds");
    run(&rig);
    rig.host.state().stubborn_groups = 3;
    rig.helper.role_reap(PGID, START, BOOT).unwrap();
    assert_eq!(killed(&log(&rig)), 4);

    run(&rig);
    rig.host.state().stubborn_groups = usize::MAX;
    let error = rig.helper.role_reap(PGID, START, BOOT).unwrap_err();
    assert!(error.contains("remain after"), "{error}");
    assert_eq!(killed(&log(&rig)), REAP_ROUNDS);
}

/// Catches: zombies (which no signal ends) counted as survivors, which would fail every
/// reap of a group whose leader the daemon has not waited for.
#[test]
fn a_zombie_is_not_a_survivor() {
    let rig = vm_rig("reap-zombie");
    run(&rig);
    rig.host.state().stubborn_groups = 1;
    rig.host.state().table.iter_mut().for_each(|p| {
        if p.pgid == PGID {
            p.zombie = true;
        }
    });
    rig.helper.role_reap(PGID, START, BOOT).unwrap();
    assert_eq!(killed(&log(&rig)), 1);
}

/// Catches: a second round that keeps signalling a group a process that is not the VM
/// user's has joined (the pid was recycled into another group member's).
#[test]
fn a_member_that_is_not_the_vm_user_s_stops_the_kill() {
    let rig = vm_rig("reap-foreign");
    run(&rig);
    rig.host.state().stubborn_groups = usize::MAX;
    rig.host
        .state()
        .table
        .push(process(PGID + 9, PGID, 0, START + 99));
    let error = rig.helper.role_reap(PGID, START, BOOT).unwrap_err();
    assert!(
        error.contains("process 4251 in group 4242 is not the VM user's"),
        "{error}"
    );
    assert_eq!(killed(&log(&rig)), 1);
}

/// Catches: a failing host (the table, the kill, the boot id) being taken as a success.
#[test]
fn a_host_failure_is_an_error() {
    let rig = vm_rig("reap-fail");
    for (call, says) in [
        ("boot_session", "reading the boot session"),
        ("processes", "reading the process table"),
        ("kill_group", "killing process group"),
    ] {
        run(&rig);
        rig.host.state().fail.insert(call);
        let error = rig.helper.role_reap(PGID, START, BOOT).unwrap_err();
        assert!(error.contains(says), "{call}: {error}");
        rig.host.state().fail.clear();
    }
    // The table read again after the kill: one look succeeds, the next fails.
    run(&rig);
    rig.host.state().tables_ok = Some(1);
    let error = rig.helper.role_reap(PGID, START, BOOT).unwrap_err();
    assert!(error.contains("reading the process table"), "{error}");
}

/// Catches: a Mac with no VM user acting on a group anyway.
#[test]
fn a_mac_without_a_vm_user_refuses() {
    let none = crate::helper::tests::rig_vm_with(
        "reap-no-vm",
        crate::lease::UidRange::new(me(), me() + 3).unwrap(),
        None,
        None,
    );
    none.helper.boot_reset(&[]).unwrap();
    let error = none.helper.role_reap(PGID, START, BOOT).unwrap_err();
    assert!(error.contains("no VM user is configured"), "{error}");
}

/// Catches: the verb routed to another reply, or reachable before the boot reset, or
/// taking descriptors.
#[test]
fn the_verb_is_served_through_handle() {
    let rig = vm_rig("reap-handle");
    run(&rig);
    let request = Request::RoleReap {
        pgid: PGID,
        start: START,
        boot: BOOT.to_owned(),
    };
    let reply = |rig: &Rig, request: &Request, fds| match rig.helper.handle(request.clone(), fds) {
        Outcome::Reply(reply) => reply,
        Outcome::Running(child) => panic!("a process started: {child:?}"),
    };
    assert_eq!(
        reply(&rig, &request, crate::helper::tests::fds(&rig.dir)),
        Reply::Refused {
            reason: "only run and role-run take descriptors".to_owned()
        }
    );
    assert_eq!(reply(&rig, &request, Vec::new()), Reply::Killed);
    let text = serde_json::to_string(&request).unwrap();
    assert_eq!(
        text,
        format!(r#"{{"verb":"role-reap","pgid":{PGID},"start":{START},"boot":"{BOOT}"}}"#)
    );
}
