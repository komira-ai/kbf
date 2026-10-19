//! A child process held between `fork` and `exec`, for tests of what a fork does to the
//! descriptors a process has open.
//!
//! A process that starts a child from one thread while another thread holds a file open
//! gives the child a copy of that descriptor, which it keeps until it execs (a file
//! opened close-on-exec, as `std` opens every file, is closed then). The tests of one
//! test binary run on threads of one process, so any test that spawns a program is such
//! a child to every file another test has open, for the few milliseconds the exec takes.
//! Two things go wrong with the copy:
//!
//! - an `flock` belongs to the open file description, which lives until the last copy of
//!   the descriptor is closed: a lock released only by closing the file stays held while
//!   a child holds a copy, and the next open of the same lock is refused;
//! - a file opened for writing counts as being written while a copy of the descriptor
//!   lives: the program in it cannot be executed (`ETXTBSY`, [`crate::spawn`]).
//!
//! [`ForkedChild`] makes that interleaving the only one: the child stays between `fork`
//! and `exec` until the test lets it go.
//!
//! The child holds a copy of *every* descriptor the process has open, those of the other
//! tests of the binary too, for as long as it is held. A test that forks one belongs in a
//! binary whose other tests do not depend on a close (a socket that stops listening, a
//! lock that is released by closing its file).

use std::io::{self, PipeReader, PipeWriter, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};
use std::thread::JoinHandle;

/// A child process caught between `fork` and `exec`, holding a copy of every descriptor
/// its parent had open when it forked. It ends, by [`Self::finish`] or by being dropped,
/// after the test has let it go.
pub struct ForkedChild {
    /// The write end of the pipe the child waits on, and the thread inside
    /// `Command::spawn`, which returns only when the child has exec'd; `None` once the
    /// child has been let go.
    held: Option<(PipeWriter, JoinHandle<Child>)>,
}

impl ForkedChild {
    /// Forks a child that waits for [`Self::finish`] before it execs, and returns once
    /// the child exists: every descriptor open now has a copy in it.
    ///
    /// # Panics
    /// When the pipes cannot be made, or the child cannot be forked.
    #[must_use]
    pub fn before_exec() -> Self {
        let (go_rx, go_tx) = io::pipe().expect("a pipe");
        let (forked_rx, forked_tx) = io::pipe().expect("a pipe");
        // `/usr/bin/true` is at that path on macOS and on usr-merged Linux; a search of
        // `PATH` is not what a test of the fork wants, and a shell is not needed.
        let mut command = Command::new("/usr/bin/true");
        // SAFETY: the closure runs in the child between fork and exec, and does what
        // `hold` does: one `write` and one `read` on pipes it owns, allocating nothing.
        unsafe {
            command.pre_exec(in_the_child(forked_tx, go_rx));
        }
        let spawner = std::thread::spawn(move || command.spawn().expect("the held child execs"));
        // The child writes once it exists. If `spawn` fails instead, the spawner thread
        // panics, its `command` drops the last writer, and this read errors, not waits.
        (&forked_rx)
            .read_exact(&mut [0])
            .expect("the child was forked");
        Self {
            held: Some((go_tx, spawner)),
        }
    }

    /// Lets the child exec and waits for it to end.
    ///
    /// # Panics
    /// When the child did not exec, or ended with a failure.
    pub fn finish(mut self) {
        let status = self.end().expect("the forked child ended");
        assert!(status.success(), "the forked child: {status}");
    }

    /// Lets the child go and waits for it; `None` when it was let go before, or did not
    /// exec.
    fn end(&mut self) -> Option<ExitStatus> {
        let (go, spawner) = self.held.take()?;
        (&go).write_all(&[1]).ok()?;
        spawner.join().ok()?.wait().ok()
    }
}

impl Drop for ForkedChild {
    /// A test that fails before [`Self::finish`] still lets the child go.
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// What the child does between fork and exec: says it exists, then waits to be let go.
/// A function of its own, returning the closure `pre_exec` takes, so a test can run it
/// here, where its lines are counted (the child's counters are gone when it execs).
fn in_the_child(
    forked: PipeWriter,
    go: PipeReader,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || {
        (&forked).write_all(&[1])?;
        (&go).read_exact(&mut [0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a child that never says it exists (the parent would wait for ever), or
    /// one that does not wait to be let go (it would exec before the test looked).
    #[test]
    fn the_child_says_it_exists_then_waits_to_be_let_go() {
        let (go_rx, go_tx) = io::pipe().expect("a pipe");
        let (forked_rx, forked_tx) = io::pipe().expect("a pipe");
        (&go_tx).write_all(&[1]).expect("let go");
        in_the_child(forked_tx, go_rx)().expect("held, then let go");
        (&forked_rx).read_exact(&mut [0]).expect("said it exists");
    }
}
