//! The pieces of the macOS host, one at a time: slots, pipes, the spec, the records and
//! the event reader.

use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::PermissionsExt as _;

use kbf_driver_native::Record;
use kbf_vmm::spec::{Event, LIMIT_EXCEEDED};

use super::{FakeSession, dir};
use crate::host::NetworkDevice;
use crate::mac::pipes::Pipes;
use crate::mac::slots::SlotLock;
use crate::mac::{RunState, read_events, records, vmspec};

/// Catches: a third VM getting a slot (the licence and the framework allow two), a slot
/// that is not given back when its holder is dropped, two holders sharing a slot, and a
/// slot directory or file that cannot be used reading as "all slots taken".
#[test]
fn two_slots_are_held_apart_and_given_back() {
    let dir = dir("slots");
    let slots = dir.path().join("slots");
    let first = SlotLock::acquire(&slots, 2).unwrap().unwrap();
    let second = SlotLock::acquire(&slots, 2).unwrap().unwrap();
    assert_eq!((first.index(), second.index()), (0, 1));
    assert!(SlotLock::acquire(&slots, 2).unwrap().is_none());
    drop(first);
    let again = SlotLock::acquire(&slots, 2).unwrap().unwrap();
    assert_eq!(again.index(), 0);
    assert!(SlotLock::acquire(&slots, 2).unwrap().is_none());
    drop((second, again));
    assert!(SlotLock::acquire(&slots, 2).unwrap().is_some());

    // A slot file that is a link is not followed; a directory that cannot be made is an
    // error, not a refusal.
    let linked = dir.path().join("linked");
    std::fs::create_dir(&linked).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), linked.join("slot-0.lock")).unwrap();
    assert!(SlotLock::acquire(&linked, 2).is_err());
    std::fs::write(dir.path().join("file"), "").unwrap();
    assert!(SlotLock::acquire(&dir.path().join("file/slots"), 2).is_err());
}

