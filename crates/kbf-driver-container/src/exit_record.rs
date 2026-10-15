//! Reading a container's exit from Podman's record once `podman start --attach` has
//! returned, and waiting, within a deadline, while the record does not hold one yet.
//!
//! Podman 4.9 (`libpod/container_internal.go`): `checkExitFile` moves a running
//! container to `stopped` and `handleExitFile` sets its `ExitCode` (from conmon's exit
//! file) and `FinishedAt` (that file's time) in the same saved state; the clean-up
//! (`cleanupRuntime`, run by `podman container cleanup`) later moves `stopped` to
//! `exited` without changing either. When no exit file appears, Podman records
//! `ExitCode` -1 with `FinishedAt` set. So `FinishedAt` is the field that says an exit
//! was recorded: a `stopped` or `exited` record without it holds no exit code, and its
//! `ExitCode` (0 until set) is not the action's. The driver never reads the code's
//! value as that mark. On the path a started container takes, Podman 4.9 saves
//! `stopped` with both already set, so a record without `FinishedAt` is not expected
//! there; the wait is the guard should one be read, not a common path.
//!
//! The wait never starts the container again: the action ran, and it is not idempotent.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;

use crate::podman::Ended;

/// The `podman inspect` format of a record: status, exit code, and whether
/// `FinishedAt` is unset (`true`: no exit recorded).
pub(crate) const FORMAT: &str =
    "--format={{.State.Status}} {{.State.ExitCode}} {{.State.FinishedAt.IsZero}}";

/// How long after `podman start --attach` returns the driver waits for Podman to record
/// the container's exit. On a loaded node (issue #658) Podman's clean-up came 24 to 41
/// seconds after the container died, and `podman start` itself gives the record 20
/// seconds once conmon has gone (`WaitForExit`); two minutes is about three times the
/// slowest seen. Past it the lease fails as the farm's, naming Podman's last record.
pub(crate) const DEADLINE: Duration = Duration::from_secs(120);

/// How often the record is read again while it holds no exit. Each read is a `podman
/// inspect` process on a node that is already loaded, so not faster.
pub(crate) const EVERY: Duration = Duration::from_millis(500);

/// What one read of the record says.
#[derive(Debug, PartialEq, Eq)]
enum Reading {
    /// The record is final: an exit, or a container that did not run to one.
    Final(Ended),
    /// `stopped` or `exited` with no exit recorded yet (`FinishedAt` unset).
    Unrecorded,
}

/// Reads one `podman inspect --format=<FORMAT>` answer.
fn read(text: &str) -> Result<Reading, String> {
    match text.split_whitespace().collect::<Vec<_>>()[..] {
        ["stopped" | "exited", _, "true"] => Ok(Reading::Unrecorded),
        [status @ ("stopped" | "exited"), code, "false"] => {
            let code: i32 = code
                .parse()
                .map_err(|_| format!("podman inspect: exit code {code:?}"))?;
            // -1: Podman recorded that it never read an exit.
            Ok(Reading::Final(if code < 0 {
                Ended::NotRun(format!("{status} {code}"))
            } else {
                Ended::Exited(code)
            }))
        }
        [status, code, _] => Ok(Reading::Final(Ended::NotRun(format!("{status} {code}")))),
        _ => Ok(Reading::Final(Ended::NotRun(text.trim().to_owned()))),
    }
}

/// Why no exit was read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NoExit {
    /// `podman inspect` failed, or answered in a way that is not a record.
    Inspect(String),
    /// The record held no exit for the whole deadline. `last` is Podman's last answer
    /// (status and exit code), `None` if no read finished in time.
    Unrecorded {
        last: Option<String>,
        after: Duration,
    },
}

impl NoExit {
    /// The lease's failure: `podman inspect`'s, or, past the deadline, Podman's last
    /// record and what `podman start` wrote to `stderr` (the file it was given).
    pub(crate) async fn reason(self, stderr: &Path) -> String {
        let (last, after) = match self {
            Self::Inspect(why) => return why,
            Self::Unrecorded { last, after } => (last, after),
        };
        let last = last.map_or_else(
            || "no `podman inspect` answered".to_owned(),
            |last| format!("its last record is {last:?} with FinishedAt unset"),
        );
        let said = match crate::program::said(stderr).await {
            Ok(said) if said.is_empty() => "(nothing on stderr)".to_owned(),
            Ok(said) => said,
            Err(e) => format!("({}: {e})", stderr.display()),
        };
        format!(
            "podman recorded no exit for the container {} s after `podman start` returned: \
             {last}; podman start: {said}",
            after.as_secs()
        )
    }
}

