//! Platform routing end to end: daemons report their OS and architecture in `Hello`,
//! actions name theirs in REAPI platform properties, and the server gives each action
//! only to a daemon that satisfies it. An action no connected daemon can run waits with
//! a reason in its operation metadata, and is refused FAILED_PRECONDITION after the
//! unservable wait.

mod support;

use std::time::{Duration, Instant};

use kbf_front::{ERROR_DOMAIN, NO_WORKER_REASON};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::reapi::{ExecuteOperationMetadata, execution_stage::Value as ExecStage};
use kbf_proto::worker::daemon_message;
use prost::Message;
use support::{Cell, FakeDaemon, Job, PROMPT, done, hello, hello_on, output, ran, response, stage};
use tokio::time::timeout;
use tonic::{Code, Streaming};

const ARM: &[(&str, &str)] = &[("arch", "arm64"), ("os", "linux")];

/// The reason in a queued operation's metadata, if it carries one.
fn waiting_reason(op: &Operation) -> Option<String> {
    let meta = op.metadata.as_ref().expect("metadata");
    let meta = ExecuteOperationMetadata::decode(meta.value.as_slice()).expect("metadata decodes");
    let partial = meta.partial_execution_metadata?;
    let [info] = partial.auxiliary_metadata.as_slice() else {
        panic!("one auxiliary entry expected: {partial:?}");
    };
    let info = ErrorInfo::decode(info.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(
        (info.reason.as_str(), info.domain.as_str()),
        (NO_WORKER_REASON, ERROR_DOMAIN)
    );
    assert_eq!(meta.stage, ExecStage::Queued as i32);
    info.metadata.get("why").cloned()
}

/// The next operation update.
async fn next(ops: &mut Streaming<Operation>) -> Operation {
    timeout(PROMPT, ops.message())
        .await
        .expect("an update in time")
        .expect("a healthy stream")
        .expect("an update")
}

/// Catches: placement that ignores the platform. The arm64 node registers first, sorts first
/// by name and holds nothing, so placement without matching would give it the x86-64
/// action, and the x86-64 daemon the arm64 action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_action_runs_on_a_daemon_of_its_platform() {
    let cell = Cell::start().await;
    let mut arm = cell
        .daemon_hello(hello_on("node-a", 4, 8, ARM))
        .await
        .expect("registered");
    let mut linux = cell.daemon("node-b", 4, 8).await;

    let linux_job = Job::new("x86", &[("OSFamily", "linux"), ("ISA", "x86-64")]);
    let arm_job = Job::new("arm", &[("OSFamily", "linux"), ("ISA", "arm-a64")]);
    cell.upload(&linux_job.blobs()).await;
    cell.upload(&arm_job.blobs()).await;

    let mut linux_ops = cell.execute(&linux_job.action).await;
    let start = linux.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&linux_job.action.proto));
    arm.no_work().await;
    let built = output(&cell, "on x86", 0).await;
    assert!(linux.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut linux_ops).await;

    let mut arm_ops = cell.execute(&arm_job.action).await;
    let start = arm.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&arm_job.action.proto));
    linux.no_work().await;
    let built = output(&cell, "on arm", 0).await;
    assert!(arm.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut arm_ops).await;
}

/// The unservable wait the refusal test runs with.
const WAIT: Duration = Duration::from_secs(1);

/// How much later than the wait a refusal may reach its caller: the server's ticks
/// (50 ms in tests), committing the refusal and the stream, with room for a loaded
/// machine. Less than the wait, so a server that waits twice as long is caught.
const LATE: Duration = Duration::from_millis(900);

