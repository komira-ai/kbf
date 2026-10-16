//! The server's `action usage:` line (`kbf_server::action_usage`), in a test binary of
//! its own: the log is read through a global subscriber, which no other test binary's
//! events can reach. Each test here picks its lines out by the shape keys of its own
//! actions, so the tests of this binary can share the subscriber and run at once.
//!
//! A buck2-shaped Execute names its argv and outputs in its `Command`; the daemon
//! reports results whose `ResourceUsage` carries the lease cgroup's counters, for an
//! OK run and for a run that failed (out of memory, timed out, the farm's fault). The
//! line carries the counters, the shape key and how the lease ended.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_proto::google::rpc;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    Action, ActionResult, Command, Directory, ExecuteRequest, ExecutedActionMetadata,
    RequestMetadata, ToolDetails,
};
use kbf_proto::worker::{
    self, CgroupUsage, MemoryKill, NumaNodeBytes, ResourceUsage, daemon_message, server_message,
};
use kbf_server::action_usage::{Shape, ShapeKey};
use kbf_types::LeaseId;
use prost::Message;
use support::{Blob, Cell, Job, done, output, ran, report_every_run};
use tonic::metadata::MetadataValue;
use tonic::{Code, Request};

/// Where the global subscriber writes: every line, without colour.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The binary's log, its subscriber installed by the first test to ask.
fn log() -> &'static Log {
    static LOG: OnceLock<Log> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        log
    })
}

/// The lines logged so far that contain `what` and name `key`.
fn lines(what: &str, key: ShapeKey) -> Vec<String> {
    let text = String::from_utf8(log().0.lock().expect("log").clone()).expect("UTF-8");
    let key = format!("key={key}");
    text.lines()
        .filter(|l| l.contains(what) && l.split(' ').any(|word| word == key))
        .map(str::to_owned)
        .collect()
}

/// `result` carrying `usage` the way a daemon sends it.
fn with_usage(mut result: ActionResult, usage: &ResourceUsage) -> ActionResult {
    result.execution_metadata = Some(ExecutedActionMetadata {
        auxiliary_metadata: vec![usage.to_any()],
        ..ExecutedActionMetadata::default()
    });
    result
}

/// A failed `Result` for `lease` with status `code`, carrying `usage` in an otherwise
/// empty `action_result`, as the daemon reports a lease that ended without a result
/// (`carry_usage`).
fn failed_with(
    lease: Option<worker::LeaseId>,
    code: Code,
    usage: &ResourceUsage,
) -> worker::Result {
    worker::Result {
        lease_id: lease,
        status: Some(rpc::Status {
            code: code as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: Some(with_usage(ActionResult::default(), usage)),
        ..worker::Result::default()
    }
}

fn usage() -> ResourceUsage {
    ResourceUsage {
        wall_micros: 2_000_000,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(3_000_000),
            cpu_user_micros: Some(2_000_000),
            cpu_system_micros: Some(1_000_000),
            cpu_periods: Some(0),
            cpu_throttled_periods: Some(0),
            cpu_throttled_micros: Some(0),
            memory_peak_bytes: Some(4096),
            numa: vec![
                NumaNodeBytes {
                    node: 0,
                    bytes: 600,
                },
                NumaNodeBytes {
                    node: 1,
                    bytes: 400,
                },
            ],
        }),
        ..ResourceUsage::default()
    }
}

/// The cgroup fields of [`usage`], as the line shows them.
const FIELDS: &str = "wall_us=2000000 cpu_usage_us=3000000 cpu_user_us=2000000 cpu_system_us=1000000 \
                      cpu_periods=0 cpu_throttled=0 throttled_us=0 mem_peak=4096 parallelism=1.50 \
                      numa_bytes=0:600,1:400 numa_spread=0.40";

