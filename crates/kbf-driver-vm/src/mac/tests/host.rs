//! `MacVmHost` over the fake session: what a start asks of the Mac and in which order,
//! what stop and the daemon's death do, and what is left behind.

use std::io::{Read as _, Write as _};
use std::time::Duration;

use kbf_driver_native::Record;
use kbf_vmm::spec::{Event, LIMIT_EXCEEDED};

use super::{FakeSession, Rig, rig, rig_with};
use super::{pid, upid};
use crate::fake::MACOS_VM_LIMIT;
use crate::host::{HostError, VmHost as _};

/// Catches: a start that asks the helper for anything but `<vmm> vm-run --spec <file>`
/// (the helper refuses anything else), a spec file in the wrong place, and a host that
/// does not start the VM at all.
#[test]
fn a_start_runs_vm_run_on_the_spec_in_the_clone() {
    let rig = rig("start-argv");
    let spec = rig.spec("lease-1-1");
    rig.host.start(&spec).unwrap();
    let file = spec.clone.join("vm-spec.json");
    assert_eq!(
        rig.session.argvs(),
        [[
            rig.vmm.display().to_string(),
            "vm-run".to_owned(),
            "--spec".to_owned(),
            file.display().to_string(),
        ]]
    );
    assert!(file.is_file());
    assert!(spec.clone.join("vm-run.log").is_file());
    rig.host.stop("lease-1-1").unwrap();
}

/// Catches: a record written after the VM started (a crash in between would leave a
/// running VM nothing names), a record that does not name the run (group id = the pid
/// the helper reported, the leader's start time, this boot), a pending trace left after
/// the complete record, and a record left after the run ended.
#[test]
fn the_record_is_written_before_the_run_and_removed_after_it() {
    let rig = rig("records");
    let spec = rig.spec("lease-1-1");
    rig.host.start(&spec).unwrap();
    // What the runs directory held when the helper was asked to start the process.
    assert_eq!(rig.session.seen_at_run(), [["lease-1-1.pending"]]);
    assert_eq!(rig.records(), ["lease-1-1"]);
    let text = std::fs::read_to_string(rig.runs().join("lease-1-1")).unwrap();
    let pgid = pid(0);
    assert_eq!(
        Record::parse(&text),
        Some(Record {
            pgid,
            start: 1_000_000 + u64::try_from(pgid).unwrap(),
            boot: "boot-1".to_owned(),
        })
    );
    rig.host.stop("lease-1-1").unwrap();
    assert!(rig.records().is_empty());
}

/// Catches: the third VM reaching the helper or the framework (it must be refused
/// before either), a slot given back before its run has ended, a slot not given back
/// after, and a refused start that leaves files behind.
#[test]
fn the_third_start_is_refused_before_any_call_to_the_session() {
    let rig = rig("limit");
    for n in 1..=MACOS_VM_LIMIT {
        rig.host.start(&rig.spec(&format!("lease-1-{n}"))).unwrap();
    }
    let calls = rig.session.calls().len();
    let third = rig.spec("lease-1-9");
    let refused = rig.host.start(&third).unwrap_err();
    assert!(matches!(refused, HostError::LimitExceeded(n) if n == MACOS_VM_LIMIT));
    assert_eq!(
        rig.session.calls().len(),
        calls,
        "{:?}",
        rig.session.calls()
    );
    assert_eq!(rig.records(), ["lease-1-1", "lease-1-2"]);
    assert!(!third.clone.join("vm-spec.json").exists());

    // A slot is free only once its run has exited.
    rig.host.stop("lease-1-1").unwrap();
    rig.host.start(&third).unwrap();
    assert!(matches!(
        rig.host.start(&rig.spec("lease-1-8")),
        Err(HostError::LimitExceeded(_))
    ));
    rig.host.stop("lease-1-2").unwrap();
    rig.host.stop("lease-1-9").unwrap();
    rig.settle();
    assert!(rig.both_slots_free());
}