/// Catches: an action no connected daemon can run handed to one that cannot, left
/// queued forever without a word, or ended without the reason (or with a code a client
/// would retry, or with MISSING details that would make it re-upload and retry). Also:
/// the server's unservable wait not the one it was given (the default, or a multiple of
/// it), seen as a refusal before the wait (the lower bound) or as a refusal that names
/// another wait (the text: the refusal names the wait the scheduler held it for, so a
/// doubled wait is caught however loaded the machine). Also: a refusal decided on time
/// but delivered late (a slow tick, a stalled stream), seen as a refusal well after the
/// wait (the upper bound, `LATE`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_action_no_daemon_can_run_waits_with_a_reason_then_is_refused() {
    let cell = Cell::start_with_unservable_wait(WAIT).await;
    let mut linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("mac only", &[("OSFamily", "macos")]);
    cell.upload(&job.blobs()).await;

    let asked = Instant::now();
    let mut ops = cell.execute(&job.action).await;
    let first = next(&mut ops).await;
    let why = waiting_reason(&first).expect("a reason from the start");
    assert!(why.contains("node-b") && why.contains("os=macos"), "{why}");
    linux.no_work().await;

    let last = done(&mut ops).await;
    let took = asked.elapsed();
    assert!(
        took >= WAIT,
        "refused after {took:?}, before the {WAIT:?} wait"
    );
    assert!(
        took < WAIT + LATE,
        "refused after {took:?}, long after the {WAIT:?} wait"
    );
    let status = response(&last).status.expect("a status");
    assert_eq!(status.code, Code::FailedPrecondition as i32, "{status:?}");
    assert!(status.message.starts_with(&why), "{}", status.message);
    let waited = format!("(waited {} s ", WAIT.as_secs());
    assert!(status.message.contains(&waited), "{}", status.message);
    assert!(status.details.is_empty());
    linux.no_work().await;
}

