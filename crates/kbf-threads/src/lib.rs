//! Thread sizing for the two farm binaries, `kbf-daemon` and `kbf-server`.
//!
//! Each binary runs a tokio runtime with two pools (async workers, and a blocking pool
//! for calls that wait on a disk or a process) and has CPU-bound work (hashing) that
//! must not run wider than the cores it may use. Tokio's defaults, one worker per
//! logical CPU and up to 512 blocking threads, suit neither: the farm nodes are
//! hyperthreaded (44 cores, 88 logical CPUs), and a worker per hyperthread only adds
//! contention to work that is mostly waiting. So the sizes are flags, with defaults
//! derived from the physical cores ([`Topology`]).
//!
//! - [`ThreadArgs`] is the flags a binary flattens into its own command line:
//!   `--reserve-cpus` (with `--reserve-share`), and the optional overrides
//!   `--async-threads`, `--blocking-threads` and `--cpu-threads`.
//! - [`ThreadArgs::resolve`] turns them, a [`Role`] and a [`Topology`] into [`Sizes`],
//!   or refuses a reserve the machine cannot give ([`ReserveError`]).
//! - [`Sizes::runtime`] builds the tokio runtime with those sizes, and
//!   [`Sizes::log`] says them once at start.
//!
//! # The reserve
//!
//! No core numbers are pinned. `--reserve-cpus=N` says how many logical CPUs the farm's
//! own processes keep for themselves; every other CPU is for the actions. On a daemon
//! the leases get the CPUs the process may use minus `N` ([`Sizes::lease_cpus`]), and
//! the daemon's own pools are sized from `N` ([`pool_sizes`]): async workers
//! `max(2, N/4)`, CPU pool `max(2, N/2)`, each at most the physical cores. On a host
//! that runs the server and the daemon the operator gives both the same `N` and
//! `--reserve-share=2`: each sizes its pools from half of it, and the daemon still
//! leaves all of `N` out of the leases. A server alone on a host reserves every CPU
//! (it runs no actions).
//!
//! `--cpu-threads` is the width of the pool for CPU-bound work. This crate only
//! resolves it ([`Sizes::cpu_threads`]); the pool is built by the code that has the
//! work.

use std::io;
use std::num::NonZeroUsize;
use std::path::Path;

use clap::Args;
use thiserror::Error;

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

/// Which binary the sizes are for; the defaults differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `kbf-daemon`: few async workers (it moves bytes and talks to one server), many
    /// blocking threads (file walks and copies in leases).
    Daemon,
    /// `kbf-server`: more workers for the connections it serves.
    Server,
}

impl Role {
    /// The default blocking-pool cap: threads that mostly sleep on disks and processes,
    /// so not tied to the CPUs.
    const fn blocking_default(self) -> usize {
        match self {
            Self::Daemon => 64,
            Self::Server => 128,
        }
    }

    /// The reserve when `--reserve-cpus` is left out: the daemon's is 8 logical CPUs
    /// (4 cores of 2 threads), or half the CPUs on a machine with fewer than 16; the
    /// server's is all of them, as it runs no actions.
    fn default_reserve(self, logical: usize) -> usize {
        match self {
            Self::Daemon => DEFAULT_DAEMON_RESERVE.min(logical / 2),
            Self::Server => logical,
        }
    }
}

/// The daemon's `--reserve-cpus` default, in logical CPUs: 4 physical cores with their
/// hyperthreads.
pub const DEFAULT_DAEMON_RESERVE: usize = 8;

/// The async-worker and CPU-pool sizes a reserve of `reserve` logical CPUs gives on a
/// machine of `cores` physical cores: `max(2, reserve/4)` and `max(2, reserve/2)`,
/// each at most `cores` (and at least 1).
#[must_use]
pub fn pool_sizes(reserve: usize, cores: usize) -> (usize, usize) {
    let cores = cores.max(1);
    (
        (reserve / 4).max(2).min(cores),
        (reserve / 2).max(2).min(cores),
    )
}

/// A `--reserve-cpus` the machine cannot give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ReserveError {
    /// The daemon's leases would get no CPU.
    #[error(
        "--reserve-cpus={reserve} leaves no CPU for leases: this machine has {logical} \
         (give less than that)"
    )]
    NoneLeft { reserve: usize, logical: usize },
    /// More than the machine has.
    #[error("--reserve-cpus={reserve} is more than this machine's {logical} CPUs")]
    MoreThanTheMachine { reserve: usize, logical: usize },
}

