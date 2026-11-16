use super::*;

/// Catches: a lease past its memory limit getting a grace in which its runaway
/// processes go on growing, and an exit, a timeout or a cancel losing the grace.
#[test]
fn a_lease_past_its_memory_limit_gets_no_grace() {
    let grace = Duration::from_secs(2);
    let exited = Ended::Exited(Ok(ExitStatus::default()));
    assert_eq!(grace_after(&exited, grace), grace);
    assert_eq!(grace_after(&Ended::TimedOut, grace), grace);
    assert_eq!(grace_after(&Ended::Killed, grace), grace);
    let oom = Ended::OutOfMemory { used: 2, limit: 1 };
    assert_eq!(grace_after(&oom, grace), Duration::ZERO);
}

/// Catches spool files a killed daemon left filling the disk, and a start refused
/// over an entry it cannot remove: a leftover file goes, a directory that is not a
/// spool stays (logged), and the spool directory is the daemon's alone.
#[test]
fn the_spool_directory_is_made_and_its_leftovers_removed() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join(format!("kbf-driver-native-spool-{}", std::process::id()));
    let spool = dir.join(SPOOL);
    std::fs::create_dir_all(spool.join("not-a-spool")).expect("mkdir");
    std::fs::write(spool.join(".kbf-spool-1-0"), b"left").expect("write");
    make_spool_dir(&spool).expect("spool directory");
    let mut left: Vec<String> = std::fs::read_dir(&spool)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["not-a-spool"]);
    std::fs::remove_dir_all(&dir).expect("clean up");
    // Made fresh, it is the daemon's alone.
    make_spool_dir(&spool).expect("spool directory");
    let fresh = std::fs::metadata(&spool)
        .expect("stat")
        .permissions()
        .mode();
    std::fs::remove_dir_all(&dir).expect("clean up");
    assert_eq!(fresh & 0o777, 0o700);
}

/// Catches an action's timeout being ignored, a zero timeout meaning "no time at
/// all", and a negative one wrapping to centuries.
#[test]
fn the_timeout_is_the_actions_or_the_default() {
    let default = Duration::from_secs(60);
    let with = |seconds, nanos| Action {
        timeout: Some(prost_types::Duration { seconds, nanos }),
        ..Action::default()
    };
    assert_eq!(timeout_of(&Action::default(), default).ok(), Some(default));
    assert_eq!(timeout_of(&with(0, 0), default).ok(), Some(default));
    assert_eq!(
        timeout_of(&with(2, 500), default).ok(),
        Some(Duration::new(2, 500))
    );
    assert!(matches!(
        timeout_of(&with(-1, 0), default),
        Err(RuntimeError::Invalid(_))
    ));
}

/// Catches a clean whose blocking task panicked reported without saying it was the
/// clean that failed (the lease then fails INTERNAL with this text).
#[tokio::test]
async fn a_clean_task_that_did_not_finish_names_the_clean() {
    let panicked = tokio::task::spawn_blocking(|| panic!("clean panicked"))
        .await
        .expect_err("the task panicked");
    assert!(clean_task_failed(panicked).starts_with("clean task: "));
}

/// A CAS no test here reaches.
#[derive(Debug)]
struct NoCas;

impl Cas for NoCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &kbf_proto::reapi::Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Missing(digest.hash.clone()))
    }

    async fn put(&self, _: Vec<u8>) -> Result<kbf_proto::reapi::Digest, CasError> {
        Err(CasError::Unavailable("-".into(), "no CAS".into()))
    }
}

/// Catches the runtime refusing to start over a leftover lease directory it
/// cannot remove (one build step would take the node out of the farm): the start
/// goes on, and the leftover is out of the lease names' way.
#[tokio::test]
async fn a_start_goes_on_past_a_leftover_that_will_not_go() {
    let scratch = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-driver-native-unit")
        .join(format!("start-{}", std::process::id()));
    let _ = kbf_outputs::remove_tree(&scratch);
    std::fs::create_dir_all(scratch.join("lease-3-3/root")).expect("mkdir");
    let refuse = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM));
    let config = NativeConfig::new(scratch.clone());
    let started = NativeRuntime::with_remover(config, Arc::new(NoCas), &refuse);
    let started = started.expect("the start goes on");
    let digest = kbf_proto::reapi::Digest::default();
    // A lease reads through the same store.
    let lease = started.cas.lease(&LeaseId::new(3, 3));
    assert!(lease.get(&digest).await.is_err(), "no CAS here");
    assert!(lease.put(Vec::new()).await.is_err(), "no CAS here");
    assert!(!scratch.join("lease-3-3").exists(), "moved out of the way");
    let aside = std::fs::read_dir(scratch.join(crate::sweep::QUARANTINE))
        .expect("quarantine")
        .count();
    assert_eq!(aside, 1);
    kbf_outputs::remove_tree(&scratch).expect("clean");
}

