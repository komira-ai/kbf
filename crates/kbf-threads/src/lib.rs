//! Thread and disk sizing for `kbf-server`. Disk sizing (what a cache, a tree store or a
//! write tier may take of a filesystem) is the [`disk`] module.
//!
//! The server runs a tokio runtime with two pools (async workers, and a blocking pool
//! for calls that wait on a disk or a process) and has CPU-bound work (hashing) that
//! must not run wider than the CPUs it may use. It runs no actions, so nothing needs
//! protecting from it: it uses every CPU it may ([`PoolArgs::resolve`]).
//!
//! - [`PoolArgs`] is the optional overrides `--async-threads`, `--blocking-threads` and
//!   `--cpu-threads`. The server flattens it into its command line.
//! - [`PoolArgs::resolve`] turns them and a [`Topology`] into [`Sizes`].
//! - [`Sizes::runtime`] builds the tokio runtime with those sizes, and
//!   [`Sizes::log`] says them once at start.
//!
//! Async workers and the CPU pool default to every logical CPU the process may use
//! ([`Topology::logical`], so an affinity mask or a cgroup quota counts), the blocking
//! cap to tokio's own default of 512 ([`TOKIO_BLOCKING_DEFAULT`]). The CPU pool is
//! sized to logical CPUs, not physical cores: the server has no other tenant, and a
//! hashing thread that stalls on a memory or disk read leaves its sibling hyperthread
//! the core.
//!
//! `--cpu-threads` is the width of the pool for CPU-bound work. This crate only
//! resolves it ([`Sizes::cpu_threads`]); the pool is built by the code that has the
//! work.

pub mod disk;

use std::io;
use std::num::NonZeroUsize;
use std::path::Path;

use clap::Args;

/// Where the kernel publishes the CPU topology.
const SYSFS: &str = "/sys";

/// What a process may run on: its logical CPUs and how many share a core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Topology {
    logical: usize,
    threads_per_core: usize,
}

impl Topology {
    /// A topology of `logical` CPUs, `threads_per_core` of them to a core (both at
    /// least 1, and `threads_per_core` at most `logical`).
    #[must_use]
    pub fn new(logical: usize, threads_per_core: usize) -> Self {
        let logical = logical.max(1);
        Self {
            logical,
            threads_per_core: threads_per_core.clamp(1, logical),
        }
    }

    /// This machine's: the CPUs this process may run on
    /// ([`std::thread::available_parallelism`], which honours the affinity mask and a
    /// cgroup CPU quota, not the machine's count) and the threads per core the kernel
    /// reports for cpu0.
    #[must_use]
    pub fn detect() -> Self {
        let logical = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        Self::detect_in(Path::new(SYSFS), logical)
    }

    /// Reads the threads per core from the sysfs tree at `sysfs` (cpu0's sibling list,
    /// `thread_siblings_list` or, where the kernel names it so, `core_cpus_list`). A
    /// missing or unreadable list, as on macOS, is one thread per core.
    #[must_use]
    pub fn detect_in(sysfs: &Path, logical: usize) -> Self {
        let dir = sysfs.join("devices/system/cpu/cpu0/topology");
        let siblings = ["thread_siblings_list", "core_cpus_list"]
            .iter()
            .find_map(|name| std::fs::read_to_string(dir.join(name)).ok())
            .and_then(|text| siblings_in_list(&text));
        Self::new(logical, siblings.unwrap_or(1))
    }

    /// The CPUs the process may run on.
    #[must_use]
    pub const fn logical(&self) -> usize {
        self.logical
    }

    /// How many CPUs share a core.
    #[must_use]
    pub const fn threads_per_core(&self) -> usize {
        self.threads_per_core
    }

    /// The physical cores among the logical CPUs, at least 1.
    #[must_use]
    pub fn physical(&self) -> usize {
        (self.logical / self.threads_per_core).max(1)
    }
}

