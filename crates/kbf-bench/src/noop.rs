//! `noop`: the no-op benchmark (docs/design/actions-as-jobs.md, section 10.1). It sends
//! Executes of actions that do nothing to one or more frontends, the arms, and records the
//! time from sending each Execute to its done operation, as JSON lines.
//!
//! **The action.** Its input root holds one file, `noop`, executable: the program of
//! [`crate::program`], which exits 0. Its Command is `./noop`, with the environment
//! variable [`ENV`] set to `<seed>-<arm>-<phase>-<run>`, so every run of every arm is an
//! action digest of its own; the Action carries the plan's platform properties (REAPI
//! 2.2's place for them, which kbf reads before the Command's). Every Execute sets
//! `skip_cache_lookup`. The action is cacheable, as a build's is: a frontend does what it
//! does for a build action.
//!
//! **The order.** All the blobs (the program, the input root's `Directory`, every
//! Command and Action) are sent to each arm first, by BatchUpdateBlobs, so the timed part
//! is the Executes alone. Then the serial phase, then the parallel phase. In each, the runs
//! are taken in blocks of `block` runs per arm, the arms' blocks alternating (block 0 of
//! every arm, in the order the arms are given, then block 1 of every arm, and so on), and
//! sent in that order through a window of `in_flight` Executes open at once: 1 in the
//! serial phase, so one run at a time, alternating between arms by block; the plan's
//! `in_flight` in the parallel phase, counted over all arms together, so the window
//! slides across block ends and the load at any moment falls on the arms alike.
//!
//! **What a run records** ([`Record`]): the arm, the phase, the block and the run; the
//! action digest and the operation name (to join with a frontend's logs); the time from
//! sending the Execute to the done operation, on the client; whether it succeeded (a done
//! operation, status OK, an `ActionResult` not from the cache, exit 0) and, if not, why;
//! and the per-lease phases the result's `ExecutedActionMetadata` carries: the worker, the
//! frontend's queued and worker times, and the input fetch, execution and output upload
//! (a daemon's own stamps, the fields of its `lease phases:` line; under the executor, the
//! task's claim, marked `claimed`). Nothing here asserts or judges a number: that is the
//! reader's (`js-analysis`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::{StreamExt as _, stream};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    Action, ActionResult, BatchUpdateBlobsRequest, Command, Digest, Directory, ExecuteRequest,
    ExecuteResponse, ExecutedActionMetadata, FileNode, Platform, batch_update_blobs_request,
    command, platform,
};
use kbf_proto::task_claim;
use prost::Message as _;
use prost_types::Timestamp;
use serde::Serialize;
use tokio::time::Instant;
use tonic::transport::Channel;

use crate::program::{self, Arch};
use crate::report::{Latency, MAX_ERRORS};
use crate::tree::digest_of;

/// The environment variable that makes every run's action new.
pub const ENV: &str = "KBF_BENCH_NOOP";

/// The program's name in the input root.
pub const PROGRAM: &str = "noop";

/// The most blobs one BatchUpdateBlobs call carries (every blob is a few hundred bytes).
const UPLOAD_BLOBS: usize = 500;

/// One frontend the runs are sent to.
#[derive(Clone, Debug)]
pub struct Arm {
    /// The arm's name in the records (`a`, `b`, `c`).
    pub name: String,
    /// The frontend's REAPI listener.
    pub channel: Channel,
}

/// What a run sends, and how many.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub instance_name: String,
    /// The Action's platform properties, as given; sorted here.
    pub platform: Vec<(String, String)>,
    pub arch: Arch,
    /// In every run's [`ENV`] value: another seed, other actions.
    pub seed: u64,
    /// Runs per arm, one at a time.
    pub serial_runs: u64,
    /// Runs per arm, `in_flight` at a time over all arms.
    pub parallel_runs: u64,
    /// The parallel phase's window; at least 1.
    pub in_flight: usize,
    /// Runs per arm in a block; at least 1.
    pub block: u64,
}

/// One of the two parts of a benchmark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// One run at a time.
    Serial,
    /// `in_flight` runs at a time.
    Parallel,
}

impl Phase {
    /// The name in the records and the table.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Serial => "serial",
            Self::Parallel => "parallel",
        }
    }
}

