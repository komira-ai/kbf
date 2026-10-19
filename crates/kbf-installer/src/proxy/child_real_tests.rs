//! Real processes (the stand-in children of [`crate::proxy::child_rig`]) under the real
//! child port.

use super::*;
use crate::proxy::child_rig::{Cleanup, Rig, alive, await_gone, refused, rig, running, wait_for};
use crate::testkit::{A, B, c};
use std::time::{Duration, Instant};

/// Catches a spawn that runs another file than the release's (the `current` link, a stale
/// path), that records a different identity than the live process's, or that leaves the
/// record unwritten: the record is what finds a candidate the update file does not know.
#[test]
fn a_spawned_child_runs_the_release_and_its_identity_is_recorded_before_spawn_returns() {
    let rig = rig("child-real-spawn");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    assert_eq!(ident, ident_of(ident.pid).expect("live"));
    assert_eq!(
        ident.exe,
        std::env::current_exe()
            .expect("exe")
            .canonicalize()
            .expect("canonical"),
        "the program is the one the release's file links to"
    );
    assert_eq!(
        rig.child.recorded(Slot::A),
        Some(Recorded {
            ident: ident.clone(),
            commit: c(A)
        })
    );
    assert_eq!(rig.child.recorded(Slot::B), None);
    assert_eq!(rig.child.exited(&ident), None);
}

/// Catches a SIGTERM that is really a SIGKILL (status 137), a terminate that does not reach
/// the child, an exit that is not reaped (the zombie would stay) and a record left behind.
#[test]
fn terminate_is_sigterm_and_the_exit_is_reaped_and_clears_the_record() {
    let rig = rig("child-real-term");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    rig.child.terminate(&ident).expect("signal");
    let mut code = None;
    wait_for("the child to exit", || {
        code = rig.child.exited(&ident);
        code.is_some()
    });
    assert_eq!(code, Some(128 + 15), "killed by SIGTERM, not SIGKILL");
    assert!(!alive(ident.pid), "reaped, not a zombie");
    assert_eq!(rig.child.recorded(Slot::A), None);
    // The entry is gone: the pid is not this port's any more.
    assert_eq!(rig.child.exited(&ident), Some(UNKNOWN_EXIT));
    assert!(rig.child.terminate(&ident).is_err());
}

/// Catches a child that reports no exit code, or the wrong one.
#[test]
fn a_child_that_exits_reports_its_status() {
    let rig = rig("child-real-exit");
    rig.set(&["exit", "3"]);
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let mut code = None;
    wait_for("the exit", || {
        code = rig.child.exited(&ident);
        code.is_some()
    });
    assert_eq!(code, Some(3));
}

/// A restarted proxy adopts the live child by its recorded identity, watches it by pidfd
/// (its exit code is unknown: it is not the new proxy's child) and signals it through that
/// pidfd. The first port plays the dead proxy and only reaps.
#[test]
fn a_live_child_is_adopted_after_a_restart_then_watched_and_terminated() {
    let rig = rig("child-real-adopt");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let recorded = after
        .recorded(Slot::A)
        .expect("the record survives the restart");
    assert_eq!(after.adopt(Slot::A, &recorded.ident), Ok(()));
    assert_eq!(after.exited(&ident), None);
    after.terminate(&ident).expect("signal");
    wait_for("the adopted child to exit", || {
        after.exited(&ident).is_some()
    });
    assert_eq!(after.exited(&ident), Some(UNKNOWN_EXIT));
    // The first port reaps its zombie, as the kernel hands the orphan to init in life.
    wait_for("the reap", || rig.child.exited(&ident).is_some());
    assert!(!alive(ident.pid));
}