/// A job whose `Command` runs `argv` and names `outputs`, as buck2 sends one.
fn buck_job(argv: &[&str], outputs: &[&str]) -> (Job, ShapeKey) {
    let strings = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let command = Command {
        arguments: strings(argv),
        output_paths: strings(outputs),
        ..Command::default()
    };
    let key = Shape::of(&command.arguments, &command.output_paths).key;
    let input = Blob::new(format!("input of {argv:?}"));
    let root = Blob::of(&Directory::default());
    let command = Blob::of(&command);
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        ..Action::default()
    });
    let job = Job {
        action,
        command,
        root,
        input,
    };
    (job, key)
}

/// The key of a job built by `support::Job::new(argv, ..)`: one argument, no outputs.
fn job_key(argv: &str) -> ShapeKey {
    Shape::of(&[argv.to_owned()], &[]).key
}

fn lease_text(lease: Option<worker::LeaseId>) -> String {
    let lease = lease.expect("lease id");
    LeaseId::new(lease.term, lease.seq).to_string()
}

/// Executes `action` as buck2 would: a `RequestMetadata` header with the action's
/// mnemonic and the target it belongs to, and `invocation` (a call without one is
/// not recorded).
async fn execute_labelled(
    cell: &Cell,
    job: &Job,
    invocation: &str,
    mnemonic: &str,
    target: &str,
) -> tonic::Streaming<kbf_proto::google::longrunning::Operation> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: invocation.to_owned(),
        action_mnemonic: mnemonic.to_owned(),
        target_id: target.to_owned(),
        ..Default::default()
    };
    let mut request = Request::new(ExecuteRequest {
        instance_name: "main".to_owned(),
        action_digest: Some(job.action.proto.clone()),
        ..Default::default()
    });
    request.metadata_mut().insert_bin(
        REQUEST_METADATA_HEADER,
        MetadataValue::from_bytes(&metadata.encode_to_vec()),
    );
    ExecutionClient::new(cell.channel())
        .execute(request)
        .await
        .expect("Execute")
        .into_inner()
}

const INVOCATION: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

/// Catches: a run's counters that never reach the server's log (no line, or a line
/// without the cgroup's fields), the fields the line had before the key moved or
/// renamed (a reader of the old line breaks), a line not labelled by the mnemonic and
/// target the client sent, a label of one action on another's line, an action without
/// a label given a made-up one rather than `-`, a result that carries no usage given a
/// line of zeros, and a line without its key, outcome, lease or node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_logs_its_usage_keyed_by_shape_and_labelled() {
    log();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;

    // A labelled action.
    let labelled = Job::new("compile foo", &[]);
    cell.upload(&labelled.blobs()).await;
    let mut ops = execute_labelled(&cell, &labelled, INVOCATION, "CxxCompile", "//lib:foo").await;
    let start = daemon.start().await;
    let first_lease = lease_text(start.lease_id);
    let result = with_usage(output(&cell, "foo.o", 0).await, &usage());
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // The same usage on an action no call labelled.
    let plain = Job::new("compile bar", &[]);
    cell.upload(&plain.blobs()).await;
    let mut ops = cell.execute(&plain.action).await;
    let start = daemon.start().await;
    let result = with_usage(output(&cell, "bar.o", 7).await, &usage());
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // A run that reports no usage (a runtime without a measurement).
    let bare = Job::new("compile baz", &[]);
    cell.upload(&bare.blobs()).await;
    let mut ops = execute_labelled(&cell, &bare, INVOCATION, "CxxCompile", "//lib:baz").await;
    let start = daemon.start().await;
    let result = output(&cell, "baz.o", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let foo = job_key("compile foo");
    let [first] = &lines("action usage:", foo)[..] else {
        panic!("one usage line for foo: {:?}", lines("action usage:", foo));
    };
    assert!(
        first.contains(&format!(
            "action usage: exit_code=0 mnemonic=\"CxxCompile\" target=\"//lib:foo\" {FIELDS} \
             key={foo} cfg=- outcome=ok lease={first_lease} node=node-a"
        )),
        "{first}"
    );
    let bar = job_key("compile bar");
    let [second] = &lines("action usage:", bar)[..] else {
        panic!("one usage line for bar");
    };
    assert!(
        second.contains(&format!(
            "action usage: exit_code=7 mnemonic=- target=- {FIELDS} key={bar} cfg=- outcome=ok"
        )),
        "{second}"
    );
    assert_eq!(
        lines("action usage:", job_key("compile baz")),
        Vec::<String>::new()
    );
}