/// Catches: a slot kept by a start that failed, wherever it failed: before the helper
/// is asked (a bad name, a name in use, a spec that cannot be written, a network device,
/// a session that is down or fails, the boot id, the records, the log), at the helper
/// (it refuses the run), or after it (the process table, the record, a process that is
/// already gone). Each leaves both slots free and no record.
#[test]
fn a_failed_start_gives_its_slot_back_at_every_step() {
    let rig = rig("errors");
    let ok = rig.spec("lease-1-1");
    rig.host.start(&ok).unwrap();
    assert!(!rig.both_slots_free());
    let in_use = rig.host.start(&ok).unwrap_err();
    assert!(
        matches!(in_use, HostError::Io(ref why) if why.kind() == std::io::ErrorKind::AlreadyExists)
    );
    rig.host.stop("lease-1-1").unwrap();
    rig.settle();
    assert!(rig.both_slots_free());

    let mut bad = rig.spec("bad name");
    bad.name = "a/b".to_owned();
    assert!(rig.host.start(&bad).is_err());
    bad.name = String::new();
    assert!(rig.host.start(&bad).is_err());
    bad.name = "x".repeat(200);
    assert!(rig.host.start(&bad).is_err());

    let mut network = rig.spec("lease-2-1");
    network.network = vec![crate::host::NetworkDevice::Nat];
    assert!(rig.host.start(&network).is_err());
    let mut missing = rig.spec("lease-2-2");
    missing.clone = rig.dir.path().join("no-such-clone");
    assert!(rig.host.start(&missing).is_err());
    let written = rig.spec("lease-2-3");
    std::fs::write(written.clone.join("vm-spec.json"), "{}").unwrap();
    assert!(rig.host.start(&written).is_err());
    let linked = rig.spec("lease-2-4");
    std::os::unix::fs::symlink(rig.dir.path().join("x"), linked.clone.join("vm-run.log")).unwrap();
    assert!(rig.host.start(&linked).is_err());
    assert!(rig.both_slots_free());

    for call in ["session_ready", "boot_id", "run"] {
        rig.session.fail(call);
        let spec = rig.spec(&format!("lease-3-{call}"));
        assert!(rig.host.start(&spec).is_err(), "{call}");
        rig.session.heal();
        assert!(rig.both_slots_free(), "{call}");
        assert!(rig.records().is_empty(), "{call}: {:?}", rig.records());
    }
    rig.session.set_ready(false);
    let down = rig.host.start(&rig.spec("lease-4-1")).unwrap_err();
    assert!(matches!(down, HostError::Io(ref why) if why.kind() == std::io::ErrorKind::WouldBlock));
    rig.session.set_ready(true);
    assert!(rig.both_slots_free());

    // A pending trace in the way.
    let spec = rig.spec("lease-4-2");
    std::fs::write(rig.runs().join("lease-4-2.pending"), "").unwrap();
    assert!(rig.host.start(&spec).is_err());
    std::fs::remove_file(rig.runs().join("lease-4-2.pending")).unwrap();
    assert!(rig.both_slots_free());

    // The runs directory cannot be made.
    let blocked = rig_with("errors-runs", FakeSession::new(), |_| {});
    std::fs::remove_dir(blocked.runs()).unwrap();
    std::fs::write(blocked.runs(), "").unwrap();
    assert!(blocked.host.start(&blocked.spec("lease-1-1")).is_err());
    assert!(blocked.both_slots_free());
}

/// Catches: a failure after the process exists leaving the VM running or its slot held:
/// the table cannot be read, the record cannot be written, the process is gone as the
/// helper reports it. The control pipe closes on the error path, the VM stops, and the
/// slot comes back when the run has ended, not before.
#[test]
fn a_failure_after_the_run_started_stops_it_and_frees_its_slot() {
    let rig = rig("after-run");
    rig.session.fail("process_start");
    let why = rig.host.start(&rig.spec("lease-1-1")).unwrap_err();
    assert!(matches!(why, HostError::Io(_)));
    rig.session.wait_eof(upid(0));
    rig.settle();
    assert!(rig.both_slots_free());
    assert!(rig.records().is_empty());
    rig.session.heal();

    // The complete record cannot be written: a directory stands where it goes.
    std::fs::create_dir(rig.runs().join("lease-1-2")).unwrap();
    std::fs::write(rig.runs().join("lease-1-2").join("x"), "").unwrap();
    assert!(rig.host.start(&rig.spec("lease-1-2")).is_err());
    rig.session.wait_eof(upid(1));
    rig.settle();
    assert!(rig.both_slots_free());
    assert!(rig.session.saw_eof(upid(1)));

    // The process had ended by the time the host looked.
    let quick = FakeSession::new();
    quick.script(Vec::new());
    let rig = rig_with("after-run-gone", quick, |_| {});
    rig.session.hide_from_table();
    let why = rig.host.start(&rig.spec("lease-1-1")).unwrap_err();
    assert!(
        matches!(why, HostError::Io(ref e) if e.to_string().contains("ended as it started")),
        "{why}"
    );
    rig.session.wait_eof(upid(0));
    rig.settle();
    assert!(rig.both_slots_free());
}