/// The thread flags. `--reserve-cpus` and `--reserve-share` size everything; the three
/// thread counts are overrides. A count left out comes from the reserve
/// ([`ThreadArgs::resolve`]); one given is used as given, even above the CPUs.
#[derive(Args, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadArgs {
    /// Logical CPUs the farm's own processes keep, in CPUs (hyperthreads count one
    /// each). The daemon gives its leases every CPU it may use but these, with no
    /// pinning, and sizes its own pools from them; the server sizes its pools from
    /// them. Default: 8 for the daemon (half the CPUs on a machine of fewer than 16),
    /// every CPU for the server. 0 is allowed; the daemon refuses a value that leaves
    /// no CPU for leases, and both refuse more than the machine has. On a host that
    /// runs both, give both the same value and `--reserve-share=2`.
    #[arg(long, value_name = "N")]
    pub reserve_cpus: Option<usize>,
    /// How many of the farm's processes on this host share the reserve: this one sizes
    /// its pools from `reserve / share` CPUs. Default 1; `2` on both the server and the daemon
    /// of one host.
    #[arg(long, value_name = "N")]
    pub reserve_share: Option<NonZeroUsize>,
    /// Worker threads of the async runtime, which run every task between its waits.
    /// Default: `max(2, reserve/4)`, at most the physical cores.
    #[arg(long, value_name = "N")]
    pub async_threads: Option<NonZeroUsize>,
    /// The most threads the runtime's blocking pool runs at once, for calls that wait
    /// on a disk or a process (a thread is made when a call needs one and ends when
    /// idle). Default: 64 for the daemon, 128 for the server.
    #[arg(long, value_name = "N")]
    pub blocking_threads: Option<NonZeroUsize>,
    /// Threads for CPU-bound work, such as hashing, which never waits and so is held
    /// to the cores. Default: `max(2, reserve/2)`, at most the physical cores.
    #[arg(long, value_name = "N")]
    pub cpu_threads: Option<NonZeroUsize>,
}

impl ThreadArgs {
    /// The sizes for `role` on `topology`: the reserve (given, or the role's default),
    /// the pools from the reserve's share, then the overrides. The async and CPU
    /// defaults are limited to the physical cores; the blocking default is not,
    /// because its threads mostly sleep.
    ///
    /// # Errors
    ///
    /// A reserve the machine cannot give ([`ReserveError`]).
    pub fn resolve(&self, role: Role, topology: Topology) -> Result<Sizes, ReserveError> {
        let logical = topology.logical();
        let reserve = self
            .reserve_cpus
            .unwrap_or_else(|| role.default_reserve(logical));
        if reserve > logical {
            return Err(ReserveError::MoreThanTheMachine { reserve, logical });
        }
        if role == Role::Daemon && reserve == logical {
            return Err(ReserveError::NoneLeft { reserve, logical });
        }
        let share = reserve / self.reserve_share.map_or(1, NonZeroUsize::get);
        let (async_default, cpu_default) = pool_sizes(share, topology.physical());
        let or = |given: Option<NonZeroUsize>, default: usize| {
            given.map_or(default, NonZeroUsize::get).max(1)
        };
        Ok(Sizes {
            async_threads: or(self.async_threads, async_default),
            blocking_threads: or(self.blocking_threads, role.blocking_default()),
            cpu_threads: or(self.cpu_threads, cpu_default),
            reserve,
            topology,
        })
    }
}

/// The resolved sizes of one process's threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizes {
    async_threads: usize,
    blocking_threads: usize,
    cpu_threads: usize,
    reserve: usize,
    topology: Topology,
}

impl Sizes {
    /// The reserve in logical CPUs, as given or defaulted.
    #[must_use]
    pub const fn reserve_cpus(&self) -> usize {
        self.reserve
    }

    /// The CPUs the daemon's leases get: the CPUs the process may use, minus the
    /// reserve (at least 1; a daemon whose reserve leaves none is refused by
    /// [`ThreadArgs::resolve`]).
    #[must_use]
    pub fn lease_cpus(&self) -> usize {
        self.topology.logical.saturating_sub(self.reserve).max(1)
    }

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

    /// The line logged at start: the sizes and what they came from.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "threads: async={} blocking={} cpu={} reserve={} leases={} (cpus={} threads_per_core={} physical_cores={})",
            self.async_threads,
            self.blocking_threads,
            self.cpu_threads,
            self.reserve,
            self.lease_cpus(),
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