/// Catches an adoption that accepts a pid whose start time or executable differs (a recycled
/// pid), one that accepts a pid that is gone, and the worse failure: a refused process being
/// signalled afterwards. The refused process is a real one and must keep running.
#[test]
fn a_recycled_pid_is_never_adopted_or_signalled() {
    let rig = rig("child-real-recycled");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let after = rig.restarted();
    let wrong_start = ProcIdent {
        start_time: ident.start_time + 1,
        ..ident.clone()
    };
    let wrong_exe = ProcIdent {
        exe: PathBuf::from("/usr/bin/other"),
        ..ident.clone()
    };
    for forged in [&wrong_start, &wrong_exe] {
        assert_eq!(
            after.adopt(Slot::A, forged),
            Err(AdoptError::Recycled { pid: ident.pid })
        );
        assert!(after.terminate(forged).is_err(), "not held, not signalled");
        assert_eq!(after.exited(forged), Some(UNKNOWN_EXIT), "not ours");
    }
    // The port that holds the process refuses a forged identity for the same pid, too: the
    // pid alone is not enough to be signalled.
    for forged in [&wrong_start, &wrong_exe] {
        assert!(rig.child.terminate(forged).is_err());
        assert!(rig.child.kill_unserved(forged).is_err());
        assert_eq!(rig.child.exited(forged), Some(UNKNOWN_EXIT));
    }
    assert!(
        alive(ident.pid),
        "the process behind the pid was left alone"
    );
    assert_eq!(rig.child.exited(&ident), None);
    // A pid that is gone is gone, not recycled.
    rig.child.terminate(&ident).expect("signal");
    wait_for("exit", || rig.child.exited(&ident).is_some());
    assert_eq!(
        after.adopt(Slot::A, &ident),
        Err(AdoptError::Gone(ident.pid))
    );
}

/// Catches a `kill_unserved` that signals the wrong process: the candidate dies, the serving
/// child in the other slot does not, and a process the port does not hold is refused.
#[test]
fn kill_unserved_kills_the_candidate_alone_and_reaps_it() {
    let rig = rig("child-real-kill");
    let old = rig.child.spawn(Slot::A, &c(A), Start::Active).expect("old");
    let _stop = Cleanup(&rig.child, vec![old.clone()]);
    // The candidate ignores SIGTERM, so only the SIGKILL ends it; it says when it does.
    let up = rig.layout_root.join("candidate-up");
    let up_path = up.to_string_lossy().into_owned();
    rig.set(&[
        "ignore-signal",
        "15",
        ";",
        "append",
        &up_path,
        "up",
        ";",
        "sleep",
        "600",
    ]);
    let candidate = rig
        .child
        .spawn(Slot::B, &c(B), Start::Active)
        .expect("candidate");
    let _stop_candidate = Cleanup(&rig.child, vec![candidate.clone()]);
    wait_for("the candidate to ignore SIGTERM", || up.exists());
    rig.child.kill_unserved(&candidate).expect("kill");
    wait_for("the candidate to go", || !alive(candidate.pid));
    wait_for("the candidate to be reaped", || {
        !Path::new(&format!("/proc/{}", candidate.pid)).exists()
    });
    assert!(alive(old.pid), "the serving child is untouched");
    assert_eq!(rig.child.exited(&old), None);
    assert_eq!(rig.child.recorded(Slot::B), None, "its record is cleared");
    assert!(rig.child.recorded(Slot::A).is_some());
    assert!(
        rig.child.kill_unserved(&candidate).is_err(),
        "no longer held"
    );
}

/// Catches a second child started into a slot that has a live one (the swap spawns only
/// into an idle slot), and a slot that stays blocked after its child exited.
#[test]
fn a_slot_with_a_live_child_refuses_a_second_and_is_free_once_it_exited() {
    let rig = rig("child-real-occupied");
    let first = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("first");
    let _stop = Cleanup(&rig.child, vec![first.clone()]);
    let busy = refused(
        &rig.child,
        rig.child.spawn(Slot::A, &c(B), Start::Active),
        "occupied",
    );
    assert!(busy.0.contains("live child already"), "{busy}");
    rig.child.terminate(&first).expect("signal");
    wait_for("exit", || !alive(first.pid));
    rig.set(&["sleep", "600"]);
    let second = rig
        .child
        .spawn(Slot::A, &c(B), Start::Active)
        .expect("free again");
    assert_ne!(first.pid, second.pid);
    let _stop2 = Cleanup(&rig.child, vec![second]);
}

/// Catches a spawn that reports success for a program that does not exist, and a record
/// written for a child that never started.
#[test]
fn a_release_without_a_program_is_a_spawn_error_and_records_nothing() {
    let rig = rig("child-real-missing");
    let release = CommitId::new(&"c".repeat(40)).expect("a commit");
    let e = refused(
        &rig.child,
        rig.child.spawn(Slot::A, &release, Start::Active),
        "no program",
    );
    assert!(e.0.contains("kbf-act"), "{e}");
    assert_eq!(rig.child.recorded(Slot::A), None);
}