/// Catches: events not reaching `connect` (a stream handed out before the VM started, or
/// after it failed or stopped), the framework's limit error read as an ordinary failure,
/// bytes that do not cross the relay, and a connect to a VM the host does not run.
#[test]
fn connect_relays_over_the_session_only_to_a_started_vm() {
    let session = FakeSession::new();
    session.script(Vec::new());
    session.set_connectable(true);
    let rig = rig_with("connect", session, |_| {});
    let unknown = rig.host.connect("nobody").err().unwrap();
    assert!(matches!(unknown, HostError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound));
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    // No event yet: the guest is not reachable, and the session was not asked.
    assert!(matches!(
        rig.host.connect("lease-1-1"),
        Err(HostError::NotReady)
    ));
    assert_eq!(rig.session.count("connect"), 0);

    // `vm-run` prints Started: the next connect goes through the session.
    let state = rig.state_of("lease-1-1");
    state.apply(Event::Started { ms: 1 });
    let mut stream = rig.host.connect("lease-1-1").unwrap();
    stream.write_all(b"ping").unwrap();
    let mut echoed = [0_u8; 4];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"PING");
    drop(stream);

    // The session cannot relay: that is the connect's error.
    rig.session.set_connectable(false);
    let why = rig.host.connect("lease-1-1").err().unwrap();
    assert!(matches!(why, HostError::Io(ref e) if e.kind() == std::io::ErrorKind::Unsupported));

    // The framework refused the VM: the limit, or another failure.
    state.apply(Event::Error {
        stage: "start".into(),
        message: "no".into(),
        code: Some("Other".into()),
    });
    let failed = rig.host.connect("lease-1-1").err().unwrap();
    assert!(matches!(failed, HostError::Io(ref e) if e.to_string().contains("the VM failed: no")));
    state.apply(Event::Error {
        stage: "start".into(),
        message: "limit".into(),
        code: Some(LIMIT_EXCEEDED.into()),
    });
    let limit = rig.host.connect("lease-1-1").err().unwrap();
    assert!(matches!(limit, HostError::LimitExceeded(n) if n == MACOS_VM_LIMIT));

    // Stopped: closing the control pipe ended the run, and a later connect says so.
    rig.host.runs().get_mut("lease-1-1").unwrap().control = None;
    rig.wait_ended("lease-1-1");
    let stopped = rig.host.connect("lease-1-1").err().unwrap();
    // The failure is reported before the exit.
    assert!(matches!(stopped, HostError::LimitExceeded(_)));
    state.lock().failure = None;
    let stopped = rig.host.connect("lease-1-1").err().unwrap();
    assert!(matches!(stopped, HostError::Io(ref e) if e.to_string().contains("has stopped")));
    rig.host.stop("lease-1-1").unwrap();
}

/// Catches: a helper connection that failed before the run's end being reported as a
/// clean stop: the host must say the run was lost, free the slot, and keep the record for
/// the next start's sweep.
#[test]
fn a_lost_helper_connection_frees_the_slot_and_keeps_the_record() {
    let _log = super::trace();
    let session = FakeSession::new();
    session.set_wait_error(true);
    let rig = rig_with("lost", session, |_| {});
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    rig.host.runs().get_mut("lease-1-1").unwrap().control = None;
    rig.settle();
    assert!(rig.both_slots_free());
    assert_eq!(rig.records(), ["lease-1-1"]);
    let why = rig.host.connect("lease-1-1").err().unwrap();
    assert!(matches!(why, HostError::Io(ref e) if e.to_string().contains("was lost")));
    rig.host.stop("lease-1-1").unwrap();
}

