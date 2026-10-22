//! Ending an action's processes politely before the kill.
//!
//! A process that is killed with SIGKILL runs nothing on its way out. On macOS that
//! matters for one kind of leftover: POSIX shared memory and semaphore names cannot be
//! listed there, so nothing can remove the ones a killed workload made. Python's
//! `multiprocessing` makes them through a resource tracker process, which unlinks every
//! name it was told about once its workload has gone (the pipe from the workload
//! closes). Killed together with the workload, the tracker unlinks nothing and the names
//! stay until the machine restarts. With SIGTERM first, a workload that ends on it
//! closes the pipe and the tracker has the grace to do its work; a workload that ignores
//! SIGTERM keeps the pipe open, and both are killed when the grace ends.
//!
//! So both places that end a lease's processes, the end of the lease
//! ([`crate::procs`]' process tree and the slot user's processes) and the slot's clean,
//! send SIGTERM first and give the processes up to a grace period to end
//! (`--native-term-grace-ms`, [`crate::NativeConfig::term_grace`]), and only then
//! SIGKILL whatever is left. A grace of zero is SIGKILL at once. [`term_first`] is that
//! first half, with the signals, the process table and the clock passed in, so that it
//! runs under test on every platform.
//!
//! A process that is still alive when the grace ends outlasted SIGTERM (it ignored it, or
//! it was started after it was sent), and is named in the lease's stderr ([`note`]) as
//! that action's own hermeticity failure: its pid, name and arguments. The action is not
//! failed for it. SIGTERM is sent once, to the processes alive when the round starts.
//!
//! The grace is extra to the wait for SIGKILL to take effect (`kill_wait`), which is
//! unchanged, and is at most [`MAX_GRACE`]. It is outside the action's timeout and the
//! memory watch: both have ended the action before it starts and neither runs during it,
//! so the longest an ending takes is the grace (60 s at most) plus `kill_wait`.
//!
//! What this cannot see: shared memory made without a tracker (Python's
//! `SharedMemory(track=False)`, or C code calling `shm_open` and exiting without
//! `shm_unlink`) is not removed by anything on macOS, and it is not reported either:
//! the system offers no way to list the names, so there is nothing to compare.

use std::time::Duration;

/// The longest grace there is: a larger `--native-term-grace-ms` is cut to this.
pub const MAX_GRACE: Duration = Duration::from_secs(60);

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
            // Only the ones [`note`] names are read: reading a process's arguments is
            // costly, and there can be thousands.
            return Ok(left
                .into_iter()
                .enumerate()
                .map(|(i, pid)| {
                    if i < MAX_NAMED {
                        describe(pid)
                    } else {
                        Survivor::of(pid, None)
                    }
                })
                .collect());
        }
        pause();
    }
}

/// What [`term_first`] gave, for a caller that kills next whatever happened: an error
/// is logged at WARN as `what` failed and no survivor is named, so that the kill that
/// follows still runs.
pub(crate) fn lenient(round: Result<Vec<Survivor>, String>, what: &str) -> Vec<Survivor> {
    round.unwrap_or_else(|why| {
        tracing::warn!("{what}: SIGTERM first did not complete ({why}); killing now");
        Vec::new()
    })
}

/// What `list` reads for the SIGTERM round, or `T::default()` without calling it when
/// `grace` is zero: [`term_first`] looks at nothing then, so the read would be wasted.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn for_round<T: Default>(grace: Duration, list: impl FnOnce() -> T) -> T {
    if grace.is_zero() {
        T::default()
    } else {
        list()
    }
}

/// The SIGTERM round `round` ([`lenient`]), then `kill` (given the survivors), which runs whatever the round
/// gave. Returns the survivors with what `kill` returned.
///
/// # Errors
/// `kill` failed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn then_kill<K>(
    round: Result<Vec<Survivor>, String>,
    what: &str,
    kill: impl FnOnce(&[Survivor]) -> Result<K, String>,
) -> Result<(Vec<Survivor>, K), String> {
    let survivors = lenient(round, what);
    kill(&survivors).map(|killed| (survivors, killed))
}

/// `text` with control characters shown as escapes, so that an argument cannot add a
/// line to a log.
fn plain(text: &str) -> String {
    text.chars().flat_map(char::escape_debug).collect()
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
            let args: String = plain(&s.args.chars().take(ARGS_SHOWN).collect::<String>());
            let cut = if s.args.chars().count() > ARGS_SHOWN {
                "..."
            } else {
                ""
            };
            format!("pid {} {} [{args}{cut}]", s.pid, plain(&s.name))
        })
        .collect();
    let more = survivors.len().saturating_sub(MAX_NAMED);
    let more = if more > 0 {
        format!("; and {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "kbf: {} process(es) outlasted SIGTERM and were killed: {}{more}\n",
        survivors.len(),
        named.join("; ")
    ))
}

/// Like [`note`] for the node's log: pids and names, no arguments (those can hold a
/// secret), without the line end.
#[must_use]
pub(crate) fn brief(survivors: &[Survivor]) -> Option<String> {
    if survivors.is_empty() {
        return None;
    }
    let named: Vec<String> = survivors
        .iter()
        .take(MAX_NAMED)
        .map(|s| format!("pid {} {}", s.pid, plain(&s.name)))
        .collect();
    Some(format!(
        "{} process(es) outlasted SIGTERM and were killed: {}",
        survivors.len(),
        named.join("; ")
    ))
}

#[cfg(test)]
mod tests;
