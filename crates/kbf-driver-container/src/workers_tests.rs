//! The driver's blocking steps run on tokio's blocking pool, never on an async worker.
//!
//! Each step reads or writes files of the node: a cgroup file can wait on the kernel (the
//! statistics of a busy cgroup are flushed under a lock, a journal commit can hold a
//! write), and so can a log file or the pressure file. Done on an async worker, it holds
//! that worker, and with a node's few workers every task of the process, the daemon's
//! heartbeat among them, waits behind it.
//!
//! Each test makes the file the step first touches a named pipe nobody serves: the call
//! cannot finish until the test lets it. The test opens the other end of the pipe, which
//! returns only once the call has opened its end, so the call is known to be waiting in
//! the kernel. It then counts the threads the runtime's blocking pool has started. Two
//! calls wait at once on a runtime of two workers; if both waited on the workers, the
//! pool has started no thread. Each runtime names its threads with a prefix of its own,
//! so the tests count only their own when they run side by side.
//!
//! The digest of the outputs' Directory messages runs on the CPU pool, and its test is
//! in `outputs_tests`.

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kbf_types::Resources;
use rustix::fs::{CWD, FileType, Mode};

use crate::blocking_steps::{create_logs, make_cgroup, oom_outcome, read_start, usage};
use crate::cgroup::{LeaseCgroup, presses_into_swap};
use crate::cgroup_usage::{NumaSample, Start};

const WORKERS: usize = 2;

/// How many runtimes the tests have built, for their thread name prefixes.
static RUNTIMES: AtomicUsize = AtomicUsize::new(0);

/// How many threads named `<prefix><n>` of this process have `n` of `WORKERS` or more:
/// the threads the blocking pool of the runtime with that prefix has started.
fn blocking_pool_threads(prefix: &str) -> usize {
    let tasks = std::fs::read_dir("/proc/self/task").expect("threads");
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .filter_map(|name| name.trim().strip_prefix(prefix)?.parse::<usize>().ok())
        .filter(|&n| n >= WORKERS)
        .count()
}

fn fifo(path: &Path) {
    rustix::fs::mknodat(CWD, path, FileType::Fifo, Mode::from_raw_mode(0o600), 0).expect("mkfifo");
}

/// Opens the pipe `path` for writing: for a call that reads it.
fn write_end(path: &Path) -> File {
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("a read opened the pipe")
}

/// Opens the pipe `path` for reading: for a call that writes it.
fn read_end(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .open(path)
        .expect("a write opened the pipe")
}