/// Catches: a stop that returns before the run has ended (the slot would still be held),
/// one that never closes the control pipe, a stop of a VM that is not running failing, and
/// one that leaves the record or the entry.
#[test]
fn stop_closes_the_control_pipe_and_returns_once_the_run_has_ended() {
    let rig = rig("stop");
    rig.host.stop("nobody").unwrap();
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    rig.host.stop("lease-1-1").unwrap();
    assert!(rig.session.saw_eof(upid(0)));
    assert_eq!(rig.session.count("reap"), 0);
    assert!(rig.records().is_empty());
    rig.host.stop("lease-1-1").unwrap();
    rig.settle();
    assert!(rig.both_slots_free());
}

/// Catches: a run that ignores its control pipe staying up (the helper must end its
/// group, by the recorded identity), a reap that fails being taken as a stop, a stop that
/// cannot be tried again after it, and a run still up after the reap being taken as
/// stopped.
#[test]
fn a_run_that_ignores_its_control_pipe_is_reaped_by_its_record() {
    let _log = super::trace();
    let session = FakeSession::new();
    session.set_stubborn(true);
    let rig = rig_with("reap", session, |config| config.stop_grace = Duration::ZERO);
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    rig.host.stop("lease-1-1").unwrap();
    let reaped = rig.session.reaped();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].pgid, pid(0));
    assert_eq!(reaped[0].boot, "boot-1");
    assert!(rig.records().is_empty());

    // The helper refuses: the run stays known, and a second stop tries again.
    rig.session.fail("reap");
    rig.host.start(&rig.spec("lease-1-2")).unwrap();
    assert!(rig.host.stop("lease-1-2").is_err());
    assert!(rig.host.runs().contains_key("lease-1-2"));
    rig.session.heal();
    rig.host.stop("lease-1-2").unwrap();
    assert!(!rig.host.runs().contains_key("lease-1-2"));
    rig.settle();
    assert!(rig.both_slots_free());
}

/// Catches: a reap that returns and a run that is still up being taken as stopped (its
/// slot would be free while its VM runs).
#[test]
fn a_run_still_up_after_the_reap_is_an_error() {
    let session = FakeSession::new();
    session.set_stubborn(true);
    session.set_reap_kills(false);
    let rig = rig_with("reap-fails", session, |config| {
        config.stop_grace = Duration::ZERO;
        config.reap_grace = Duration::ZERO;
    });
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    let why = rig.host.stop("lease-1-1").unwrap_err();
    assert!(why.to_string().contains("has not ended"), "{why}");
    assert!(!rig.both_slots_free());
    rig.session.kill(upid(0));
    rig.settle();
    rig.host.stop("lease-1-1").unwrap();
    assert!(rig.both_slots_free());
}

/// Catches: a daemon that dies without stopping its VMs leaving them running: its control
/// pipes close with it, and each `vm-run` sees the end of its stdin.
#[test]
fn the_daemon_dying_closes_the_control_pipe() {
    let rig = rig("death");
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    assert!(!rig.session.saw_eof(upid(0)));
    let Rig {
        session, host, dir, ..
    } = rig;
    // Dropping the host closes what the kernel closes when the process dies.
    drop(host);
    session.wait_eof(upid(0));
    drop(dir);
}

/// Catches: the start-up sweep not running at construction, or running on a host that is
/// not the one that serves (the records of earlier daemons are ended once, before any
/// start).
#[test]
fn a_new_host_ends_the_runs_an_earlier_daemon_recorded() {
    let session = FakeSession::new();
    session.leave_process(777, 5555);
    let dir = super::dir("new-sweeps");
    let mut config = super::config(&dir);
    std::fs::create_dir_all(config.runs()).unwrap();
    std::fs::write(
        config.runs().join("lease-1-1"),
        Record {
            pgid: 777,
            start: 5555,
            boot: "boot-1".to_owned(),
        }
        .line(),
    )
    .unwrap();
    config.stop_grace = super::LONG;
    let host = crate::mac::MacVmHost::new(config, session.clone()).unwrap();
    assert_eq!(host.swept().reaped.len(), 1);
    assert!(!session.alive(777));
    assert_eq!(session.count("reap"), 1);
    // The image is cloned through the session.
    let image = dir.path().join("image");
    std::fs::create_dir(&image).unwrap();
    std::fs::write(image.join("disk.img"), "x").unwrap();
    host.clone_image(&image, &dir.path().join("clone")).unwrap();
    assert!(dir.path().join("clone/disk.img").is_file());
    assert!(host.clone_image(&image, &dir.path().join("clone")).is_err());
}

