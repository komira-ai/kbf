//! The driver's per-lease cgroup reads run on tokio's blocking pool, never on an async
//! worker.
//!
//! A read of a cgroup file can wait on the kernel (the statistics of a busy cgroup are
//! flushed under a lock, a journal commit can hold a write). Done on an async worker, it
//! holds that worker, and with a node's few workers every task of the process, the
//! daemon's heartbeat among them, waits behind it.
//!
//! Each test makes the file a named pipe nobody writes: the read cannot finish until the
//! test lets it. The test opens the pipe for writing, which returns only once the read
//! has opened its end, so the read is known to be waiting in the kernel. It then counts
//! the threads the runtime's blocking pool has started. Two reads wait at once on a runtime of two
//! workers; if both waited on the workers, the pool has started no thread.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{CWD, FileType, Mode};

use crate::cgroup::{LeaseCgroup, presses_into_swap};
use crate::cgroup_usage::NumaSample;

const WORKERS: usize = 2;

const THREAD: &str = "kbfw-";

/// How many threads of this process are named like the runtime's but are not among its
/// first `WORKERS`: the threads the blocking pool has started.
fn blocking_pool_threads() -> usize {
    let tasks = std::fs::read_dir("/proc/self/task").expect("threads");
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .filter_map(|name| name.trim().strip_prefix(THREAD)?.parse::<usize>().ok())
        .filter(|&n| n >= WORKERS)
        .count()
}

fn fifo(path: &Path) {
    rustix::fs::mknodat(CWD, path, FileType::Fifo, Mode::from_raw_mode(0o600), 0).expect("mkfifo");
}

/// Starts `WORKERS` reads on a runtime of `WORKERS` async workers, each of the cgroup
/// directory `<name>/<n>` whose `blocked` file is a pipe, waits until every read waits on
/// its pipe, and returns how many threads the runtime's blocking pool has started then.
/// `read` is the work under test, given the cgroup directory.
fn blocking_threads_while_reads_wait<F, Fut>(name: &str, blocked: &str, read: F) -> usize
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let scratch = crate::unit_dir(name);
    // The runtime starts its workers when it is built, so the first `WORKERS` names go to
    // them and any later name to a thread of the blocking pool.
    let named = std::sync::atomic::AtomicUsize::new(0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .thread_name_fn(move || {
            let n = named.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("{THREAD}{n}")
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
            ] {
                std::fs::write(dir.join(file), text).expect("file");
            }
            std::fs::remove_file(dir.join(blocked)).expect("replace");
            fifo(&dir.join(blocked));
            dir
        })
        .collect();
    let tasks: Vec<_> = dirs.iter().map(|dir| rt.spawn(read(dir.clone()))).collect();
    // Returns once the read has opened the pipe: it waits for a writer from here.
    let writers: Vec<_> = dirs
        .iter()
        .map(|dir| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(dir.join(blocked))
                .expect("a read opened the pipe")
        })
        .collect();
    let started = blocking_pool_threads();
    // Let every read finish, and every later one find a plain file.
    for dir in &dirs {
        let plain = dir.join("plain");
        std::fs::write(&plain, "").expect("plain");
        std::fs::rename(&plain, dir.join(blocked)).expect("replace");
    }
    drop(writers);
    for task in &tasks {
        task.abort();
    }
    drop(rt);
    started
}

/// Catches the swap watch reading `memory.events`, `memory.current` and
/// `memory.swap.current` on an async worker every second for every running lease: with
/// a node's two workers, two slow reads stop every other task of the daemon, its
/// heartbeat included.
#[test]
fn the_swap_watch_reads_off_the_async_workers() {
    let started =
        blocking_threads_while_reads_wait("swap-watch", "memory.events", |dir| async move {
            let leaf = dir.file_name().and_then(|n| n.to_str()).expect("leaf");
            let cgroup = LeaseCgroup::at(dir.parent().expect("parent"), "", leaf);
            presses_into_swap(&cgroup, Some(1 << 30), u64::MAX, Duration::ZERO).await;
        });
    assert_eq!(
        started, WORKERS,
        "two reads waited at once and the blocking pool started {started} threads: \
         the rest waited on the async workers"
    );
}

/// Catches the NUMA sampler reading `memory.stat` and `memory.numa_stat` on an async
/// worker every second for every running lease.
#[test]
fn the_numa_sampler_reads_off_the_async_workers() {
    let started =
        blocking_threads_while_reads_wait("numa-watch", "memory.stat", |dir| async move {
            NumaSample::default().watch(&dir, Duration::ZERO).await;
        });
    assert_eq!(
        started, WORKERS,
        "two reads waited at once and the blocking pool started {started} threads: \
         the rest waited on the async workers"
    );
}

/// Catches the node load reading the `actions/` cgroup's files on an async worker every
/// heartbeat.
#[test]
fn the_node_load_reads_off_the_async_workers() {
    let started = blocking_threads_while_reads_wait("node-load", "cpu.stat", |dir| async move {
        let tracker = crate::node_load::LoadTracker::new(crate::node_load::process_clock());
        let _ = tracker.reading(dir).await;
    });
    assert_eq!(
        started, WORKERS,
        "two reads waited at once and the blocking pool started {started} threads: \
         the rest waited on the async workers"
    );
}