/// Starts `WORKERS` calls on a runtime of `WORKERS` async workers, each given the
/// directory `<name>/<n>` in which the file `blocked` is a pipe (the other files a
/// cgroup has are plain files), waits until every call waits on its pipe, and returns how
/// many threads the runtime's blocking pool has started then. `meet` opens the other end
/// of the pipe, and `call` is the work under test.
fn blocking_threads_while_calls_wait<F, Fut>(
    name: &str,
    blocked: &str,
    meet: fn(&Path) -> File,
    call: F,
) -> usize
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let scratch = crate::unit_dir(name);
    // The runtime starts its workers when it is built, so the first `WORKERS` names go to
    // them and any later name to a thread of the blocking pool. A thread name is at most
    // 15 bytes long.
    let prefix = format!("kw{}-", RUNTIMES.fetch_add(1, Ordering::Relaxed));
    let named = AtomicUsize::new(0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name_fn({
            let prefix = prefix.clone();
            move || format!("{prefix}{}", named.fetch_add(1, Ordering::Relaxed))
        })
        .enable_all()
        .build()
        .expect("runtime");
    let dirs: Vec<PathBuf> = (0..WORKERS)
        .map(|n| {
            let dir = scratch.join(n.to_string());
            std::fs::create_dir_all(&dir).expect("mkdir");
            for (file, text) in [
                ("memory.events", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n"),
                ("memory.current", "1\n"),
                ("memory.swap.current", "0\n"),
                ("memory.stat", "anon 1\n"),
                ("cpu.stat", "usage_usec 1\n"),
                ("cpu.pressure", "some avg10=1.00 avg60=0 avg300=0 total=0\n"),
                ("memory.events.local", "oom 0\n"),
            ] {
                std::fs::write(dir.join(file), text).expect("file");
            }
            let _ = std::fs::remove_file(dir.join(blocked));
            fifo(&dir.join(blocked));
            dir
        })
        .collect();
    let tasks: Vec<_> = dirs.iter().map(|dir| rt.spawn(call(dir.clone()))).collect();
    // Returns once the call has opened the pipe: it waits for its other end from here.
    let held: Vec<_> = dirs.iter().map(|dir| meet(&dir.join(blocked))).collect();
    let started = blocking_pool_threads(&prefix);
    // Let every call finish, and every later one find a plain file.
    for dir in &dirs {
        let plain = dir.join("plain");
        std::fs::write(&plain, "").expect("plain");
        std::fs::rename(&plain, dir.join(blocked)).expect("replace");
    }
    drop(held);
    for task in &tasks {
        task.abort();
    }
    drop(rt);
    started
}

/// The failure message when `started` threads are too few.
fn rest_waited(started: usize) -> String {
    format!(
        "two calls waited at once and the blocking pool started {started} threads: \
         the rest waited on the async workers"
    )
}

/// The lease cgroup `dir`, as the swap watch and the OOM check name it: a directory
/// under its parent.
fn cgroup_at(dir: &Path) -> LeaseCgroup {
    let leaf = dir.file_name().and_then(|n| n.to_str()).expect("leaf");
    LeaseCgroup::at(dir.parent().expect("parent"), "", leaf)
}

/// Catches the swap watch reading `memory.events`, `memory.current` and
/// `memory.swap.current` on an async worker every second for every running lease: with
/// a node's two workers, two slow reads stop every other task of the daemon, its
/// heartbeat included.
#[test]
fn the_swap_watch_reads_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait(
        "swap-watch",
        "memory.events",
        write_end,
        |dir| async move {
            presses_into_swap(&cgroup_at(&dir), Some(1 << 30), u64::MAX, Duration::ZERO).await;
        },
    );
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the NUMA sampler reading `memory.stat` and `memory.numa_stat` on an async
/// worker every second for every running lease.
#[test]
fn the_numa_sampler_reads_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait(
        "numa-watch",
        "memory.stat",
        write_end,
        |dir| async move {
            NumaSample::default().watch(&dir, Duration::ZERO).await;
        },
    );
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the container's stdout and stderr files being created on an async worker: a
/// create on a busy file system waits for the journal.
#[test]
fn the_logs_are_created_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait("logs", "stdout", read_end, |dir| async move {
        let _ = create_logs(&dir.join("stdout"), &dir.join("stderr")).await;
    });
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the lease cgroup being made on an async worker: the read of the parent's OOM
/// count, the `mkdir`, the limit writes and the wait for the keeper process are the
/// first calls of a lease that touch the cgroup file system.
#[test]
fn the_cgroup_is_made_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait(
        "make-cgroup",
        "memory.events.local",
        write_end,
        |dir| async move {
            let cgroup = LeaseCgroup::at(&dir, "", "lease");
            let _ = make_cgroup(cgroup, Resources::new(1000, 1 << 30), None).await;
        },
    );
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the read of the node's CPU pressure at a lease's start on an async worker.
#[test]
fn the_cpu_pressure_is_read_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait(
        "start-read",
        "cpu.pressure",
        write_end,
        |dir| async move {
            read_start(dir.join("cpu.pressure"), 1000).await;
        },
    );
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the check for an OOM kill, which reads `memory.events` of a lease that was
/// killed, on an async worker.
#[test]
fn the_oom_check_reads_off_the_async_workers() {
    let started = blocking_threads_while_calls_wait(
        "oom-check",
        "memory.events",
        write_end,
        |dir| async move {
            let _ = oom_outcome(cgroup_at(&dir), None, None).await;
        },
    );
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the lease's resource usage, read from the cgroup's `cpu.stat`, memory and
/// pressure files when the action has ended, on an async worker.
#[test]
fn the_usage_is_read_off_the_async_workers() {
    let started =
        blocking_threads_while_calls_wait("usage", "cpu.stat", write_end, |dir| async move {
            usage(dir, NumaSample::default(), Instant::now(), Start::default()).await;
        });
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}

/// Catches the node load reading the `actions/` cgroup's files on an async worker every
/// heartbeat.
#[test]
fn the_node_load_reads_off_the_async_workers() {
    let started =
        blocking_threads_while_calls_wait("node-load", "cpu.stat", write_end, |dir| async move {
            let tracker = crate::node_load::LoadTracker::new(crate::node_load::process_clock());
            let _ = tracker.reading(dir).await;
        });
    assert_eq!(started, WORKERS, "{}", rest_waited(started));
}
