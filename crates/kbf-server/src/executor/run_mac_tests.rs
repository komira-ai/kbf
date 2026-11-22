//! The pack of a Mac job: the Mac's agent commits it to the tier store only after the job ended
//! and its processes are swept, so the frontend reads it again while the store has none, for a
//! Mac job and only for one.

use std::time::Duration;

use js_proto::v1::{ExecMode, Shape};
use kbf_front::{Dispatch as _, Finished};
use kbf_job_client::fake::Machine;
use tonic::Code;

use super::*;
use crate::executor::tests::{Caller, IMAGE, Rig};
use crate::executor::{ActionFacts, MacPool, RejectReason, System};

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// A rig whose executor has Macs and whose job service has one, `mac-1`, as a Mac's agent reports
/// itself (`os=macos`, `arch=arm64`, the native runtime) with no Xcode; its actions ask for a Mac
/// when `mac`. The Mac job, which names no `exec` and no Xcode, is placed on it.
async fn rig(mac: bool) -> Rig {
    let rig = Rig::with(|cfg| {
        cfg.mac = Some(MacPool {
            bundle: BUNDLE.to_owned(),
            os_version: crate::executor::DEFAULT_MAC_OS_VERSION,
            roots: None,
        });
    })
    .await;
    rig.fake.add_host(
        "mac-1",
        Machine {
            shape: Shape {
                cpu_millis: 16_000,
                memory_bytes: 64 << 30,
                gpus: 0,
                gpu_class: String::new(),
                disk_bytes: Some(1 << 40),
            },
            os: "macos".to_owned(),
            arch: "arm64".to_owned(),
            exec: ExecMode::SandboxExec,
            xcodes: Vec::new(),
        },
    );
    if mac {
        let mut facts = rig.inputs.facts.lock().expect("facts");
        let shape = crate::action_usage::Shape::of(&["cc".to_owned()], &["o".to_owned()]);
        *facts = Ok(ActionFacts {
            image: IMAGE.to_owned(),
            timeout: Some(Duration::from_secs(60)),
            system: System::Macos { os_version: None },
            size_class: crate::executor::size_class::SizeClass::of_action(&shape),
        });
    }
    rig
}

/// The ingest answers `n` reads with no object under the key, then a clean run.
fn absent_for(rig: &Rig, n: usize) {
    let mut script = rig.ingest.script.lock().expect("script");
    script.extend((0..n).map(|_| Ingested::Reject(RejectReason::Missing)));
}

/// The keys of the packs read, in order.
fn keys_read(rig: &Rig) -> Vec<String> {
    let read = rig.ingest.ingested.lock().expect("reads");
    read.iter().map(|(_, key)| key.clone()).collect()
}

/// The host `host` runs attempt `attempt` of `caller`'s operation to an exit of 0.
async fn ends_well(rig: &Rig, caller: &Caller, attempt: u32, host: &str) {
    let job = rig.submitted(caller, attempt).await;
    assert!(rig.fake.place(&job, host), "{job} is placed on {host}");
    assert!(rig.fake.starting(&job) && rig.fake.run(&job), "{job} runs");
    assert!(rig.fake.finish(&job, 0), "{job} finishes");
}

/// Asserts that a Mac job that ended well, whose pack the store does not have yet, has the same
/// key read again every poll until the store has it, and that the caller is answered from that
/// read with no second job. Catches a Mac run failed as the farm's (and the action run again)
/// because its pack was read before its Mac's tier store stored it.
#[tokio::test(start_paused = true)]
async fn a_mac_jobs_pack_is_read_again_until_the_store_has_it() {
    let rig = rig(true).await;
    absent_for(&rig, 2);
    let caller = rig.submit(1);
    ends_well(&rig, &caller, 0, "mac-1").await;
    let finished = rig.until_done(&caller).await;
    assert!(
        matches!(&finished, Finished::Ran(result) if result.exit_code == 0),
        "{finished:?}"
    );
    let keys = keys_read(&rig);
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert!(keys.iter().all(|key| *key == keys[0]), "{keys:?}");
    assert_eq!(rig.fake.jobs().len(), 1);
}

/// Asserts that a Mac job whose pack is still absent once [`PACK_WAIT`] has passed is the farm's
/// failure: the action runs again as the next attempt, under a new key, and its caller is
/// answered from that attempt. Catches a wait that never ends, holding the operation for a pack
/// its Mac never stores.
#[tokio::test(start_paused = true)]
async fn a_mac_jobs_pack_still_absent_after_the_wait_runs_the_action_again() {
    let rig = rig(true).await;
    absent_for(&rig, 2);
    let caller = rig.submit(1);
    ends_well(&rig, &caller, 0, "mac-1").await;
    rig.until("the first read", || keys_read(&rig).len() == 1)
        .await;
    rig.advance(PACK_WAIT).await;
    rig.until("the read after the wait", || keys_read(&rig).len() == 2)
        .await;
    ends_well(&rig, &caller, 1, "mac-1").await;
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
    let keys = keys_read(&rig);
    assert_eq!(keys.len(), 3, "{keys:?}");
    assert_eq!(keys[0], keys[1]);
    assert_ne!(
        keys[2], keys[0],
        "the next attempt has its own put directory"
    );
    assert_eq!(rig.fake.jobs().len(), 2);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// Asserts that a Linux job whose pack is absent is not waited on: the key is read once and the
/// action runs again as the next attempt. Catches the Mac wait applied to a Linux job, whose
/// store holds the pack before the job can end.
#[tokio::test(start_paused = true)]
async fn a_linux_jobs_absent_pack_is_read_once() {
    let rig = rig(false).await;
    absent_for(&rig, 1);
    let caller = rig.submit(1);
    ends_well(&rig, &caller, 0, "h1").await;
    ends_well(&rig, &caller, 1, "h1").await;
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
    let keys = keys_read(&rig);
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert_ne!(keys[0], keys[1]);
}

/// Asserts that a cancel while the frontend waits for a Mac job's pack answers the caller
/// CANCELLED and stops the reads, and sends the job service no cancel: the job has ended.
/// Catches a caller that cancels and is held until the wait ends.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_a_mac_pack_is_awaited_answers_cancelled() {
    let rig = rig(true).await;
    absent_for(&rig, 1_000);
    let caller = rig.submit(1);
    ends_well(&rig, &caller, 0, "mac-1").await;
    rig.until("the first read", || !keys_read(&rig).is_empty())
        .await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    let Finished::Failed(status) = finished else {
        panic!("not cancelled: {finished:?}");
    };
    assert_eq!(status.code, Code::Cancelled as i32);
    let reads = keys_read(&rig).len();
    rig.advance(rig.cfg.poll * 4).await;
    assert_eq!(keys_read(&rig).len(), reads, "no read after the cancel");
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert_eq!(rig.fake.jobs().len(), 1);
}