/// Catches a child left running that nothing can find again: when the record cannot be
/// written, the spawn fails and the child it started is killed.
#[test]
fn a_child_whose_record_cannot_be_written_is_killed_and_the_spawn_fails() {
    let rig = rig("child-real-norecord");
    // The record directory is a file, so the record cannot be made.
    fs::write(&rig.dir, b"in the way").expect("block");
    rig.set(&["sleep", "31337"]);
    let e = refused(
        &rig.child,
        rig.child.spawn(Slot::A, &c(A), Start::Active),
        "no record",
    );
    assert!(e.0.contains("record the new child"), "{e}");
    let running = || {
        fs::read_dir("/proc")
            .expect("proc")
            .flatten()
            .filter_map(|e| fs::read(e.path().join("cmdline")).ok())
            .any(|cmd| {
                cmd.windows(5).any(|w| w == b"31337")
                    && cmd.starts_with(rig.layout_root.as_os_str().as_encoded_bytes())
            })
    };
    wait_for("the child to be killed", || !running());
}

/// Catches children that stay in the proxy's process group (a signal to the group would
/// reach them) or that inherit the activation variables.
#[test]
fn a_child_leads_its_own_process_group_and_gets_no_activation_variables() {
    let rig = rig("child-real-group");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let stat = fs::read_to_string(format!("/proc/{}/stat", ident.pid)).expect("stat");
    // After the command name: state, parent, process group.
    let group: u32 = stat[stat.rfind(')').expect("comm") + 1..]
        .split_whitespace()
        .nth(2)
        .and_then(|g| g.parse().ok())
        .expect("process group");
    assert_eq!(group, ident.pid, "the child leads its own group");

    let mut command = Command::new("x");
    detach(&mut command);
    let removed: Vec<_> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    for name in [
        "LISTEN_PID",
        "LISTEN_FDS",
        "LISTEN_FDNAMES",
        "NOTIFY_SOCKET",
    ] {
        assert!(
            removed.iter().any(|n| n == name),
            "{name} is kept: {removed:?}"
        );
    }
}

/// Catches a start time read from the wrong field when the command name holds spaces or
/// parentheses, which `/proc/<pid>/stat` does not escape.
#[test]
fn the_start_time_is_field_22_whatever_the_command_name_holds() {
    let tail =
        "S 1 1 1 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 424242 1000 10 18446744073709551615";
    for comm in ["kbf", "a b", "x) S 9 (y", "((("] {
        let line = format!("123 ({comm}) {tail}");
        assert_eq!(start_time_of(&line), Some(424_242), "{comm}");
    }
    assert_eq!(start_time_of("123 (kbf)"), None);
    assert_eq!(start_time_of("no parenthesis"), None);
}

/// Catches a port that kills its children when it goes away: a proxy restart must leave them
/// serving. The port is dropped, the child lives, and a port of the restarted proxy adopts it.
#[test]
fn dropping_the_port_leaves_the_child_running_for_the_next_proxy_to_adopt() {
    let rig = rig("child-real-drop");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let after = rig.restarted();
    let Rig { child, .. } = rig;
    drop(child);
    assert!(
        alive(ident.pid),
        "the child outlives the port that started it"
    );
    let recorded = after.recorded(Slot::A).expect("record");
    assert_eq!(after.adopt(Slot::A, &recorded.ident), Ok(()));
    let _stop = Cleanup(&after, vec![ident.clone()]);
    assert_eq!(after.exited(&ident), None);
}

