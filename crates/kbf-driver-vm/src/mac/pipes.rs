//! The two pipes between the daemon and one `kbf-vmm vm-run`.
//!
//! - The **control pipe** is the run's stdin: the daemon keeps the write end and nothing
//!   else holds one. `kbf-vmm` force-stops its VM when stdin reaches end of file, so the
//!   daemon dying (the kernel closes its descriptors) or the daemon closing the end ends
//!   the VM, with no signal and no helper involved. A line `shutdown` asks the guest to
//!   stop instead.
//! - The **event pipe** is the run's stdout: the daemon reads one JSON event per line.
//!   Once the helper has the write end and the daemon has closed its own, end of file
//!   means the run (and every child that inherited the descriptor) is gone.
//!
//! Every descriptor is close-on-exec, so no process the daemon starts later inherits
//! the write end of the control pipe and holds the VM up after the daemon is gone.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;

/// The ends the daemon passes to the helper, and closes once the helper has its own.
#[derive(Debug)]
pub struct HelperEnds {
    /// The run's stdin: the control pipe's read end.
    pub stdin: OwnedFd,
    /// The run's stdout: the event pipe's write end.
    pub stdout: OwnedFd,
}

/// Both pipes.
#[derive(Debug)]
pub struct Pipes {
    /// The control pipe's write end; closing it stops the VM.
    pub control: OwnedFd,
    /// The event pipe's read end.
    pub events: File,
    /// What goes to the helper.
    pub helper: HelperEnds,
}

impl Pipes {
    /// Makes both pipes.
    ///
    /// # Errors
    /// The system is out of descriptors.
    pub fn new() -> io::Result<Self> {
        // `std::io::pipe` makes close-on-exec descriptors on every platform.
        let (stdin, control) = std::io::pipe()?;
        let (events, stdout) = std::io::pipe()?;
        Ok(Self {
            control: OwnedFd::from(control),
            events: File::from(OwnedFd::from(events)),
            helper: HelperEnds {
                stdin: OwnedFd::from(stdin),
                stdout: OwnedFd::from(stdout),
            },
        })
    }
}