/// Reads the record with `inspect` until it is final, re-reading every `every` while it
/// holds no exit, for at most `deadline` (a read still running then is dropped).
pub(crate) async fn wait<F, Fut>(
    mut inspect: F,
    deadline: Duration,
    every: Duration,
) -> Result<Ended, NoExit>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let give_up = Instant::now() + deadline;
    let mut last = None;
    loop {
        let Ok(answer) = tokio::time::timeout_at(give_up, inspect()).await else {
            break;
        };
        let text = answer.map_err(NoExit::Inspect)?;
        match read(&text).map_err(NoExit::Inspect)? {
            Reading::Final(ended) => return Ok(ended),
            Reading::Unrecorded => {
                last = Some(
                    text.split_whitespace()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }
        let now = Instant::now();
        if now >= give_up {
            break;
        }
        // The last read is at the deadline.
        tokio::time::sleep_until((now + every).min(give_up)).await;
    }
    Err(NoExit::Unrecorded {
        last,
        after: deadline,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use futures::FutureExt as _;
    use futures::future::BoxFuture;

    use super::*;

    /// An `inspect` that answers `answers` in turn, the last one for ever, and counts
    /// its calls. `HANG` never answers; `err:<why>` fails with `<why>`. One closure
    /// type for every test, so the coverage job's one instantiation of `wait` sees
    /// every path.
    fn answering(
        answers: &[&str],
    ) -> (
        impl FnMut() -> BoxFuture<'static, Result<String, String>> + use<>,
        Arc<Mutex<usize>>,
    ) {
        let mut queue: VecDeque<String> = answers.iter().map(|a| (*a).to_owned()).collect();
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let inspect = move || {
            *counted.lock().expect("count") += 1;
            let answer = if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            }
            .unwrap_or_default();
            if answer == HANG {
                return std::future::pending().boxed();
            }
            let answer = match answer.strip_prefix("err:") {
                Some(why) => Err(why.to_owned()),
                None => Ok(answer),
            };
            std::future::ready(answer).boxed()
        };
        (inspect, calls)
    }

    const HANG: &str = "hang";
    const SHORT: Duration = Duration::from_millis(300);
    const TICK: Duration = Duration::from_millis(10);

    /// Catches a record without `FinishedAt` taken as an exit (its unset code is 0, so
    /// an action that exited 3 would be cached as a success) or as a container that
    /// never ran: the wait reads again until the record holds the exit, and takes its
    /// code.
    #[tokio::test]
    async fn an_unrecorded_exit_is_read_again_until_recorded() {
        let (inspect, calls) = answering(&[
            "stopped 0 true",
            "stopped 0 true",
            "exited 0 true",
            "exited 3 false",
        ]);
        let ended = wait(inspect, Duration::from_secs(10), TICK).await;
        assert_eq!(ended, Ok(Ended::Exited(3)));
        assert_eq!(*calls.lock().expect("count"), 4);
    }

    /// Catches a wait with no end, and a failure that does not say what Podman last
    /// recorded: a record that never holds the exit fails after the deadline (not
    /// before), naming its last status and code.
    #[tokio::test]
    async fn a_record_that_never_holds_the_exit_fails_at_the_deadline() {
        let (inspect, calls) = answering(&["stopped 0 true"]);
        let began = std::time::Instant::now();
        let ended = wait(inspect, SHORT, TICK).await;
        let took = began.elapsed();
        assert_eq!(
            ended,
            Err(NoExit::Unrecorded {
                last: Some("stopped 0".to_owned()),
                after: SHORT
            })
        );
        assert!(took >= SHORT, "gave up early, after {took:?}");
        assert!(took < SHORT * 4, "kept waiting, {took:?}");
        assert!(*calls.lock().expect("count") > 1, "never read again");
    }

    /// Catches a read that hangs holding the lease past the deadline: it is dropped
    /// then, and the failure says no read finished.
    #[tokio::test]
    async fn a_read_that_hangs_ends_at_the_deadline() {
        let (inspect, _) = answering(&[HANG]);
        let ended = wait(inspect, SHORT, TICK).await;
        assert_eq!(
            ended,
            Err(NoExit::Unrecorded {
                last: None,
                after: SHORT
            })
        );
    }

    /// Catches a delay added to the common case: a recorded exit (`exited`, or
    /// `stopped` before Podman's clean-up) is taken from the first read, with no sleep.
    #[tokio::test]
    async fn a_recorded_exit_is_taken_from_the_first_read() {
        for (answer, code) in [
            ("exited 0 false", 0),
            ("stopped 3 false", 3),
            ("exited 137 false", 137),
        ] {
            let (inspect, calls) = answering(&[answer]);
            let began = std::time::Instant::now();
            let ended = wait(inspect, Duration::from_secs(10), Duration::from_secs(5)).await;
            assert_eq!(ended, Ok(Ended::Exited(code)), "{answer}");
            assert_eq!(*calls.lock().expect("count"), 1, "{answer}");
            assert!(began.elapsed() < Duration::from_secs(1), "{answer} waited");
        }
    }

    /// Catches Podman's record of an exit it never read (`-1`) taken as an exit code,
    /// and a container that is not `stopped` or `exited` read as having exited: both are
    /// final at once, as a container that did not run to an exit, named by status and
    /// code.
    #[test]
    fn what_is_not_a_recorded_exit_is_named() {
        for (text, state) in [
            ("stopped -1 false", "stopped -1"),
            ("exited -1 false", "exited -1"),
            ("created 0 true", "created 0"),
            ("running 0 true", "running 0"),
            ("weird", "weird"),
            ("", ""),
        ] {
            assert_eq!(
                read(text),
                Ok(Reading::Final(Ended::NotRun(state.to_owned()))),
                "{text:?}"
            );
        }
        assert_eq!(read("exited 0 true\n"), Ok(Reading::Unrecorded));
        assert_eq!(
            read("exited many false"),
            Err("podman inspect: exit code \"many\"".to_owned())
        );
    }

    /// Catches a deadline failure that does not say what Podman last recorded or what
    /// `podman start` said, or that ends at a dangling colon (issue #658); and an
    /// inspect failure reworded.
    #[tokio::test]
    async fn a_failure_names_podmans_record_and_start() {
        // Beside the test binary, inside the target directory.
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join("exit-record");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        let stderr = dir.join("stderr");
        let after = Duration::from_secs(120);
        let unrecorded = |last: Option<&str>| NoExit::Unrecorded {
            last: last.map(str::to_owned),
            after,
        };

        std::fs::write(&stderr, "Error: busy\n").expect("write");
        assert_eq!(
            unrecorded(Some("stopped 0")).reason(&stderr).await,
            "podman recorded no exit for the container 120 s after `podman start` returned: \
             its last record is \"stopped 0\" with FinishedAt unset; podman start: Error: busy"
        );
        std::fs::write(&stderr, "").expect("write");
        assert_eq!(
            unrecorded(None).reason(&stderr).await,
            "podman recorded no exit for the container 120 s after `podman start` returned: \
             no `podman inspect` answered; podman start: (nothing on stderr)"
        );
        std::fs::remove_file(&stderr).expect("rm");
        let gone = unrecorded(None).reason(&stderr).await;
        assert!(
            gone.ends_with(&format!(
                "podman start: ({}: No such file or directory (os error 2))",
                stderr.display()
            )),
            "{gone}"
        );
        std::fs::remove_dir(&dir).expect("rm scratch");
        let why = NoExit::Inspect("podman inspect: refused".to_owned());
        assert_eq!(why.reason(&stderr).await, "podman inspect: refused");
    }

    /// Catches an inspect failure, or an answer that is not a record, waited out
    /// instead of reported.
    #[tokio::test]
    async fn an_inspect_failure_is_reported_at_once() {
        for (answer, why) in [
            ("err:podman inspect: refused", "podman inspect: refused"),
            ("exited many false", "podman inspect: exit code \"many\""),
        ] {
            let (inspect, calls) = answering(&[answer]);
            let ended = wait(inspect, Duration::from_secs(10), TICK).await;
            assert_eq!(ended, Err(NoExit::Inspect(why.to_owned())), "{answer}");
            assert_eq!(*calls.lock().expect("count"), 1, "{answer}");
        }
    }
}