/// Catches: a waiting action not placed when a daemon that can run it connects, its
/// callers still told it cannot run, and a caller that joins it while it waits never
/// told why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_can_run_it_ends_the_wait() {
    let cell = Cell::start().await;
    let _linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("for arm", &[("OSFamily", "linux"), ("ISA", "arm-a64")]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    assert!(waiting_reason(&next(&mut ops).await).is_some());
    let mut joined = cell.execute(&job.action).await;
    assert!(
        waiting_reason(&next(&mut joined).await).is_some(),
        "a joiner not told why"
    );

    let mut arm = cell
        .daemon_hello(hello_on("node-m", 4, 8, ARM))
        .await
        .expect("registered");
    let start = arm.start().await;
    for stream in [&mut ops, &mut joined] {
        let mut op = next(stream).await;
        while stage(&op) == ExecStage::Queued as i32 {
            assert_eq!(waiting_reason(&op), None, "a reason after the wait ended");
            op = next(stream).await;
        }
        assert_eq!(stage(&op), ExecStage::Executing as i32);
    }
    let built = output(&cell, "on arm", 0).await;
    assert!(arm.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
    done(&mut joined).await;
}

/// Catches: a node report without an architecture accepted (it could never be matched
/// safely), a resent report whose platform entries are dropped (an `os_image` added later is
/// never matched), and a malformed resent report taken anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hello_carries_what_placement_matches() {
    let cell = Cell::start().await;
    let mut no_arch = hello("node-x", 4, 8);
    no_arch.capabilities.retain(|c| c.key != "arch");
    let refused = cell.daemon_hello(no_arch).await.err().expect("refused");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(refused.message().contains("arch"), "{}", refused.message());

    let mut arm = cell
        .daemon_hello(hello_on("node-m", 4, 8, ARM))
        .await
        .expect("registered");
    let job = Job::new("pool", &[("os_image", "image-a")]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    arm.no_work().await;

    // A garbled resend is ignored; the node keeps its report.
    let garbled = hello_on("node-m", 4, 8, &[("arch", "sparc"), ("os", "linux")]);
    arm.send(daemon_message::Message::Hello(garbled));
    arm.no_work().await;

    let imaged = hello_on(
        "node-m",
        4,
        8,
        &[("arch", "arm64"), ("os", "linux"), ("os_image", "image-a")],
    );
    arm.send(daemon_message::Message::Hello(imaged));
    let start = arm.start().await;
    let built = output(&cell, "pooled", 0).await;
    assert!(arm.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
}

/// Catches: a whole-machine lease offered to a daemon whose drivers serve only actions
/// (that daemon refuses its `Start`), and one left waiting without saying why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_machine_lease_waits_while_no_daemon_serves_it() {
    let cell = Cell::start().await;
    let mut linux = cell.daemon("node-b", 4, 8).await;
    let job = Job::new("whole", &[("kbf-lease", "whole_machine")]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let first = next(&mut ops).await;
    let why = waiting_reason(&first).expect("a reason from the start");
    assert!(
        why.contains("serves lease kind whole_machine") && why.contains("native-whole-machine"),
        "{why}"
    );
    linux.no_work().await;
}

/// Runs `job` on `daemon` and reports it done, asserting `others` are offered nothing.
async fn run_on(cell: &Cell, job: &Job, daemon: &mut FakeDaemon, others: &mut [&mut FakeDaemon]) {
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;
    assert_eq!(start.action_digest.as_ref(), Some(&job.action.proto));
    for other in others {
        other.no_work().await;
    }
    let built = output(cell, "built", 0).await;
    assert!(daemon.report(ran(start.lease_id, &built)).await.accepted);
    done(&mut ops).await;
}

/// Catches (the live incident: an action whose platform was only `pool=mojo-bench` was
/// leased to a node of another platform): an action that names no OS and no architecture
/// given to the arm64 node because it registered first and sorts first. The arm64 node is
/// `arm-a`, ahead of the x86-64 node by name, and the platform has no OS key (one property
/// kbf does not read, and none at all), so only the default can keep it off the arm64 node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_action_naming_no_os_is_not_leased_to_an_arm_node() {
    let cell = Cell::start_with_default_platform(kbf_front::DefaultPlatform::LINUX_X86_64).await;
    let mut arm = cell
        .daemon_hello(hello_on("arm-a", 4, 8, ARM))
        .await
        .expect("registered");
    let mut linux = cell.daemon("node-b", 4, 8).await;

    let pool = Job::new("pool only", &[("pool", "mojo-bench")]);
    run_on(&cell, &pool, &mut linux, &mut [&mut arm]).await;
    let empty = Job::new("empty platform", &[]);
    run_on(&cell, &empty, &mut linux, &mut [&mut arm]).await;
}

/// Catches: the default taking an action that names an architecture to the default one, so
/// that `ISA=arm-a64` alone is not built on the arm64 node it asked for. It names half of a
/// platform and is never defaulted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn half_a_platform_still_goes_to_the_node_it_names() {
    let cell = Cell::start_with_default_platform(kbf_front::DefaultPlatform::LINUX_X86_64).await;
    let mut linux = cell.daemon("node-a", 4, 8).await;
    let mut arm = cell
        .daemon_hello(hello_on("node-b", 4, 8, ARM))
        .await
        .expect("registered");

    let isa = Job::new("arm alone", &[("ISA", "arm-a64")]);
    run_on(&cell, &isa, &mut arm, &mut [&mut linux]).await;
}

/// Catches: `--default-platform none` ignored (or the flag's value ignored for the
/// built-in): with it off an action that names no OS is given to the first node by
/// name, the arm64 node, as before the flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_platform_none_leaves_placement_to_the_first_fit() {
    let cell = Cell::start_with_default_platform(kbf_front::DefaultPlatform::NONE).await;
    let mut arm = cell
        .daemon_hello(hello_on("arm-a", 4, 8, ARM))
        .await
        .expect("registered");
    let mut linux = cell.daemon("node-b", 4, 8).await;

    let pool = Job::new("pool only", &[("pool", "mojo-bench")]);
    run_on(&cell, &pool, &mut arm, &mut [&mut linux]).await;
}