/// Catches: a memory-killed rung's usage dropped (the peak the ladder needs is lost:
/// the killed result is discarded before the line), a killed run given an exit code,
/// a rung that runs again higher up the ladder logged under another key or not at
/// all, more than one line per lease, and a `lease granted` line without the key (the
/// daemon's `lease phases:` line of a cancelled lease joins to the action through it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_memory_rung_and_its_rerun_each_log_a_line() {
    log();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-b", 4, 8).await;
    let (job, key) = buck_job(
        &[
            "interp",
            "buck-out/v2/art/prelude/7a3c1e9f0b2d4c68/rust/tools/__rustc_action__/rustc_action.py",
        ],
        &["buck-out/v2/art/komira/7a3c1e9f0b2d4c68/src/foo/__foo__/libfoo.rlib"],
    );
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;

    let start = daemon.start().await;
    let killed_lease = lease_text(start.lease_id);
    let mut killed = failed_with(start.lease_id, Code::ResourceExhausted, &usage());
    killed.memory_kill = MemoryKill::OwnLimit as i32;
    assert!(daemon.report(killed).await.accepted);

    let rerun = daemon.start().await;
    let rerun_lease = lease_text(rerun.lease_id);
    let result = with_usage(output(&cell, "foo.rlib", 0).await, &usage());
    assert!(daemon.report(ran(rerun.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let usage_lines = lines("action usage:", key);
    let [oom, ok] = &usage_lines[..] else {
        panic!("two usage lines, one per rung: {usage_lines:?}");
    };
    assert!(
        oom.contains(&format!(
            "action usage: exit_code=- mnemonic=- target=- {FIELDS} key={key} \
             cfg=7a3c1e9f0b2d4c68 outcome=oom lease={killed_lease} node=node-b"
        )),
        "{oom}"
    );
    assert!(
        ok.contains(&format!(
            "exit_code=0 mnemonic=- target=- {FIELDS} key={key} cfg=7a3c1e9f0b2d4c68 \
             outcome=ok lease={rerun_lease}"
        )),
        "{ok}"
    );
    let granted = lines("lease granted", key);
    assert_eq!(granted.len(), 2, "{granted:?}");
    assert!(
        granted[0].contains(&format!("lease={killed_lease}")),
        "{granted:?}"
    );
    assert!(
        granted[1].contains(&format!("lease={rerun_lease}")),
        "{granted:?}"
    );
    assert_eq!(lines("action shape:", key).len(), 1, "described once");
}

/// Catches: a timed-out lease's usage dropped (the partial result keeps only its logs,
/// so the usage must be read before it is cut), or its CPU fields lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_lease_logs_its_usage() {
    log();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-c", 4, 8).await;
    let (job, key) = buck_job(
        &[
            "/usr/bin/env",
            "runner",
            "-e",
            "buck-out/v2/art/prelude/7a3c1e9f0b2d4c68/genrule/__genrule__/sh/genrule.sh",
        ],
        &["buck-out/v2/art/komira/src/slow/__slow__/output_artifacts/slow.txt"],
    );
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let lease = lease_text(start.lease_id);
    let result = failed_with(start.lease_id, Code::DeadlineExceeded, &usage());
    assert!(daemon.report(result).await.accepted);
    done(&mut ops).await;

    let [line] = &lines("action usage:", key)[..] else {
        panic!("one usage line: {:?}", lines("action usage:", key));
    };
    assert!(
        line.contains(&format!(
            "action usage: exit_code=- mnemonic=- target=- {FIELDS} key={key} cfg=- \
             outcome=timeout lease={lease} node=node-c"
        )),
        "{line}"
    );
}

/// Catches: a lease the farm failed (UNAVAILABLE, no memory kill) dropping its
/// usage, or its reruns logging fewer lines than leases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_infrastructure_failure_logs_each_lease() {
    log();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-d", 4, 8).await;
    let (job, key) = buck_job(&["tools/kbf-m2-act", "boom"], &["out/boom"]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    report_every_run(&mut daemon, start, |lease| {
        failed_with(lease, Code::Unavailable, &usage())
    })
    .await;
    done(&mut ops).await;

    let usage_lines = lines("action usage:", key);
    assert_eq!(
        usage_lines.len(),
        1 + kbf_sched::FARM_RERUNS as usize,
        "{usage_lines:?}"
    );
    for line in &usage_lines {
        assert!(line.contains(" outcome=infra lease="), "{line}");
        assert!(line.contains("exit_code=- "), "{line}");
    }
}

/// Catches: a label taken only from a recorded call (buck2's calls are not recorded
/// with the recorder off or without an invocation id; Bazel's label would be lost),
/// and two callers of one action (a twin) logging two lines for its one lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrecorded_call_is_labelled_and_a_twin_logs_once() {
    log();
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-e", 4, 8).await;
    let (job, key) = buck_job(&["cc", "-c", "a.c"], &["a.o"]);
    cell.upload(&job.blobs()).await;
    let mut first = execute_labelled(&cell, &job, "", "CppCompile", "//pkg:a").await;
    let mut twin = cell.execute(&job.action).await;
    let start = daemon.start().await;
    let result = with_usage(output(&cell, "a.o", 0).await, &usage());
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut first).await;
    done(&mut twin).await;

    let [line] = &lines("action usage:", key)[..] else {
        panic!(
            "one usage line for the twin: {:?}",
            lines("action usage:", key)
        );
    };
    assert!(
        line.contains(&format!(
            "action usage: exit_code=0 mnemonic=\"CppCompile\" target=\"//pkg:a\" {FIELDS} key={key}"
        )),
        "{line}"
    );
}

