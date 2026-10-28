//! [`VmSession`]: what the macOS host asks of the Mac around it, behind a trait so the
//! host's rules run on any platform against a fake.
//!
//! The real session (`mac::client`, macOS only) is the root helper's socket
//! (`kbf-mac-session`'s `Client`) plus the kernel:
//! `role-session-ready`, `role-run`, the wait for the run's exit and `role-reap`, the
//! boot id and a process's start time. Cloning an image is here too, because only a Mac
//! clones copy-on-write.

use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

use kbf_driver_native::Record;
use kbf_guest::server::Stream;

/// How a run ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// The four descriptors `role-run` passes the process, in the order it takes them.
#[derive(Debug)]
pub struct RunFds<'a> {
    pub stdin: BorrowedFd<'a>,
    pub stdout: BorrowedFd<'a>,
    pub stderr: BorrowedFd<'a>,
    /// The process's working directory.
    pub cwd: BorrowedFd<'a>,
}

/// The Mac, as the host reaches it. Calls block.
pub trait VmSession: Send + Sync + 'static {
    /// A run the helper started.
    type Running: Send + 'static;
    /// The relayed byte stream to a guest's `kbf-guest` agent.
    type Stream: Stream;

    /// Clones the image directory `image` to the new directory `clone`, copy-on-write,
    /// each file owned by the caller and without the immutable flag a sealed image's
    /// files carry.
    ///
    /// # Errors
    /// The clone could not be made.
    fn clone_image(&self, image: &Path, clone: &Path) -> io::Result<()>;

    /// Whether the VM user's GUI session is up, which a run needs.
    ///
    /// # Errors
    /// The helper could not be asked.
    fn session_ready(&self) -> io::Result<bool>;

    /// Starts `argv` as the VM user with `fds`. The helper takes its own copies of the
    /// descriptors; the caller's stay the caller's.
    ///
    /// # Errors
    /// The helper refused or the process did not start.
    fn run(&self, argv: &[String], fds: RunFds<'_>) -> io::Result<Self::Running>;

    /// The pid the helper reported for `running`: the leader of its process group.
    fn pid(&self, running: &Self::Running) -> u32;

    /// Blocks until `running` has ended.
    ///
    /// # Errors
    /// The connection to the helper failed before the run's end was reported.
    fn wait(&self, running: Self::Running) -> io::Result<RunExit>;

    /// Ends the process group `record` names, if it is still that run (the helper
    /// checks pid, start time and boot again); returns once the group is empty.
    ///
    /// # Errors
    /// The helper refused or processes of the group survived.
    fn reap(&self, record: &Record) -> io::Result<()>;

    /// This boot's id (macOS: `kern.bootsessionuuid`).
    ///
    /// # Errors
    /// The kernel did not say.
    fn boot_id(&self) -> io::Result<String>;

    /// The start time of the live process `pid`, in the unit [`Record::start`] holds;
    /// `None` when there is no such process.
    ///
    /// # Errors
    /// The process table could not be read.
    fn process_start(&self, pid: i32) -> io::Result<Option<u64>>;

    /// A new stream to the guest agent of the running VM `name`.
    ///
    /// # Errors
    /// The guest cannot be reached.
    fn connect(&self, name: &str) -> io::Result<Self::Stream>;
}