/// One run, one JSON line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    pub arm: String,
    pub phase: Phase,
    /// The phase's window.
    pub in_flight: usize,
    /// The Executes open, of every arm, when this one was sent, this one included.
    pub open: usize,
    /// The arm's block the run is in, from 0.
    pub block: u64,
    /// The run's number in its arm and phase, from 0.
    pub run: u64,
    /// The action digest, `<sha256>/<size>`.
    pub action: String,
    /// The operation's name, as the first message of the Execute stream named it.
    pub operation: Option<String>,
    /// From sending the Execute to receiving the done operation; `None` when no done
    /// operation came.
    pub execute_to_done_ms: Option<f64>,
    /// Done, status OK, a result not from the cache, and exit 0.
    pub ok: bool,
    /// Why the run is not `ok`.
    pub error: Option<String>,
    /// The result's exit code, when there is a result.
    pub exit_code: Option<i32>,
    /// The result's worker, when there is a result.
    pub worker: Option<String>,
    /// `input_fetch_ms`, `execution_ms` and `output_upload_ms` are a task's claim
    /// (`kbf_proto::task_claim`), not a daemon's measurement; `queued_ms` and
    /// `worker_ms` are the frontend's own times either way.
    pub claimed: bool,
    /// `queued_timestamp` to `worker_start_timestamp`.
    pub queued_ms: Option<f64>,
    /// `worker_start_timestamp` to `worker_completed_timestamp`.
    pub worker_ms: Option<f64>,
    pub input_fetch_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub output_upload_ms: Option<f64>,
}

/// One arm's runs of one phase, a row of the table.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub arm: String,
    pub phase: Phase,
    pub runs: u64,
    pub failed: u64,
    /// Execute to done of the runs that succeeded; `None` when none did.
    pub latency: Option<Latency>,
}

/// What a benchmark did.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub arch: Arch,
    pub seed: u64,
    pub in_flight: usize,
    pub block: u64,
    /// Per arm, in the order given, the serial row then the parallel one (a phase of no
    /// runs has none).
    pub rows: Vec<Row>,
    /// Runs that did not succeed.
    pub failed: u64,
    /// The first [`MAX_ERRORS`] of their errors, each with its arm, phase and run.
    pub errors: Vec<String>,
}