/// Catches an adoption that trusts the identity it read before it opened the pidfd: a pid
/// recycled between the two reads would be signalled through a pidfd of another process.
#[test]
fn a_pid_recycled_between_the_check_and_the_open_is_refused() {
    use std::cell::Cell;
    let recorded = ProcIdent {
        pid: 4242,
        start_time: 7,
        exe: PathBuf::from("/x/kbf"),
    };
    let other = ProcIdent {
        start_time: 8,
        ..recorded.clone()
    };
    let reads = Cell::new(0);
    let identity = |_: u32| {
        reads.set(reads.get() + 1);
        Ok(if reads.get() == 1 {
            recorded.clone()
        } else {
            other.clone()
        })
    };
    let open = |_: u32| Ok(OwnedFd::from(fs::File::open("/dev/null").expect("open")));
    assert_eq!(
        verified_open(&recorded, identity, open).err(),
        Some(AdoptError::Recycled { pid: 4242 })
    );
    // The same process on both reads is accepted, and a process that vanished is `Gone`.
    assert!(verified_open(&recorded, |_| Ok(recorded.clone()), open).is_ok());
    assert_eq!(
        verified_open(&recorded, |_| Err(std::io::Error::other("gone")), open).err(),
        Some(AdoptError::Gone(4242))
    );
    let no_pidfd = |_: u32| Err(Errno::SRCH);
    assert_eq!(
        verified_open(&recorded, |_| Ok(recorded.clone()), no_pidfd).err(),
        Some(AdoptError::Gone(4242))
    );
}

/// Catches an identity read that takes a malformed `stat` for a start time, and one that
/// reads the executable of another entry: over a `/proc` of the test's own.
#[test]
fn an_identity_is_the_start_time_and_the_executable_of_that_entry() {
    let proc = kbf_host::testkit::scratch("child-real-proc");
    let entry = proc.join("77");
    fs::create_dir_all(&entry).expect("entry");
    let tail = "S 1 1 1 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 31337 1000 10 1844";
    fs::write(entry.join("stat"), format!("77 (kbf) {tail}")).expect("stat");
    std::os::unix::fs::symlink("/usr/lib/kbf", entry.join("exe")).expect("exe");
    assert_eq!(
        ident_in(&proc, 77).expect("identity"),
        ProcIdent {
            pid: 77,
            start_time: 31_337,
            exe: PathBuf::from("/usr/lib/kbf"),
        }
    );
    fs::write(entry.join("stat"), "77 (kbf)").expect("short stat");
    let e = ident_in(&proc, 77).expect_err("short");
    assert!(
        e.to_string().contains("unreadable") && e.to_string().contains("77"),
        "{e}"
    );
    assert!(ident_in(&proc, 78).is_err(), "no such entry");
}

/// Catches a slot that an adopted child holds being given a second child: the occupancy
/// check watches an adopted process by its pidfd, as it watches its own by `try_wait`.
#[test]
fn a_slot_held_by_an_adopted_child_refuses_a_spawn_until_that_child_exits() {
    let rig = rig("child-real-adopted-slot");
    let ident = rig
        .child
        .spawn(Slot::A, &c(A), Start::Active)
        .expect("spawn");
    let after = rig.restarted();
    let _stop = Cleanup(&rig.child, vec![ident.clone()]);
    let recorded = after.recorded(Slot::A).expect("record");
    after.adopt(Slot::A, &recorded.ident).expect("adopt");
    let busy = refused(
        &after,
        after.spawn(Slot::A, &c(B), Start::Active),
        "held by the adopted child",
    );
    assert!(busy.0.contains("live child already"), "{busy}");
    after.terminate(&ident).expect("signal");
    wait_for("the adopted child to exit", || {
        after.exited(&ident).is_some()
    });
    rig.set(&["sleep", "600"]);
    let next = after
        .spawn(Slot::A, &c(B), Start::Active)
        .expect("free again");
    let _stop2 = Cleanup(&after, vec![next]);
}

/// Catches a started process that cannot be named being left running: when the pidfd or the
/// identity fails, the spawn fails and the child is killed.
#[test]
fn a_started_child_that_cannot_be_identified_is_killed_and_the_spawn_fails() {
    let rig = rig("child-real-noident");
    rig.set(&["sleep", "31338"]);
    let port = rig.restarted().with_identify(|_| Err("no pidfd".into()));
    let e = refused(
        &port,
        port.spawn(Slot::A, &c(A), Start::Active),
        "not identified",
    );
    assert!(
        e.0.contains("identify the new child") && e.0.contains("no pidfd"),
        "{e}"
    );
    assert_eq!(port.recorded(Slot::A), None);
    let root = rig.layout_root.as_os_str().as_encoded_bytes().to_vec();
    wait_for("the child to be killed", || {
        !fs::read_dir("/proc")
            .expect("proc")
            .flatten()
            .filter_map(|e| fs::read(e.path().join("cmdline")).ok())
            .any(|cmd| cmd.starts_with(&root) && cmd.windows(5).any(|w| w == b"31338"))
    });
}