/// Catches a daemon running as root starting (it would run every action as root):
/// the check refuses it with the reason, and lets any other user through.
#[test]
fn a_daemon_running_as_root_does_not_start() {
    let refused = check_daemon_user(true).expect_err("root");
    assert!(refused.contains("runs as root"), "{refused}");
    assert_eq!(check_daemon_user(false), Ok(()));
}
/// A process table with one process, above any kernel's pid limit, that never dies.
fn undying() -> std::io::Result<Vec<Proc>> {
    Ok(vec![Proc {
        pid: 2_000_000_005,
        ppid: 1,
        pgid: 2_000_000_005,
        start: 5,
        zombie: false,
    }])
}

fn nothing() -> std::io::Result<Vec<Proc>> {
    Ok(Vec::new())
}

/// Catches a dropped run that removes its run record although processes of it
/// survived SIGKILL (the next start could not find them), and one that keeps the
/// record once they have all ended.
#[test]
fn a_dropped_run_keeps_its_record_only_while_processes_survive() {
    let dir = crate::unit_dir("runtime-group");
    let record = dir.join("lease-5-5");
    std::fs::write(&record, "x").expect("write");
    let me = i32::try_from(std::process::id()).expect("pid");
    let tracker = || Tracker::new(2_000_000_005, me);
    drop(Group::reading(tracker(), record.clone(), undying));
    assert!(record.exists(), "kept for the next start's sweep");
    drop(Group::reading(tracker(), record.clone(), nothing));
    assert!(!record.exists(), "removed once nothing survives");
}

/// Reads the process table, except that its first read fails: the SIGTERM round's.
fn table_fails_once() -> std::io::Result<Vec<Proc>> {
    static READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
        Err(std::io::Error::other("table"))
    } else {
        procs::snapshot()
    }
}

/// Catches an action that outlives its lease because the SIGTERM round failed (the
/// process table could not be read): the end must still SIGKILL it, succeed, and
/// name no survivor. The leader is a real `sleep`, so only the kill ends it.
#[tokio::test]
async fn the_kill_runs_when_the_sigterm_round_fails() {
    use std::os::unix::process::ExitStatusExt as _;
    let dir = crate::unit_dir("runtime-group-term-fails");
    let record = dir.join("lease-6-6");
    std::fs::write(&record, "x").expect("write");
    use std::os::unix::process::CommandExt as _;
    let mut child = Leader::new(
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .expect("spawn sleep"),
    );
    let leader = child.pid();
    let me = i32::try_from(std::process::id()).expect("pid");
    let mut group = Group::reading(Tracker::new(leader, me), record, table_fails_once);
    let survivors = group
        .end(&mut child, Duration::from_secs(10), Duration::from_secs(2))
        .await
        .expect("ended");
    assert!(survivors.is_empty());
    let status = child.wait().await.expect("reaped").status;
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
}

/// Catches a signal death reported as exit 0 or as the raw status.
#[test]
fn a_signal_is_128_plus_its_number() {
    assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
    assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGKILL)), 128 + 9);
}

/// Catches errors reported as the wrong party's: a missing input or a bad path is
/// the client's, anything else the farm's. (Compared as text: a pattern guard here
/// would be a branch the coverage ratchet counts and no run takes.)
#[test]
fn errors_are_reported_as_whose_they_are() {
    let shown = |e: RuntimeError| format!("{e:?}");
    let missing = tree_error(TreeError::Cas(CasError::Missing("ab/1".to_owned())));
    assert_eq!(shown(missing), "MissingBlob(\"ab/1\")");
    let corrupt = tree_error(TreeError::Cas(CasError::Corrupt("a".into(), "b".into())));
    assert_eq!(
        shown(corrupt),
        "Failed(\"blob a failed verification: its bytes hash to b\")"
    );
    assert_eq!(
        shown(tree_error(TreeError::Invalid("x".into()))),
        "Invalid(\"x\")"
    );
    assert_eq!(
        shown(outputs_error(OutputsError::Invalid("y".into()))),
        "Invalid(\"y\")"
    );
    let io = OutputsError::Io {
        path: PathBuf::from("p"),
        source: std::io::Error::other("disk"),
    };
    assert_eq!(shown(outputs_error(io)), "Failed(\"p: disk\")");
}

