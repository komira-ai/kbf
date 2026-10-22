//! `kbf cluster finalize`: the answers of `POST /v1/finalize` and `GET /v1/finalize`
//! (`docs/api.md`) as a table a person reads, and the wait for every node to be done.
//!
//! - **`--status`** prints the farm's target and one line per member: the build it runs, the
//!   newest log version its build can raise to, the version its log is at, its state against
//!   the target and why it is blocked ([`show`]).
//! - **`--wait`** prints the answer of the `POST`, then asks `GET /v1/finalize` every
//!   [`POLL`] until every node is `done`, and prints the table ([`wait`]). It gives up after
//!   the timeout (`--timeout-secs`, default [`DEFAULT_TIMEOUT_SECS`]) and exits 1 with the
//!   table of where the nodes were: a raise happens at a node's next heartbeat, so a node that
//!   is down, or on a build that cannot raise, holds it up.

use std::io::Write;
use std::time::{Duration, Instant};

use kbf_server::api::FinalizeView;
use kbf_server::api_client::{ClientError, Reply};
use kbf_server::members::FinalizeState;

/// How long `--wait` polls when `--timeout-secs` is not given: ten minutes, a few dozen
/// heartbeats of a node at the default interval.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// How long `--wait` waits between two reads of the report.
pub const POLL: Duration = Duration::from_secs(2);

/// When `--wait` stops and how often it asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Polling {
    /// How long to wait for every node to be done.
    pub timeout: Duration,
    /// The wait between two reads of the report.
    pub every: Duration,
}

/// The report in `body`, or why it is none.
fn parse(body: &str) -> Result<FinalizeView, String> {
    serde_json::from_str(body).map_err(|e| format!("the answer is not a finalize report: {e}"))
}

/// The report `answer` carries, or why there is none: the call failed, the status is not 2xx,
/// or the body is no report.
fn read(answer: Result<Reply, ClientError>) -> Result<FinalizeView, String> {
    let reply = answer.map_err(|e| e.to_string())?;
    if !reply.is_success() {
        return Err(format!("{} {}", reply.status, reply.body.trim_end()));
    }
    parse(&reply.body)
}

/// One line of the table: the cells in columns, without trailing spaces.
fn row(cells: [&str; 6]) -> String {
    let [member, build, runs, log, state, reason] = cells;
    let line = format!("{member:<7} {build:<12} {runs:<5} {log:<4} {state:<8} {reason}");
    format!("{}\n", line.trim_end())
}

/// `version` as a cell, `-` when the node did not say.
fn number(version: Option<u32>) -> String {
    version.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// The report as a table: a line for the target, a header and a line per member. The build is
/// the first 12 characters of its commit.
#[must_use]
pub fn table(view: &FinalizeView) -> String {
    let mut out = match view.target {
        Some(target) => format!("finalize target {target}\n"),
        None => "no finalize target is set\n".to_owned(),
    };
    out.push_str(&row(["MEMBER", "BUILD", "RUNS", "LOG", "STATE", "REASON"]));
    for node in &view.nodes {
        let commit = node.build_commit.as_deref().unwrap_or("-");
        let build = commit.get(..12).unwrap_or(commit);
        out.push_str(&row([
            &node.member.to_string(),
            build,
            &number(node.runs_log),
            &number(node.log_version),
            node.state.name(),
            &node.reason,
        ]));
    }
    out
}

/// Prints the report in `reply`, the 2xx answer of `GET /v1/finalize`, to `out`, and returns
/// the exit status: 0. An answer that is no report is said on `err` and is exit 1.
pub fn show(reply: &Reply, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    match parse(&reply.body) {
        Ok(view) => {
            let _ = write!(out, "{}", table(&view));
            0
        }
        Err(why) => {
            let _ = writeln!(err, "kbf cluster: {why}");
            1
        }
    }
}

/// Reads the report with `fetch` until every node is `done` (exit 0) and prints the table; if
/// `polling.timeout` passes first, prints the table of where the nodes were and exits 1. A
/// call that fails, a status that is not 2xx and an answer that is no report are said on
/// `err` at once and are exit 1: they are not waited out. The first read is made at once. The
/// wait ends at the timeout plus the time of the read that is under way at it.
pub fn wait(
    fetch: &mut dyn FnMut() -> Result<Reply, ClientError>,
    polling: &Polling,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let deadline = Instant::now() + polling.timeout;
    loop {
        let view = match read(fetch()) {
            Ok(view) => view,
            Err(why) => {
                let _ = writeln!(err, "kbf cluster: {why}");
                return 1;
            }
        };
        if view
            .nodes
            .iter()
            .all(|node| node.state == FinalizeState::Done)
        {
            let _ = write!(out, "{}", table(&view));
            return 0;
        }
        if Instant::now() >= deadline {
            let _ = write!(out, "{}", table(&view));
            let secs = polling.timeout.as_secs();
            let _ = writeln!(
                err,
                "kbf cluster: timed out after {secs} s: not every node is done"
            );
            return 1;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(polling.every.min(left));
    }
}

#[cfg(test)]
#[path = "finalize_tests.rs"]
mod tests;