/// Catches a signal error being reported as success: a held entry whose descriptor is not a
/// pidfd fails the signal, and the error names the pid.
#[test]
fn a_signal_the_kernel_refuses_is_an_error() {
    let rig = rig("child-real-badfd");
    let ident = ProcIdent {
        pid: 31_339,
        start_time: 1,
        exe: PathBuf::from("/x/kbf"),
    };
    rig.child.held.lock().expect("lock").insert(
        ident.pid,
        Held {
            slot: Slot::A,
            ident: ident.clone(),
            pidfd: OwnedFd::from(fs::File::open("/dev/null").expect("open")),
            process: None,
        },
    );
    let e = rig.child.terminate(&ident).expect_err("not a pidfd");
    assert!(e.0.contains("signal 31339"), "{e}");
}

/// Catches a guard that cannot tell a running process from a gone one, and one that gives up
/// at once or never: the test's own process is running (and a recycled pid, the same pid with
/// another start time, is not), and the wait sleeps its polls while it runs. Deterministic:
/// the guard's own wait only sleeps when a killed child has not quite gone yet.
#[test]
fn the_cleanup_guard_sees_a_running_process_and_waits_a_bounded_time_for_it() {
    let me = ident_of(std::process::id()).expect("own identity");
    assert_eq!(running(std::slice::from_ref(&me)), vec![me.pid]);
    let recycled = ProcIdent {
        start_time: me.start_time + 1,
        ..me.clone()
    };
    assert!(
        running(std::slice::from_ref(&recycled)).is_empty(),
        "another process"
    );
    let started = Instant::now();
    await_gone(std::slice::from_ref(&me), 3, Duration::from_millis(20));
    assert!(
        started.elapsed() >= Duration::from_millis(60),
        "three polls"
    );
    let started = Instant::now();
    await_gone(&[recycled], 50, Duration::from_millis(100));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "nothing to wait for"
    );
}

/// Catches an adoption of a pid that is not a process, or of a vanished one: through the real
/// reads, a pid with no entry is `Gone` at the first identity read, and a thread id (its entry
/// is there, so its identity reads; `pidfd_open` refuses a thread that leads no group) is
/// `Gone` at the open.
#[test]
fn the_real_reads_refuse_a_missing_pid_and_a_thread_id() {
    let missing = ProcIdent {
        pid: u32::MAX - 1,
        start_time: 1,
        exe: PathBuf::from("/x/kbf"),
    };
    assert_eq!(
        open_verified(&missing).err(),
        Some(AdoptError::Gone(missing.pid))
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let (done, wait) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        // SAFETY: `gettid` has no arguments and cannot fail.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        tx.send(tid).expect("send");
        wait.recv().expect("held until the test is done");
    });
    let tid = rx.recv().expect("tid");
    let ident = ident_of(tid).expect("a thread has a stat entry");
    assert_eq!(open_verified(&ident).err(), Some(AdoptError::Gone(tid)));
    done.send(()).expect("release");
    thread.join().expect("thread");
}

/// Catches a pid the kernel API cannot take being passed on: 0 and a number above `i32::MAX`
/// are `EINVAL` before any syscall, and `identify_started` names which read failed (the
/// pidfd of a pid that is not a process; the identity of a process that is a zombie, whose
/// `exe` is gone while its pidfd still opens).
#[test]
fn identify_started_and_pidfd_of_refuse_what_is_not_a_live_process() {
    assert_eq!(pidfd_of(0).err(), Some(Errno::INVAL));
    assert_eq!(pidfd_of(u32::MAX).err(), Some(Errno::INVAL));
    assert!(identify_started(u32::MAX - 1).is_err(), "no such process");

    let mut zombie = std::process::Command::new("true")
        .spawn()
        .expect("a short-lived process");
    let pid = zombie.id();
    wait_for("the process to exit", || {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.rsplit(')')
                .next()
                .is_some_and(|r| r.trim_start().starts_with('Z'))
        })
    });
    let e = identify_started(pid).expect_err("a zombie has no identity");
    assert!(!e.is_empty(), "the reason is kept");
    zombie.wait().expect("reaped");
}
