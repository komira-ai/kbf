//! The native driver for `kbf-daemon`: [`NativeRuntime`] runs each `action` lease as
//! plain processes on the node, on Linux, behind the daemon's `Runtime` trait.
//!
//! What one lease gets:
//! - a fresh lease directory under the configured scratch root, the input root written
//!   into it, the working directory and each output's parent made (REAPI);
//! - its own home, temporary and cache directories inside the lease directory, named by
//!   `HOME`, `CFFIXED_USER_HOME`, `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`
//!   (`home`); so a tool's `~` and module caches go with the lease;
//! - its `arguments` run with those variables and the Command's environment (which wins
//!   where both name a variable) and nothing else, stdin from `/dev/null`, stdout and
//!   stderr captured to files, as the leader of a new process group;
//! - no sandbox: the action runs as the daemon's user with the node's network whatever
//!   its `network` platform property says (a value that property does not define is
//!   still refused), and the node reports that in its `network_isolation` capability
//!   ([`network`]);
//! - a memory watch: every poll, the physical footprint of all the action's processes
//!   together ([`procs`]); past the lease's limit, the whole tree is killed and the
//!   lease ends RESOURCE_EXHAUSTED ([`kbf_daemon::RuntimeError::OutOfMemory`]);
//! - an end that asks first: SIGTERM to every process of the lease, up to
//!   [`NativeConfig::term_grace`] for them to go, then SIGKILL ([`grace`]); the grace is
//!   outside the timeout and the memory watch, and at most 60 s; a process that
//!   ignored SIGTERM is named in the lease's stderr;
//! - a wall-clock timeout, and the daemon's kill, each ending the whole process tree;
//!   a timeout still stores what the action wrote to stdout and stderr
//!   ([`kbf_daemon::RuntimeError::TimedOut`]);
//! - on every path, also a normal exit, every process of the action ended before the
//!   outputs are read: SIGKILL to the group and to each process the tracker knows,
//!   repeated until a snapshot shows none alive; any that survive fail the lease;
//! - outputs, stdout and stderr read with `kbf-outputs`: no symlink followed at any
//!   level; depth, entry, byte and stdio limits; files of more than a chunk streamed
//!   to the CAS ([`kbf_daemon::Cas::put_chunks`]); names that are not UTF-8 refused;
//! - the lease directory removed afterwards (`kbf_outputs::remove_tree`: iterative,
//!   by descriptor, giving back permissions the action took away); a lease directory
//!   that stays fails the lease, so a dirty node is loud. A run the daemon drops is
//!   cleaned by `Drop`.
//!
//! A macOS action does not run here: the farm sends it to a `js-agent` job on the Mac
//! (`docs/design/actions-as-jobs.md`, section 14).
//!
//! Before an action's program runs, its run record (the leader's pid, start time and
//! boot) is written under `runs/` in the scratch root (`record`): the child waits
//! between fork and exec until it is. At start the runtime kills every action a
//! previous daemon recorded and left running, the daemon having been killed (`sweep`;
//! processes of the daemon's user that survive SIGKILL stop the start; anything in
//! `runs/` that is not a record of the daemon's is moved aside into `quarantine/`, and
//! another user's processes are left alone), then removes every lease directory it left;
//! one that cannot be removed is moved aside into `quarantine/` under the scratch root
//! and logged, and the daemon starts anyway (an action decides what its directory
//! holds, so refusing to start would let one build step take the node out of the farm).
//! Until then, each action's keeper (`keeper`), a member of its group, kills the group
//! when the daemon dies, restarted or not.
//!
//! Known gaps, all of them closed only by running each lease as its own user:
//! - **Processes that leave the tree.** A process that calls `setsid` and is orphaned
//!   between two polls is not seen ([`procs`]), so it outlives the lease.
//! - **The daemon's own files.** Actions run as the daemon's user, so they can read
//!   and write what it can, the node's TLS private key (`--key`) included. An action
//!   that locks the scratch root or `quarantine/` (`chmod 555`) makes every later lease
//!   on the node fail until an operator unlocks it.
//! - **The signal race.** [`procs::kill_all`] signals pids from a snapshot; one
//!   recycled in between is a process of the daemon's user killed by mistake.
//! - **The run records.** The start-up sweep trusts `runs/` because only the daemon
//!   writes it, but an action, which runs as the daemon's user, can write a well-formed
//!   record there (pids and start times are readable in `/proc`), and the next start
//!   SIGKILLs every process of that user in the group it names or below it. That holds
//!   for a group the daemon may not signal too: another user's group is left alone, but
//!   the walk goes on below it and kills the daemon's user's processes there. Only a
//!   record naming the daemon's own group or its parent's is dropped without its group
//!   being walked. What is not a regular file of the daemon's user that only it may
//!   write is set aside unread, and neither another user's processes nor those the
//!   daemon never signals ([`procs::Guards`]) are counted as survivors, so a forged
//!   entry cannot stop or hang a start.
//!
//! Not yet: a per-lease user (which would make "every process of the action" exact,
//! keep actions out of each other's files and the daemon's key, and close the gaps
//! above), CPU limits, and network isolation.

mod cas;
mod config;
pub mod grace;
pub mod home;
mod keeper;
pub mod leader;
pub mod network;
pub mod procs;
mod program;
mod record;
mod runtime;
mod sweep;

// The keepers this crate's unit tests start come from the test binary.
#[cfg(test)]
kbf_daemon::keeper_test_hook!();

/// A scratch directory for one unit test, beside the test binary (inside the target
/// directory), removed when it is dropped. Declare it before anything that runs in it.
#[cfg(test)]
pub(crate) fn unit_dir(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(unit_root(), name)
}

/// The directory the unit tests' scratch directories are made in.
#[cfg(test)]
pub(crate) fn unit_root() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    exe.parent()
        .expect("deps directory")
        .join("kbf-driver-native-unit")
}

/// A link to the test binary named for the act steps, in `dir`: one per run of the
/// binary, which lives as long as the process (so no `TestDir`), remade by rename so
/// that runs side by side do not race.
#[cfg(test)]
pub(crate) fn act_link(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("act directory");
    let link = dir.join(kbf_testkit::act::NAME);
    let made = dir.join(format!(".act-{}", std::process::id()));
    let _ = std::fs::remove_file(&made);
    kbf_testkit::act::link_self(&made).expect("link");
    std::fs::rename(&made, &link).expect("place the link");
    link
}

/// A link to this test binary named `kbf-act`, made once: run through it, the binary
/// runs kbf-testkit's act steps (`kbf_testkit::act`) instead of the tests.
#[cfg(test)]
pub(crate) fn act() -> std::path::PathBuf {
    static LINK: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    LINK.get_or_init(|| act_link(&unit_root().join("act")))
        .clone()
}
// So do the act steps the unit tests run.
#[cfg(test)]
kbf_testkit::act_test_hook!();

pub use cas::CasStore;
pub use config::{DEFAULT_TERM_GRACE, MemoryPolicy, NativeConfig};
pub use record::{Record, boot_id};
pub use runtime::{DRIVER, KIND, NativeRuntime};