/// How many CPUs a kernel CPU list names: `0,44` is 2, `0-1` is 2, `0-3,8` is 5.
/// `None` for text that is not a list.
fn siblings_in_list(text: &str) -> Option<usize> {
    let mut count = 0usize;
    for part in text.trim().split(',') {
        let (from, to) = match part.split_once('-') {
            Some((from, to)) => (from.trim().parse::<usize>().ok()?, to.trim().parse().ok()?),
            None => {
                let one = part.trim().parse::<usize>().ok()?;
                (one, one)
            }
        };
        count = count.checked_add(to.checked_sub(from)?.checked_add(1)?)?;
    }
    Some(count)
}

/// Tokio's own cap on the blocking pool, which the server keeps.
pub const TOKIO_BLOCKING_DEFAULT: usize = 512;

/// The overrides of the three thread counts. A count left out comes from the default
/// ([`PoolArgs::resolve`]); one given is used as
/// given, even above the CPUs.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolArgs {
    /// Worker threads of the async runtime, which run every task between its waits.
    /// Default: every CPU the process may use.
    #[arg(long, value_name = "N")]
    pub async_threads: Option<NonZeroUsize>,
    /// The most threads the runtime's blocking pool runs at once, for calls that wait
    /// on a disk or a process (a thread is made when a call needs one and ends when
    /// idle). Default: 512 (tokio's).
    #[arg(long, value_name = "N")]
    pub blocking_threads: Option<NonZeroUsize>,
    /// Threads for CPU-bound work, such as hashing, which never waits and so is held
    /// to the CPUs. Default: every CPU the process may use.
    #[arg(long, value_name = "N")]
    pub cpu_threads: Option<NonZeroUsize>,
}

impl PoolArgs {
    /// The server's sizes on `topology`: every logical CPU for the async workers and
    /// the CPU pool, tokio's cap for the blocking pool, then the overrides. No reserve
    /// is taken: the server runs no actions.
    #[must_use]
    pub fn resolve(&self, topology: Topology) -> Sizes {
        let or = |given: Option<NonZeroUsize>, default: usize| {
            given.map_or(default, NonZeroUsize::get).max(1)
        };
        Sizes {
            async_threads: or(self.async_threads, topology.logical()),
            blocking_threads: or(self.blocking_threads, TOKIO_BLOCKING_DEFAULT),
            cpu_threads: or(self.cpu_threads, topology.logical()),
            topology,
        }
    }
}

/// The resolved sizes of one process's threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    async_threads: usize,
    blocking_threads: usize,
    cpu_threads: usize,
    topology: Topology,
}

impl Sizes {
    /// Worker threads of the async runtime.
    #[must_use]
    pub const fn async_threads(&self) -> usize {
        self.async_threads
    }

    /// The most threads the blocking pool runs.
    #[must_use]
    pub const fn blocking_threads(&self) -> usize {
        self.blocking_threads
    }

    /// The width of the pool for CPU-bound work.
    #[must_use]
    pub const fn cpu_threads(&self) -> usize {
        self.cpu_threads
    }

    /// Builds a multi-thread tokio runtime of these sizes, with every driver enabled.
    ///
    /// # Errors
    ///
    /// Whatever tokio returns when it cannot start the runtime (no threads, no
    /// epoll instance).
    pub fn runtime(&self) -> io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.async_threads)
            .max_blocking_threads(self.blocking_threads)
            .thread_name("kbf-async")
            .enable_all()
            .build()
    }

    /// The line logged at start: the sizes and the topology they came from.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "threads: async={} blocking={} cpu={} (cpus={} threads_per_core={} physical_cores={})",
            self.async_threads,
            self.blocking_threads,
            self.cpu_threads,
            self.topology.logical,
            self.topology.threads_per_core,
            self.topology.physical(),
        )
    }

    /// Logs [`Sizes::describe`] at INFO.
    pub fn log(&self) {
        tracing::info!("{}", self.describe());
    }
}

#[cfg(test)]
mod tests;