/// Catches a spawn that gives up at once on a program still open for writing (a
/// just-materialised input another fork briefly holds), instead of retrying.
#[cfg(target_os = "linux")]
#[tokio::test(start_paused = true)]
async fn a_busy_program_is_retried() {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = crate::unit_dir("runtime-busy");
    let program = dir.join("busy");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&program)
        .expect("create");
    // Its interpreter is the act steps' link: run, it echoes its own path.
    writeln!(file, "#!{} echo", crate::act().display()).expect("write");
    let closer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(file);
    });
    let command = || {
        let mut command = tokio::process::Command::new(&program);
        command.stdout(Stdio::piped());
        command
    };
    let exec = || Exec::new(&program, [], []).expect("exec");
    let record = dir.join(format!("busy-record-{}", std::process::id()));
    let paused_at = tokio::time::Instant::now();
    // The file was open for writing when the spawn began, so it spawned only after the
    // closer let go of it at 100 ms of the paused clock: it retried.
    let Spawned { child, pid, .. } = spawn(&mut command(), exec(), &record, KeeperFor::Group)
        .await
        .expect("spawned once closed");
    assert!(tokio::time::Instant::now() - paused_at >= Duration::from_millis(100));
    assert!(pid > 0);
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", program.display()),
        "not the program that was busy"
    );
    // As the lease's end does: a record is written only where none is.
    std::fs::remove_file(&record).expect("the run record");
    closer.await.expect("closer");
    // Held open for writing past the wait: the spawn gives up with ETXTBSY.
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(&program)
        .expect("open");
    let busy = spawn(&mut command(), exec(), &record, KeeperFor::Group)
        .await
        .expect_err("still busy");
    assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
    drop(held);
    // Any other failure is not retried: a file that is no program at all.
    std::fs::write(&program, [0x7f, b'E', b'L', b'F', 0, 0]).expect("write");
    let before = tokio::time::Instant::now();
    let error = spawn(&mut command(), exec(), &record, KeeperFor::Group)
        .await
        .expect_err("not a program");
    assert_ne!(error.raw_os_error(), Some(libc::ETXTBSY));
    // No retry pause passed on the paused clock.
    assert_eq!(tokio::time::Instant::now(), before);
    assert!(!record.exists(), "a failed spawn leaves no run record");
    std::fs::remove_file(&program).expect("remove");
}

/// Catches a note added to an action's stderr that pushes the stderr past the stdio limit
/// (all of it would then be dropped), or a stderr that cannot be appended to failing the
/// lease: a note that fits is appended; one that does not fit leaves the stderr as it was;
/// a stderr that is a directory is left alone, and the note stays in the node's log.
#[test]
fn a_note_is_added_to_stderr_only_where_it_fits_and_can_be_written() {
    let scratch = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-driver-native-unit")
        .join(format!("stderr-note-{}", std::process::id()));
    let _ = kbf_outputs::remove_tree(&scratch);
    std::fs::create_dir_all(&scratch).expect("mkdir");
    let keep = |_: &Path| Ok(());
    let mut config = NativeConfig::new(scratch.join("root"));
    config.outputs.max_stdio_bytes = 4;
    let runtime = NativeRuntime::with_remover(config, Arc::new(NoCas), &keep).expect("runtime");
    let lease = LeaseId::new(5, 1);
    let dir = scratch.join("lease");
    std::fs::create_dir_all(&dir).expect("lease dir");
    std::fs::write(dir.join("stderr"), b"x").expect("stderr");
    runtime.add_to_stderr(lease, &dir, "ab", "a note");
    assert_eq!(std::fs::read(dir.join("stderr")).expect("read"), b"xab");
    runtime.add_to_stderr(lease, &dir, "cd", "a note");
    assert_eq!(std::fs::read(dir.join("stderr")).expect("read"), b"xab");

    let mut config = NativeConfig::new(scratch.join("root"));
    config.outputs.max_stdio_bytes = 1 << 30;
    let runtime = NativeRuntime::with_remover(config, Arc::new(NoCas), &keep).expect("runtime");
    let odd = scratch.join("odd");
    std::fs::create_dir_all(odd.join("stderr")).expect("stderr as a directory");
    runtime.add_to_stderr(lease, &odd, "ab", "a note");
    assert!(odd.join("stderr").is_dir(), "left alone");
    kbf_outputs::remove_tree(&scratch).expect("clean");
}