/// Catches: a line for a report the scheduler did not take (its lease was given up
/// while the server checked the result, and another lease ran the action): the line
/// would say `outcome=ok` for a run whose result nobody got, and count the action
/// twice. Its usage stays in the daemon's `lease phases:` line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_report_the_scheduler_does_not_take_has_no_line() {
    log();
    let cell = Cell::start().await;
    let mut first = cell.daemon("node-f", 4, 8).await;
    let (job, key) = buck_job(&["cc", "-c", "raced.c"], &["raced.o"]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lost = first.start().await;
    let raced = with_usage(output(&cell, "from the lease given up", 0).await, &usage());

    cell.cache.meta().query.close_next();
    first.send(daemon_message::Message::Result(ran(lost.lease_id, &raced)));
    cell.cache.meta().query.held().await;
    let mut again = cell.daemon("node-f", 4, 8).await;
    assert!(again.heartbeat(&[]).await);
    let current = again.start().await;
    cell.cache.meta().query.open();
    let ack = first
        .expect("ResultAck", |m| match m {
            server_message::Message::ResultAck(a) => Some(*a),
            _ => None,
        })
        .await;
    assert!(!ack.accepted, "the given-up lease's result was taken");

    let result = with_usage(output(&cell, "from the current lease", 0).await, &usage());
    assert!(again.report(ran(current.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let current = lease_text(current.lease_id);
    let [line] = &lines("action usage:", key)[..] else {
        panic!(
            "one line, the current lease's: {:?}",
            lines("action usage:", key)
        );
    };
    assert!(
        line.contains(&format!("outcome=ok lease={current} ")),
        "{line}"
    );
}