/// Catches: a VM path that is not text being passed to the helper lossily (the helper
/// would run another file), and a slot kept by that refusal.
#[test]
fn a_vmm_path_that_is_not_text_is_refused() {
    use std::os::unix::ffi::OsStringExt as _;
    let rig = rig_with("not-utf8", FakeSession::new(), |config| {
        config.vmm = std::ffi::OsString::from_vec(vec![b'/', 0xff, b'v']).into();
    });
    let why = rig.host.start(&rig.spec("lease-1-1")).unwrap_err();
    assert!(why.to_string().contains("is not UTF-8"), "{why}");
    assert_eq!(rig.session.count("run"), 0);
    assert!(rig.both_slots_free());
    assert!(rig.records().is_empty());
}

/// Catches: a helper that refuses to end a left run (certain every time, when the process
/// is not the VM user's) failing `new`, so the daemon could never start until someone
/// deleted the record by hand; and the record being dropped, so the run is forgotten.
#[test]
fn a_run_the_helper_will_not_end_does_not_lock_the_daemon_out() {
    let session = FakeSession::new();
    session.leave_process(777, 5555);
    session.fail("reap");
    let dir = super::dir("sweep-refused");
    let config = super::config(&dir);
    std::fs::create_dir_all(config.runs()).unwrap();
    let left = Record {
        pgid: 777,
        start: 5555,
        boot: "boot-1".to_owned(),
    };
    std::fs::write(config.runs().join("lease-1-1"), left.line()).unwrap();
    let host = crate::mac::MacVmHost::new(config.clone(), session.clone()).unwrap();
    assert!(host.swept().reaped.is_empty());
    assert_eq!(host.swept().failed.len(), 1);
    assert_eq!(host.swept().failed[0].0, left);
    assert!(session.alive(777));
    assert!(config.runs().join("lease-1-1").is_file());
    drop(host);
    // The next start tries again, and ends it once the helper will.
    session.heal();
    let host = crate::mac::MacVmHost::new(config.clone(), session.clone()).unwrap();
    assert_eq!(host.swept().reaped, [left]);
    assert!(!session.alive(777));
    assert!(!config.runs().join("lease-1-1").exists());
}

/// Catches: a second daemon on the same state directory finding the first one's live
/// record and having the helper kill its running VM (and deleting its traces).
#[test]
fn a_second_daemon_on_one_state_directory_leaves_the_first_ones_vms_alone() {
    let rig = rig("two-daemons");
    rig.host.start(&rig.spec("lease-1-1")).unwrap();
    assert_eq!(rig.records(), ["lease-1-1"]);
    let second = crate::mac::MacVmHost::new(super::config(&rig.dir), rig.session.clone());
    let why = second.err().expect("the directory is owned");
    assert_eq!(why.kind(), std::io::ErrorKind::WouldBlock, "{why}");
    assert!(why.to_string().contains("one daemon per Mac"), "{why}");
    assert_eq!(rig.session.count("reap"), 0);
    assert!(rig.session.alive(pid(0)));
    assert_eq!(rig.records(), ["lease-1-1"]);
    assert!(!rig.session.saw_eof(upid(0)));
    let Rig {
        session, host, dir, ..
    } = rig;
    // Once the first daemon is gone, its record is the next one's to sweep.
    drop(host);
    session.wait_eof(upid(0));
    let next = crate::mac::MacVmHost::new(super::config(&dir), session.clone());
    assert!(next.is_ok());
    drop(next);
}

/// Catches: an ownership lock that cannot be taken at all (a state path that is a file).
#[test]
fn a_state_directory_that_cannot_be_made_is_an_error() {
    let dir = super::dir("owner-bad");
    std::fs::write(dir.path().join("state"), "").unwrap();
    let config = super::config(&dir);
    assert!(crate::mac::MacVmHost::new(config, FakeSession::new()).is_err());
}