/// Why a benchmark stopped.
#[derive(Debug, thiserror::Error)]
pub enum NoopError {
    #[error("sending the actions to arm {arm}: {message}")]
    Upload { arm: String, message: String },
    #[error("--jsonl: {0}")]
    Write(#[from] std::io::Error),
}

/// One run to make: its arm (an index into the arms), phase, block and number.
#[derive(Clone, Debug)]
struct Run {
    arm: usize,
    phase: Phase,
    block: u64,
    index: u64,
    action: Digest,
}

/// One blob to send.
#[derive(Clone)]
struct Blob {
    digest: Digest,
    bytes: Vec<u8>,
}

impl Blob {
    fn of(bytes: Vec<u8>) -> Self {
        Self {
            digest: digest_of(&bytes),
            bytes,
        }
    }
}

/// What came back for one Execute.
#[derive(Default)]
struct Answer {
    operation: Option<String>,
    done_after: Option<Duration>,
    result: Option<ActionResult>,
    error: Option<String>,
}

/// The input root: the program alone, executable, and its blobs.
fn input_root(arch: Arch) -> (Digest, [Blob; 2]) {
    let program = Blob::of(program::noop(arch));
    let root = Directory {
        files: vec![FileNode {
            name: PROGRAM.to_owned(),
            digest: Some(program.digest.clone()),
            is_executable: true,
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let root = Blob::of(root.encode_to_vec());
    (root.digest.clone(), [program, root])
}

/// The action of run `index` of `arm` in `phase`, and its Command and Action blobs.
fn action(plan: &Plan, root: &Digest, arm: &str, phase: Phase, index: u64) -> [Blob; 2] {
    let mut properties: Vec<platform::Property> = plan
        .platform
        .iter()
        .map(|(name, value)| platform::Property {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    // REAPI: sorted by name, then by value.
    properties.sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value)));
    let platform = (!properties.is_empty()).then_some(Platform { properties });
    let command = Command {
        arguments: vec![format!("./{PROGRAM}")],
        environment_variables: vec![command::EnvironmentVariable {
            name: ENV.to_owned(),
            value: format!("{}-{arm}-{}-{index}", plan.seed, phase.name()),
        }],
        ..Command::default()
    };
    let command = Blob::of(command.encode_to_vec());
    let action = Action {
        command_digest: Some(command.digest.clone()),
        input_root_digest: Some(root.clone()),
        platform,
        ..Action::default()
    };
    [command, Blob::of(action.encode_to_vec())]
}

/// The runs of `phase` in the order they are sent: block by block, the arms alternating
/// within each.
fn order(
    plan: &Plan,
    actions: &BTreeMap<(usize, Phase, u64), Digest>,
    arms: usize,
    phase: Phase,
) -> Vec<Run> {
    let runs = match phase {
        Phase::Serial => plan.serial_runs,
        Phase::Parallel => plan.parallel_runs,
    };
    let mut order = Vec::new();
    for block in 0..runs.div_ceil(plan.block) {
        for arm in 0..arms {
            for index in block * plan.block..runs.min((block + 1) * plan.block) {
                order.push(Run {
                    arm,
                    phase,
                    block,
                    index,
                    action: actions[&(arm, phase, index)].clone(),
                });
            }
        }
    }
    order
}

/// Sends `blobs` to `arm`, [`UPLOAD_BLOBS`] a call.
async fn upload(arm: &Arm, blobs: &[Blob]) -> Result<(), NoopError> {
    let failed = |message: String| NoopError::Upload {
        arm: arm.name.clone(),
        message,
    };
    let mut cas = ContentAddressableStorageClient::new(arm.channel.clone());
    for chunk in blobs.chunks(UPLOAD_BLOBS) {
        let request = BatchUpdateBlobsRequest {
            requests: chunk
                .iter()
                .map(|blob| batch_update_blobs_request::Request {
                    digest: Some(blob.digest.clone()),
                    data: blob.bytes.clone(),
                    ..batch_update_blobs_request::Request::default()
                })
                .collect(),
            ..BatchUpdateBlobsRequest::default()
        };
        let answer = cas
            .batch_update_blobs(request)
            .await
            .map_err(|status| failed(format!("BatchUpdateBlobs: {}", status_text(&status))))?;
        for blob in answer.into_inner().responses {
            let status = blob.status.unwrap_or_default();
            if status.code != 0 {
                let hash = blob.digest.unwrap_or_default().hash;
                return Err(failed(format!(
                    "BatchUpdateBlobs refused {hash}: code {}: {}",
                    status.code, status.message
                )));
            }
        }
    }
    Ok(())
}

fn status_text(status: &tonic::Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
}

/// Sends the Execute of `action` and reads its stream to the done operation.
async fn execute(channel: Channel, instance_name: String, action: Digest) -> Answer {
    let mut answer = Answer::default();
    let request = ExecuteRequest {
        instance_name,
        action_digest: Some(action),
        skip_cache_lookup: true,
        ..ExecuteRequest::default()
    };
    let sent = Instant::now();
    let mut updates = match ExecutionClient::new(channel).execute(request).await {
        Ok(stream) => stream.into_inner(),
        Err(status) => {
            answer.error = Some(format!("Execute: {}", status_text(&status)));
            return answer;
        }
    };
    let done = loop {
        match updates.message().await {
            Ok(Some(update)) => {
                answer.operation.get_or_insert(update.name.clone());
                if update.done {
                    break update;
                }
            }
            Ok(None) => {
                answer.error =
                    Some("the Execute stream ended before the operation was done".to_owned());
                return answer;
            }
            Err(status) => {
                answer.error = Some(format!("the Execute stream: {}", status_text(&status)));
                return answer;
            }
        }
    };
    answer.done_after = Some(sent.elapsed());
    match response(done) {
        Ok(response) => {
            answer.error = verdict(&response);
            answer.result = response.result;
        }
        Err(error) => answer.error = Some(error),
    }
    answer
}

/// The `ExecuteResponse` a done operation carries.
fn response(done: Operation) -> Result<ExecuteResponse, String> {
    match done.result {
        None => Err("the done operation has no result".to_owned()),
        Some(operation::Result::Error(status)) => Err(format!(
            "the operation failed: code {}: {}",
            status.code, status.message
        )),
        Some(operation::Result::Response(any)) => ExecuteResponse::decode(any.value.as_slice())
            .map_err(|e| format!("the done operation's response is not an ExecuteResponse: {e}")),
    }
}

/// Why `response` is not a successful run of `./noop`, if it is not.
fn verdict(response: &ExecuteResponse) -> Option<String> {
    let status = response.status.clone().unwrap_or_default();
    if status.code != 0 {
        return Some(format!(
            "the ExecuteResponse's status: code {}: {}",
            status.code, status.message
        ));
    }
    let Some(result) = &response.result else {
        return Some("the ExecuteResponse has no ActionResult".to_owned());
    };
    if response.cached_result {
        return Some(
            "answered from the action cache, though the Execute set skip_cache_lookup".to_owned(),
        );
    }
    (result.exit_code != 0).then(|| format!("./{PROGRAM} exited {}", result.exit_code))
}

/// The milliseconds from `start` to `end`, when both are stamped and in order.
fn span(start: Option<&Timestamp>, end: Option<&Timestamp>) -> Option<f64> {
    let (start, end) = (start?, end?);
    let seconds = i128::from(end.seconds) - i128::from(start.seconds);
    let nanos = seconds * 1_000_000_000 + i128::from(end.nanos) - i128::from(start.nanos);
    (nanos >= 0).then(|| nanos as f64 / 1e6)
}

/// The record of `run`, sent with `open` Executes open, which got `answer`.
fn record(arms: &[Arm], run: &Run, in_flight: usize, open: usize, answer: Answer) -> Record {
    let metadata = answer
        .result
        .as_ref()
        .map(|r| r.execution_metadata.clone().unwrap_or_default());
    let phases = |m: &ExecutedActionMetadata| {
        [
            span(
                m.queued_timestamp.as_ref(),
                m.worker_start_timestamp.as_ref(),
            ),
            span(
                m.worker_start_timestamp.as_ref(),
                m.worker_completed_timestamp.as_ref(),
            ),
            span(
                m.input_fetch_start_timestamp.as_ref(),
                m.input_fetch_completed_timestamp.as_ref(),
            ),
            span(
                m.execution_start_timestamp.as_ref(),
                m.execution_completed_timestamp.as_ref(),
            ),
            span(
                m.output_upload_start_timestamp.as_ref(),
                m.output_upload_completed_timestamp.as_ref(),
            ),
        ]
    };
    let [
        queued_ms,
        worker_ms,
        input_fetch_ms,
        execution_ms,
        output_upload_ms,
    ] = metadata.as_ref().map_or([None; 5], phases);
    Record {
        arm: arms[run.arm].name.clone(),
        phase: run.phase,
        in_flight,
        open,
        block: run.block,
        run: run.index,
        action: format!("{}/{}", run.action.hash, run.action.size_bytes),
        operation: answer.operation,
        execute_to_done_ms: answer.done_after.map(|d| d.as_secs_f64() * 1e3),
        ok: answer.error.is_none(),
        error: answer.error,
        exit_code: answer.result.as_ref().map(|r| r.exit_code),
        worker: metadata.as_ref().map(|m| m.worker.clone()),
        claimed: metadata.as_ref().is_some_and(task_claim::is_marked),
        queued_ms,
        worker_ms,
        input_fetch_ms,
        execution_ms,
        output_upload_ms,
    }
}

/// What the table is made of, per arm and phase.
#[derive(Default)]
struct Tally {
    runs: u64,
    failed: u64,
    times: Vec<Duration>,
}

/// Runs the benchmark `plan` describes against `arms`, writing each run's [`Record`] to
/// `out` as one JSON line, flushed, as it finishes; returns the summary, whether every
/// run succeeded or not.
///
/// # Errors
/// An arm refused a blob or the BatchUpdateBlobs call (before any Execute); `out` could
/// not be written.
pub async fn run(
    plan: &Plan,
    arms: &[Arm],
    out: &mut (dyn Write + Send),
) -> Result<Summary, NoopError> {
    let (root, root_blobs) = input_root(plan.arch);
    let mut actions = BTreeMap::new();
    for (at, arm) in arms.iter().enumerate() {
        let mut blobs = root_blobs.to_vec();
        for (phase, runs) in [
            (Phase::Serial, plan.serial_runs),
            (Phase::Parallel, plan.parallel_runs),
        ] {
            for index in 0..runs {
                let [command, action] = action(plan, &root, &arm.name, phase, index);
                actions.insert((at, phase, index), action.digest.clone());
                blobs.extend([command, action]);
            }
        }
        upload(arm, &blobs).await?;
    }
    let mut tallies: BTreeMap<(usize, Phase), Tally> = BTreeMap::new();
    let mut errors = Vec::new();
    for (phase, in_flight) in [(Phase::Serial, 1), (Phase::Parallel, plan.in_flight)] {
        let open = AtomicUsize::new(0);
        let runs = order(plan, &actions, arms.len(), phase)
            .into_iter()
            .map(|run| {
                let (open, channel) = (&open, arms[run.arm].channel.clone());
                let instance_name = plan.instance_name.clone();
                async move {
                    let now = open.fetch_add(1, Ordering::SeqCst) + 1;
                    let answer = execute(channel, instance_name, run.action.clone()).await;
                    open.fetch_sub(1, Ordering::SeqCst);
                    let done_after = answer.done_after;
                    (record(arms, &run, in_flight, now, answer), run, done_after)
                }
            });
        let mut finished = stream::iter(runs).buffer_unordered(in_flight);
        while let Some((record, run, done_after)) = finished.next().await {
            let mut line = serde_json::to_vec(&record).map_err(std::io::Error::from)?;
            line.push(b'\n');
            out.write_all(&line)?;
            out.flush()?;
            let tally = tallies.entry((run.arm, phase)).or_default();
            tally.runs += 1;
            match (&record.error, done_after) {
                (None, Some(took)) => tally.times.push(took),
                (error, _) => {
                    tally.failed += 1;
                    if errors.len() < MAX_ERRORS {
                        let why = error.as_deref().unwrap_or_default();
                        errors.push(format!(
                            "{} {} run {}: {why}",
                            record.arm,
                            phase.name(),
                            record.run
                        ));
                    }
                }
            }
        }
    }
    let rows: Vec<Row> = tallies
        .into_iter()
        .map(|((arm, phase), tally)| Row {
            arm: arms[arm].name.clone(),
            phase,
            runs: tally.runs,
            failed: tally.failed,
            latency: (!tally.times.is_empty()).then(|| Latency::of(tally.times)),
        })
        .collect();
    Ok(Summary {
        arch: plan.arch,
        seed: plan.seed,
        in_flight: plan.in_flight,
        block: plan.block,
        failed: rows.iter().map(|r| r.failed).sum(),
        rows,
        errors,
    })
}

impl Summary {
    /// Every run succeeded.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.failed == 0
    }

    /// The summary as a table for a terminal: per arm and phase, the runs, the failures
    /// and the nearest-rank percentiles of Execute to done of the runs that succeeded.
    #[must_use]
    pub fn table(&self) -> String {
        let mut out = format!(
            "noop {}, seed {}, blocks of {}, parallel {} in flight\n\
             {:<12}{:<10}{:>7}{:>8}{:>11}{:>11}{:>11}{:>11}\n",
            self.arch.name(),
            self.seed,
            self.block,
            self.in_flight,
            "arm",
            "phase",
            "runs",
            "failed",
            "p50 ms",
            "p90 ms",
            "p99 ms",
            "max ms",
        );
        for row in &self.rows {
            let _ = write!(
                out,
                "{:<12}{:<10}{:>7}{:>8}",
                row.arm,
                row.phase.name(),
                row.runs,
                row.failed
            );
            let _ = match &row.latency {
                Some(l) => writeln!(
                    out,
                    "{:>11.3}{:>11.3}{:>11.3}{:>11.3}",
                    l.p50_ms, l.p90_ms, l.p99_ms, l.max_ms
                ),
                None => writeln!(out, "{:>11}{:>11}{:>11}{:>11}", "-", "-", "-", "-"),
            };
        }
        let _ = writeln!(out, "failed: {}", self.failed);
        for e in &self.errors {
            let _ = writeln!(out, "  {e}");
        }
        out
    }
}