/// Catches: a control pipe whose write end has a second holder (the helper's side or a
/// descriptor inherited across exec), so closing the daemon's end would not stop the VM;
/// an event pipe whose write end the daemon keeps, so the reader never sees the run end.
#[test]
fn the_daemon_s_write_ends_are_the_only_ones_once_the_helper_has_its_own() {
    let pipes = Pipes::new().unwrap();
    for fd in [
        pipes.control.as_fd(),
        pipes.helper.stdin.as_fd(),
        pipes.helper.stdout.as_fd(),
        std::os::fd::AsFd::as_fd(&pipes.events),
    ] {
        let flags = rustix::io::fcntl_getfd(fd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
    let Pipes {
        control,
        mut events,
        helper,
    } = pipes;
    let mut stdin = std::fs::File::from(helper.stdin);
    let mut stdout = std::fs::File::from(helper.stdout);

    // Bytes go through, then closing the one write end of each ends it.
    let mut control = std::fs::File::from(control);
    control.write_all(b"shutdown\n").unwrap();
    drop(control);
    let mut seen = String::new();
    stdin.read_to_string(&mut seen).unwrap();
    assert_eq!(seen, "shutdown\n");

    stdout.write_all(b"{}\n").unwrap();
    drop(stdout);
    let mut text = String::new();
    events.read_to_string(&mut text).unwrap();
    assert_eq!(text, "{}\n");
}

fn spec() -> crate::host::VmSpec {
    crate::host::VmSpec {
        name: "lease-1-1".to_owned(),
        clone: "/images/clones/lease-1-1".into(),
        machine_id: crate::host::MachineId([1; 16]),
        cpus: 6,
        memory_bytes: (16 << 30) + 12345,
        shares: vec![
            crate::host::Share {
                tag: crate::host::INPUTS_TAG,
                host: "/leases/in".into(),
                read_only: true,
            },
            crate::host::Share {
                tag: crate::host::OUTPUTS_TAG,
                host: "/leases/out".into(),
                read_only: false,
            },
        ],
        network: Vec::new(),
    }
}

/// Catches: a driver decision lost on the way to `vm-run` (the clone, the cores, the
/// memory in MiB, the tag, path and read-only flag of each share), a VM without graphics
/// (its window server crash-loops), and a network device dropped silently instead of
/// refused.
#[test]
fn the_drivers_spec_becomes_the_vm_run_spec() {
    let out = vmspec::translate(&spec()).unwrap();
    assert_eq!(out.bundle.to_str(), Some("/images/clones/lease-1-1"));
    assert_eq!(
        (out.cpus, out.memory_mib, out.graphics),
        (6, 16 * 1024, true)
    );
    let shares: Vec<_> = out
        .tagged
        .iter()
        .map(|s| (s.tag.as_str(), s.path.to_str().unwrap(), s.read_only))
        .collect();
    assert_eq!(
        shares,
        [
            ("inputs", "/leases/in", true),
            ("outputs", "/leases/out", false)
        ]
    );
    assert!(out.vsock_ports.is_empty() && out.console_ports.is_empty());
    assert!(out.automount_dir.is_none() && out.results_dir.is_none());

    let mut network = spec();
    network.network = vec![NetworkDevice::Nat];
    let why = vmspec::translate(&network).unwrap_err();
    assert_eq!(why.kind(), std::io::ErrorKind::Unsupported);
    let mut small = spec();
    small.memory_bytes = (1 << 20) - 1;
    assert_eq!(
        vmspec::translate(&small).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

/// Catches: a spec file `vm-run` cannot parse (it must load as kbf-vmm's own type), one
/// the VM user cannot read, and one written over an existing file or through a link.
#[test]
fn the_spec_file_loads_in_vm_run_and_is_never_overwritten() {
    let dir = dir("vmspec");
    let mut spec = spec();
    spec.clone = dir.path().to_owned();
    let file = vmspec::write(&spec).unwrap();
    assert_eq!(file, dir.path().join(vmspec::SPEC_FILE));
    let loaded: kbf_vmm::spec::VmSpec =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(loaded, vmspec::translate(&spec).unwrap());
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
    assert_eq!(
        vmspec::write(&spec).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(dir.path().join("target"), &file).unwrap();
    assert!(vmspec::write(&spec).is_err());
    spec.network = vec![NetworkDevice::Nat];
    assert!(vmspec::write(&spec).is_err());
}

fn record(pgid: i32, start: u64, boot: &str) -> Record {
    Record {
        pgid,
        start,
        boot: boot.to_owned(),
    }
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches: a pending record that stays after the complete one, a complete record that
/// is not the line the native driver's records use, a finish that leaves a file or
/// fails on one already gone, and a record written over another.
#[test]
fn a_record_is_pending_then_complete_then_gone() {
    let dir = dir("records");
    let runs = dir.path().join("runs");
    records::begin(&runs, "lease-1-1", "boot-1").unwrap();
    assert_eq!(names(&runs), ["lease-1-1.pending"]);
    assert!(records::begin(&runs, "lease-1-1", "boot-1").is_err());
    records::complete(&runs, "lease-1-1", &record(4000, 99, "boot-1")).unwrap();
    assert_eq!(names(&runs), ["lease-1-1"]);
    let text = std::fs::read_to_string(runs.join("lease-1-1")).unwrap();
    assert_eq!(Record::parse(&text), Some(record(4000, 99, "boot-1")));
    records::finish(&runs, "lease-1-1");
    records::finish(&runs, "lease-1-1");
    assert!(names(&runs).is_empty());
    // A directory in the way cannot be removed; that is logged, not fatal.
    std::fs::create_dir(runs.join("stuck")).unwrap();
    records::finish(&runs, "stuck");
    assert_eq!(names(&runs), ["stuck"]);
}

/// Catches: the sweep killing on a record that is not the same process in the same boot
/// (pid reused: another start time; another boot; a process gone), not killing the one
/// that is, leaving a record behind, or treating junk (a link, a directory, a long or
/// half-written line, a pending trace) as a run to end.
#[test]
fn the_sweep_ends_the_recorded_run_and_nothing_else() {
    let _log = super::trace();
    let dir = dir("sweep");
    let runs = dir.path().join("runs");
    std::fs::create_dir_all(&runs).unwrap();
    let session = FakeSession::new();
    session.set_boot("boot-2");
    for (pid, start) in [(500, 1000), (501, 1001), (502, 1002), (503, 1003)] {
        session.leave_process(pid, start);
    }
    let put = |name: &str, record: &Record| std::fs::write(runs.join(name), record.line()).unwrap();
    put("a-match", &record(500, 1000, "boot-2"));
    // The pid was reused: the process there started later.
    put("b-reused", &record(501, 7, "boot-2"));
    put("c-other-boot", &record(502, 1002, "boot-1"));
    put("d-gone", &record(599, 1, "boot-2"));
    std::fs::write(
        runs.join("e-two-lines"),
        "503 1003 boot-2\n503 1003 boot-2\n",
    )
    .unwrap();
    std::fs::write(runs.join("f-junk"), "not a record").unwrap();
    std::fs::write(runs.join("g-long"), "x".repeat(10_000)).unwrap();
    std::fs::write(runs.join("h.pending"), "boot-2\n").unwrap();
    std::fs::write(runs.join("i.part"), "503 1003").unwrap();
    std::fs::create_dir(runs.join("j-dir")).unwrap();
    std::os::unix::fs::symlink(runs.join("a-match"), runs.join("k-link")).unwrap();

    let swept = records::sweep(&runs, &session).unwrap();
    assert_eq!(swept.reaped, [record(500, 1000, "boot-2")]);
    assert_eq!(swept.stale, 10);
    assert_eq!(session.reaped(), [record(500, 1000, "boot-2")]);
    assert!(!session.alive(500));
    for pid in [501, 502, 503] {
        assert!(session.alive(pid), "{pid} was not the recorded run");
    }
    // Everything but the directory (which cannot be removed as a file) is gone.
    assert_eq!(names(&runs), ["j-dir"]);
}

/// Catches: a sweep that stops the daemon over nothing (no directory yet), one that
/// goes on after a failure it cannot recover from, and a kernel question that failed read
/// as "no".
#[test]
fn the_sweep_fails_loudly_when_it_cannot_account_for_a_run() {
    let dir = dir("sweep-fail");
    let runs = dir.path().join("runs");
    let session = FakeSession::new();
    assert_eq!(
        records::sweep(&runs, &session).unwrap(),
        records::Swept::default()
    );
    std::fs::write(dir.path().join("file"), "").unwrap();
    assert!(records::sweep(&dir.path().join("file"), &session).is_err());

    std::fs::create_dir_all(&runs).unwrap();
    std::fs::write(runs.join("a"), record(500, 1000, "boot-1").line()).unwrap();
    session.leave_process(500, 1000);
    session.heal();
    session.fail("process_start");
    assert!(records::sweep(&runs, &session).is_err());
    session.heal();
    session.fail("boot_id");
    assert!(records::sweep(&runs, &session).is_err());
    session.heal();
    assert_eq!(records::sweep(&runs, &session).unwrap().reaped.len(), 1);
    assert!(names(&runs).is_empty());
}

fn feed(text: &[u8]) -> RunState {
    let dir = dir("events");
    let file = dir.path().join("events");
    std::fs::write(&file, text).unwrap();
    let state = RunState::default();
    read_events(std::fs::File::open(&file).unwrap(), &state);
    state
}

/// Catches: events not reaching the progress (the start, the framework's error with its
/// code), junk lines stopping the reader, an over-long line read whole or taking the
/// reader's place in the stream (the line after it must still be read), and a last line
/// with no newline taken for a whole event.
#[test]
fn the_reader_keeps_the_events_that_matter_and_survives_the_rest() {
    let line = |e: Event| format!("{}\n", e.to_line());
    let mut text = Vec::new();
    text.extend_from_slice(b"not json\n");
    text.extend_from_slice(&[0xff, 0xfe, b'\n']);
    text.extend_from_slice(line(Event::Configured { ms: 1 }).as_bytes());
    text.extend_from_slice(&vec![b'x'; 200_000]);
    text.push(b'\n');
    text.extend_from_slice(line(Event::Started { ms: 2 }).as_bytes());
    text.extend_from_slice(
        line(Event::Error {
            stage: "start".into(),
            message: "too many".into(),
            code: Some(LIMIT_EXCEEDED.into()),
        })
        .as_bytes(),
    );
    text.extend_from_slice(line(Event::Stopped { ms: 3 }).as_bytes());
    // Half an event at the end of the stream.
    text.extend_from_slice(br#"{"event":"stopped""#);
    let state = feed(&text);
    let progress = state.lock();
    assert!(progress.started);
    let failure = progress.failure.clone().unwrap();
    assert_eq!(failure.code.as_deref(), Some(LIMIT_EXCEEDED));
    assert_eq!(failure.message, "too many");
    assert!(progress.ended.is_none());

    // An over-long line cut at the end of the stream, and an empty stream.
    let state = feed(&vec![b'y'; 100_000]);
    assert!(!state.lock().started);
    assert!(!feed(b"").lock().started);
    // A line of exactly the bound, then a valid event.
    let mut exact = vec![b'z'; crate::mac::MAX_LINE as usize - 1];
    exact.push(b'\n');
    exact.extend_from_slice(line(Event::Started { ms: 1 }).as_bytes());
    assert!(feed(&exact).lock().started);
}
