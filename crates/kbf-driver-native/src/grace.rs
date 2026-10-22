//! Ending an action's processes politely before the kill.
//!
//! A process that is killed with SIGKILL runs nothing on its way out. On macOS that
//! matters for one kind of leftover: POSIX shared memory and semaphore names cannot be
//! listed there, so nothing can remove the ones a killed workload made. Python's
//! `multiprocessing` makes them through a resource tracker process, which unlinks every
//! name it was told about when it is asked to end (SIGTERM) or when its parent goes
//! away. Killed together with the workload, the tracker unlinks nothing and the names
//! stay until the machine restarts.
//!
//! So both places that end a lease's processes, the end of the lease
//! ([`crate::procs`]' process tree and the slot user's processes) and the slot's clean,
//! send SIGTERM first and give the processes up to a grace period to end
//! (`--native-term-grace-ms`, [`crate::NativeConfig::term_grace`]), and only then
//! SIGKILL whatever is left. A grace of zero is SIGKILL at once. [`term_first`] is that
//! first half, with the signals, the process table and the clock passed in, so that it
//! runs under test on every platform.
//!
//! A process that is still alive when the grace ends ignored SIGTERM, and is named in
//! the lease's stderr ([`note`]) as that action's own hermeticity failure: its pid,
//! name and arguments. The action is not failed for it.
//!
//! What this cannot see: shared memory made without a tracker (Python's
//! `SharedMemory(track=False)`, or C code calling `shm_open` and exiting without
//! `shm_unlink`) is not removed by anything on macOS, and it is not reported either:
//! the system offers no way to list the names, so there is nothing to compare.

use std::time::Duration;

/// The most survivors [`note`] names; the rest are counted.
pub(crate) const MAX_NAMED: usize = 8;

/// The most characters of a survivor's arguments [`note`] shows.
pub(crate) const ARGS_SHOWN: usize = 120;

/// A process that was still alive when the grace ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Survivor {
    pub pid: i32,
    pub name: String,
    /// Its arguments as one line; empty when they could not be read.
    pub args: String,
}

impl Survivor {
    /// The survivor `pid`, named by `command` (its name and arguments as
    /// [`crate::procs::command_of`] reads them); `?` if the process is already gone.
    #[must_use]
    pub fn of(pid: i32, command: Option<(String, String)>) -> Self {
        let (name, args) = command.unwrap_or_else(|| ("?".to_owned(), String::new()));
        Self { pid, name, args }
    }
}

/// SIGTERM first: calls `term` (SIGTERM to every process of the lease), then `live`
/// (the pids still alive) every `pause` until none is left or `elapsed` has reached
/// `grace`. Returns the processes still alive then, each described by `describe`
/// before anything kills it; the caller kills them next.
///
/// Nothing is sent when `grace` is zero or when `live` finds no process at first: the
/// result is empty and the caller's kill runs as it always did.
///
/// # Errors
/// `term` or `live` failed (the text says which).
pub(crate) fn term_first(
    grace: Duration,
    term: &mut dyn FnMut() -> Result<(), String>,
    live: &mut dyn FnMut() -> Result<Vec<i32>, String>,
    describe: &mut dyn FnMut(i32) -> Survivor,
    elapsed: &mut dyn FnMut() -> Duration,
    pause: &mut dyn FnMut(),
) -> Result<Vec<Survivor>, String> {
    if grace.is_zero() || live()?.is_empty() {
        return Ok(Vec::new());
    }
    term()?;
    loop {
        let left = live()?;
        if left.is_empty() {
            return Ok(Vec::new());
        }
        if elapsed() >= grace {
            return Ok(left.into_iter().map(describe).collect());
        }
        pause();
    }
}

/// The line that names `survivors` in the lease's stderr, or `None` for no survivor:
/// how many there were, then up to [`MAX_NAMED`] of them by pid, name and arguments
/// (cut at [`ARGS_SHOWN`] characters).
#[must_use]
pub(crate) fn note(survivors: &[Survivor]) -> Option<String> {
    if survivors.is_empty() {
        return None;
    }
    let named: Vec<String> = survivors
        .iter()
        .take(MAX_NAMED)
        .map(|s| {
            let args: String = s.args.chars().take(ARGS_SHOWN).collect();
            let cut = if s.args.chars().count() > ARGS_SHOWN {
                "..."
            } else {
                ""
            };
            format!("pid {} {} [{args}{cut}]", s.pid, s.name)
        })
        .collect();
    let more = survivors.len().saturating_sub(MAX_NAMED);
    let more = if more > 0 {
        format!("; and {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "kbf: {} process(es) ignored SIGTERM and were killed: {}{more}\n",
        survivors.len(),
        named.join("; ")
    ))
}

#[cfg(test)]
mod tests;
