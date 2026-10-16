//! The server's `action usage:` line (`farm/record.rs`), in a test binary of its own:
//! the log is read through a global subscriber, which no other test's events can reach
//! or race. A buck2-shaped Execute names its mnemonic and target in `RequestMetadata`;
//! the daemon reports a result whose `ResourceUsage` carries the lease cgroup's
//! counters; the line carries both.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use kbf_front::REQUEST_METADATA_HEADER;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    ActionResult, ExecuteRequest, ExecutedActionMetadata, RequestMetadata, ToolDetails,
};
use kbf_proto::worker::{CgroupUsage, NumaNodeBytes, ResourceUsage};
use prost::Message;
use support::{Cell, Job, done, output, ran};
use tonic::Request;
use tonic::metadata::MetadataValue;

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

/// `result` carrying `usage` the way a daemon sends it.
fn with_usage(mut result: ActionResult, usage: &ResourceUsage) -> ActionResult {
    result.execution_metadata = Some(ExecutedActionMetadata {
        auxiliary_metadata: vec![usage.to_any()],
        ..ExecutedActionMetadata::default()
    });
    result
}

/// Executes `action` as buck2 would: a `RequestMetadata` header with an invocation id,
/// the action's mnemonic and the target it belongs to.
async fn execute_labelled(
    cell: &Cell,
    job: &Job,
    mnemonic: &str,
    target: &str,
) -> tonic::Streaming<kbf_proto::google::longrunning::Operation> {
    let metadata = RequestMetadata {
        tool_details: Some(ToolDetails {
            tool_name: "buck2".to_owned(),
            tool_version: "0.1.0".to_owned(),
        }),
        tool_invocation_id: "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4".to_owned(),
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

/// Catches: a run's counters that never reach the server's log (no line, or a line
/// without the cgroup's fields), a line not keyed by the mnemonic and target the
/// client sent (the per-category aggregation the line is for), a label of one action
/// on another's line, an action without a label given a made-up one rather than `-`,
/// and a result that carries no usage given a line of zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_logs_its_usage_keyed_by_mnemonic_and_target() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-a", 4, 8).await;
    let usage = ResourceUsage {
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
    };

    // A labelled action.
    let labelled = Job::new("compile foo", &[]);
    cell.upload(&labelled.blobs()).await;
    let mut ops = execute_labelled(&cell, &labelled, "CxxCompile", "//lib:foo").await;
    let start = daemon.start().await;
    let result = with_usage(output(&cell, "foo.o", 0).await, &usage);
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // The same usage on an action no recorded call named: no label to key it by.
    let plain = Job::new("compile bar", &[]);
    cell.upload(&plain.blobs()).await;
    let mut ops = cell.execute(&plain.action).await;
    let start = daemon.start().await;
    let result = with_usage(output(&cell, "bar.o", 7).await, &usage);
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    // A run that reports no usage (a runtime without a measurement).
    let bare = Job::new("compile baz", &[]);
    cell.upload(&bare.blobs()).await;
    let mut ops = execute_labelled(&cell, &bare, "CxxCompile", "//lib:baz").await;
    let start = daemon.start().await;
    let result = output(&cell, "baz.o", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;

    let text = String::from_utf8(std::mem::take(&mut *log.0.lock().expect("log"))).expect("UTF-8");
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("action usage:"))
        .collect();
    let [first, second] = lines.as_slice() else {
        panic!("two usage lines expected (the third run reported none): {text}");
    };
    let fields = "wall_us=2000000 cpu_usage_us=3000000 cpu_user_us=2000000 cpu_system_us=1000000 \
                  cpu_periods=0 cpu_throttled=0 throttled_us=0 mem_peak=4096 parallelism=1.50 \
                  numa_bytes=0:600,1:400 numa_spread=0.40";
    assert!(
        first.contains(&format!(
            "action usage: exit_code=0 mnemonic=\"CxxCompile\" target=\"//lib:foo\" {fields}"
        )),
        "{first}"
    );
    assert!(
        second.contains(&format!(
            "action usage: exit_code=7 mnemonic=- target=- {fields}"
        )),
        "{second}"
    );
}
