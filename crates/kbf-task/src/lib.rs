//! One build action, run as a job of `js-agent`: the task tool inside the job's container.
//!
//! The job's root file system is the action's tree, which the frontend wrote, and the job has no
//! network. The tool then
//! 1. reads the Action and its Command from the tree, checking each against its digest
//!    ([`message`], [`tree`]), and finds the input root there: nothing is fetched and no read
//!    grant is held;
//! 2. runs the Command there ([`run`]): its argv, environment and working directory, stdout and
//!    stderr to files, under a deadline. The environment is the Command's own, preceded by `HOME`,
//!    `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`, which name fresh directories
//!    `private/{home,tmp,cache}` (the last holding `clang/ModuleCache`) under the task's run
//!    directory `.kbf-run`, mode 0700, made new for the run (an existing run directory is
//!    refused), beside the input root and not in it. A job's image root is read-only and its
//!    `/tmp` is a small `noexec` tmpfs, so a compiler that writes under `~` or the temporary
//!    directory would otherwise fail; and since outputs are collected relative to the input
//!    root, nothing written there is packed. The job is the Command's sandbox ([`run::Job`]): it
//!    is started directly, and every process it leaves is ended when it ends, before the outputs
//!    are read. A variable the Command sets itself keeps its value. A directory that cannot be
//!    made fails the run start (exit 11). The platform property `kbf-network=allow` lifts the
//!    network denial for one action, and any other value of it fails the run start before the
//!    Command runs ([`network`]; this tool is non-dumpable from its first instruction, so the
//!    Command cannot take its sockets);
//! 3. collects the declared outputs as REAPI does ([`outputs`]): files, directories as Trees,
//!    symlinks, the executable bit, each blob written to the run's one object as it is read,
//!    never held whole ([`spool`]);
//! 4. ends the object with the manifest and its index, validated with the shared task pack types
//!    of `kbf-task-pack` (`TaskManifest`), the same code the frontend that ingests the object
//!    uses, and commits it ([`out_dir`]).
//!
//! # The put directory
//!
//! The run's one object, `pack`, is streamed as a file into `--out-dir` (`/out` by default) while
//! the outputs are read, and committed by two renames, `pack.part` to `pack.sha256-<hex>.part`
//! and that to `pack` ([`out_dir`]; the object's format is in [`spool`] and
//! docs/design/one-pass-task-object.md). The exit code is 0 only when the commit `rename`
//! returned 0, whatever the Command's own exit code; a failed write or commit is exit 13.
//!
//! The tool returns nothing but an exit code, so the manifest travels inside the object, and the
//! exit code says only whether the farm did its part: 0 when the action ran and its object was
//! committed (whatever the action's own exit code, which is in the manifest), nonzero when
//! reading, packing or storing failed. Nothing the tool writes is trusted: the frontend hashes
//! every record of the object, checks its footer, and builds the action's result from its own
//! hashing.
//!
//! # What the tool does not separate
//!
//! The Command runs as a child of the tool, under the same user: the job is the sandbox.
//!
//! # The confinement probe
//!
//! `kbf-task --probe-confinement` runs a Command under the same confinement as an action's and
//! prints whether it holds, one `probe <name>: ok|FAIL|n/a` line per check ([`probe`]). It needs
//! no server or network.

#[cfg(target_os = "linux")]
pub(crate) mod child;
pub mod digest;
pub mod files;
mod inputs;
pub mod message;
pub mod network;
#[cfg(target_os = "linux")]
mod orphans;
pub mod out_dir;
pub mod outputs;
pub mod pack;
pub mod probe;
#[cfg(target_os = "linux")]
pub mod relay;
pub mod run;
pub mod sandbox;
pub mod spool;
pub mod task;
mod task_util;
pub mod tree;

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod in_child;

// The feature that lets a denied Command run with the network where no confine exists is for tests:
// a build for a system that is neither Linux nor macOS, without debug assertions (a release build),
// that turns it on does not compile. On Linux and macOS, which have a confine, the feature does
// nothing, so it is allowed there (a workspace-wide `--all-features` build).
#[cfg(all(
    feature = "unconfined-for-tests",
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(debug_assertions),
    not(test)
))]
compile_error!(
    "the `unconfined-for-tests` feature of kbf-task is for test builds only; a release build must not enable it"
);
