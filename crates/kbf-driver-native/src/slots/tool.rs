//! Running a tool with a deadline that holds on every path: the call returns within the
//! time it was given, whatever the tool or a process it started does with its pipes.
//!
//! The output is read on threads of their own and handed back through a channel, so the
//! caller waits for it with a timeout instead of joining. A thread still blocked in a read
//! (a process the tool started holds the pipe's write end) is abandoned: it holds nothing
//! but its end of the pipe and a sender whose receiver is gone, and ends when the last
//! writer of the pipe does.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::pace::{Pace, Wall};

/// How long a killed tool is waited for before it is given up as not gone.
pub(crate) const KILL_WAIT: Duration = Duration::from_secs(5);

/// The pause between two looks at whether a child has exited.
const POLL: Duration = Duration::from_millis(5);

/// A tool's status, stdout and stderr.
pub(crate) type Output = (ExitStatus, Vec<u8>, Vec<u8>);

/// Runs `command` with a plain environment and no input; its status and output. The call
/// returns within `within` (plus [`KILL_WAIT`] for a tool that does not die when killed):
/// a tool that has not exited by then is killed and the call fails, and so does a tool
/// that exited but left a process holding its output open.
pub(crate) fn run_within(command: &mut Command, within: Duration) -> Result<Output, String> {
    run_paced(&Wall::new(), command, within, KILL_WAIT)
}

/// [`run_within`] on the clock `pace`, with the wait after a kill given.
pub(crate) fn run_paced(
    pace: &dyn Pace,
    command: &mut Command,
    within: Duration,
    kill_wait: Duration,
) -> Result<Output, String> {
    let mut child = command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let (sender, receiver) = mpsc::channel();
    // Both are piped above, so both are there.
    drain(
        child.stdout.take().expect("stdout is piped"),
        0,
        sender.clone(),
    );
    drain(child.stderr.take().expect("stderr is piped"), 1, sender);
    finish(pace, &mut child, &receiver, (within, kill_wait))
}

/// A process that can be looked at and killed: a [`Child`], or a stand-in in a test.
pub(crate) trait Waited {
    /// The status, once the process has exited.
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
}

impl Waited for Child {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
}

/// The part of a call after the tool has started: waits for its exit until `within` has
/// passed on `pace` (killing it then, and waiting `kill_wait` more for it to go), then
/// for both streams on `receiver` until the same time.
fn finish(
    pace: &dyn Pace,
    child: &mut dyn Waited,
    receiver: &mpsc::Receiver<(usize, Vec<u8>)>,
    (within, kill_wait): (Duration, Duration),
) -> Result<Output, String> {
    let until = pace.now() + within;
    let Some(status) = poll_exit(pace, child, until) else {
        let _ = child.kill();
        let reaped = poll_exit(pace, child, pace.now() + kill_wait).is_some();
        return Err(timeout_message(within, reaped));
    };
    let mut streams: [Vec<u8>; 2] = Default::default();
    for _ in 0..2 {
        let (which, bytes) = loop {
            match receiver.try_recv() {
                Ok(got) => break got,
                Err(mpsc::TryRecvError::Empty) if pace.now() < until => pace.pause(POLL),
                Err(_) => {
                    return Err(format!(
                        "its output was not closed within {within:?} (a process it started holds it)"
                    ));
                }
            }
        };
        streams[which] = bytes;
    }
    let [out, err] = streams;
    Ok((status, out, err))
}

/// What the call says when the tool did not finish in time: whether it then went when
/// killed.
fn timeout_message(within: Duration, reaped: bool) -> String {
    if reaped {
        format!("did not finish within {within:?}")
    } else {
        format!("did not finish within {within:?} and did not exit after kill")
    }
}

/// Waits for `child` to exit until `pace` reads `until`; its status, or `None` if it has
/// not exited by then (or cannot be waited for).
pub(crate) fn poll_exit(
    pace: &dyn Pace,
    child: &mut dyn Waited,
    until: Duration,
) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if pace.now() < until => pace.pause(POLL),
            _ => return None,
        }
    }
}

/// Reads `pipe` to its end on a thread of its own and sends it as stream `which`, so that
/// a tool that prints more than a pipe holds does not block.
fn drain<R: Read + Send + 'static>(mut pipe: R, which: usize, sender: Sender<(usize, Vec<u8>)>) {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        // The receiver is gone when the call gave up; the bytes are then not wanted.
        let _ = sender.send((which, bytes));
    });
}

#[cfg(test)]
mod tests;
