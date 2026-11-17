//! What a submitted job holds: its network, labels, constraints, needs and the task tool's
//! flags, and what the executor refuses to submit at all.

use js_proto::v1::{ExecMode, NetworkLevel, Shape};
use kbf_front::Finished;
use kbf_job_client::fake::{Fault, Machine};
use kbf_server::executor::{MintError, TASK_IN_TREE};
use tonic::Code;

use crate::rig::{Posted, Rig, machine, submits};

const HOST: &str = "node1";

fn code(finished: &Finished) -> Code {
    match finished {
        Finished::Failed(status) => Code::from_i32(status.code),
        other => panic!("not a failure: {other:?}"),
    }
}

fn message(finished: &Finished) -> String {
    match finished {
        Finished::Failed(status) => status.message.clone(),
        other => panic!("not a failure: {other:?}"),
    }
}

async fn submitted(rig: &mut Rig) -> Posted {
    rig.pump().await;
    let _caller = rig.submit(3);
    rig.pump().await;
    rig.jobs_of(3).remove(0)
}

/// A host of the default machine that reports `disk` bytes of scratch disk (`None`: reports no
/// disk at all).
fn host_with_disk(disk: Option<u64>) -> Machine {
    let base = machine(64, "x86_64");
    Machine {
        shape: Shape {
            disk_bytes: disk,
            ..base.shape.clone()
        },
        ..base
    }
}

/// Catches a submitted job that gives the action more than it should or leaks what it must
/// not: no image, no port, no environment, no file, the task tool in the tree as the job's
/// command, no network, the labels, which every list of jobs shows, hold the frontend, the
/// process and the size class and nothing else, no argument carries a url, a grant or a
/// secret, and the flags of the retired presigned task (`--pack-url`, `--grant-token`,
/// `--insecure-argv-secrets`, `--allow-plain-http-put`, `--allow-plain-http-grant`,
/// `--locate`, `--work-dir`) and the retired `--root-der` are absent.
#[tokio::test(start_paused = true)]
async fn a_job_holds_no_image_port_file_network_or_secret() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let job = submitted(&mut rig).await;
    let spec = &job.spec;
    assert!(spec.image.is_empty());
    assert!(spec.ports.is_empty() && spec.host_ports.is_empty());
    assert!(spec.env.is_empty());
    assert_eq!(spec.command, TASK_IN_TREE);
    assert!(spec.files.is_empty());
    assert_eq!(spec.network(), NetworkLevel::None);
    let labels = &spec.labels;
    assert_eq!(labels.len(), 3, "{labels:?}");
    assert_eq!(
        labels.get("size-class").map(String::as_str),
        Some("root.pkg.t.build")
    );
    assert_eq!(
        labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );
    assert_eq!(
        labels.get("kbf-instance").map(String::as_str),
        Some(rig.cfg.instance.as_str())
    );
    for arg in &spec.args {
        assert!(!arg.contains("://"), "a url is in {arg:?}");
    }
    for flag in [
        "--pack-url",
        "--grant-token",
        "--grant-url",
        "--insecure-argv-secrets",
        "--allow-plain-http-put",
        "--allow-plain-http-grant",
        "--locate",
        "--root-der",
        "--work-dir",
    ] {
        assert!(!spec.args.iter().any(|a| a == flag), "{flag} was passed");
    }
}

/// Catches a job the preparation refuses (an input not held, a tree over budget) being rerun
/// as if the farm were at fault: asking again gives the same answer, so it is the caller's to
/// fix and ends as an invalid argument, not as a retry, and nothing is submitted.
#[tokio::test(start_paused = true)]
async fn a_job_the_preparation_refuses_is_the_callers_not_rerun() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    *rig.ingest.mint_fails.lock().expect("mint") =
        Some(MintError::Refused("an input is not held".to_owned()));
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
    assert!(
        format!("{finished:?}").contains("an input is not held"),
        "{finished:?}"
    );
    assert!(rig.jobs().is_empty(), "submitted without a directory");
    assert!(submits(&rig.fake.calls()).is_empty());
}

/// Catches a directory the mint would not issue being submitted anyway: the job needs a put
/// directory to commit into, and without one it is infrastructure, so nothing is submitted and
/// the farm runs the action again, which the mint then answers.
#[tokio::test(start_paused = true)]
async fn a_put_directory_that_cannot_be_issued_is_rerun_not_submitted() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    *rig.ingest.mint_fails.lock().expect("mint") =
        Some(MintError::Unavailable("no secret".to_owned()));
    let caller = rig.submit(3);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.exec.step().await;
    assert!(rig.jobs().is_empty(), "submitted without a directory");
    rig.elapse(15).await;
    let job = rig.jobs_of(3).remove(0);
    rig.host_finishes(job.id(), HOST, 0);
    let finished = rig.until_done(&caller, 6).await;
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
}

/// Catches a job that asks for any host: its constraints name Linux, the architecture the
/// config reports and containers, so the job service never places it on a host of another
/// kind.
#[tokio::test(start_paused = true)]
async fn a_job_asks_for_linux_the_configured_architecture_and_containers() {
    for arch in ["x86_64", "arm64"] {
        let mut rig = Rig::start(&[], 1, move |cfg| cfg.arch = arch.to_owned()).await;
        rig.fake.add_host(HOST, machine(64, arch));
        let job = submitted(&mut rig).await;
        let wants = job.spec.constraints.as_ref().expect("constraints");
        assert_eq!(wants.os, "linux");
        assert_eq!(wants.arch, arch);
        assert_eq!(wants.exec(), ExecMode::Container);
    }
}

/// Catches a job of this executor reaching a host of another kind, or a refusal of that being
/// rerun for ever: a job service whose only host is of another architecture or system refuses
/// the submit `no-capable-host`, which nothing could ever satisfy, so the action ends
/// INVALID_ARGUMENT after one submit and no rerun.
#[tokio::test(start_paused = true)]
async fn only_hosts_of_the_reported_system_and_architecture_take_the_job() {
    let other_system = Machine {
        os: "macos".to_owned(),
        ..machine(64, "x86_64")
    };
    for other in [machine(64, "aarch64"), other_system] {
        let mut rig = Rig::start(&[], 1, |_| {}).await;
        rig.fake.add_host("other", other);
        rig.pump().await;
        let caller = rig.submit(3);
        let finished = rig.until_done(&caller, 3).await;
        assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
        assert!(
            message(&finished).contains("no-capable-host"),
            "{finished:?}"
        );
        assert!(rig.jobs().is_empty());
        assert_eq!(submits(&rig.fake.calls()).len(), 1, "the action was rerun");
    }
}

/// Catches a job that asks the job service for disk: a job does not carry `disk_bytes` (unset,
/// not zero, so the service has no disk to place it by), whatever the host reports. It names no
/// CPU and no memory either (the service sizes it from its class), and is not the whole host.
#[tokio::test(start_paused = true)]
async fn no_job_books_disk() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let shared = submitted(&mut rig).await;
    let needs = shared.spec.needs.as_ref().expect("needs");
    assert_eq!(needs.disk_bytes, None, "{needs:?}");
    assert_eq!((needs.cpu_millis, needs.memory_bytes), (0, 0), "{needs:?}");
    assert!(!needs.whole_host, "{needs:?}");
}

/// Catches a job placed only on a host that reports scratch disk: a host that reports none, and
/// one that reports zero bytes, each take the job: the job service makes it (it is never refused
/// `no-capable-host`, which would end the action), after one submit and no rerun, and the
/// worker stays.
#[tokio::test(start_paused = true)]
async fn a_job_is_placed_on_a_host_that_reports_no_disk() {
    for disk in [None, Some(0)] {
        let mut rig = Rig::start(&[], 1, |_| {}).await;
        rig.fake.add_host(HOST, host_with_disk(disk));
        let job = submitted(&mut rig).await;
        assert_eq!(job.spec.needs.as_ref().and_then(|n| n.disk_bytes), None);
        assert_eq!(rig.jobs().len(), 1, "{disk:?}: the job was not made");
        assert_eq!(
            submits(&rig.fake.calls()).len(),
            1,
            "{disk:?}: the action was rerun"
        );
        let workers = rig.exec.workers();
        assert_eq!(workers.len(), 1, "{disk:?}: the worker was dropped");
        assert!(!workers[0].silent && !workers[0].draining, "{disk:?}");
    }
}

/// Catches the refusal rule swallowing every failure of a submit: a status that is not about
/// the job (the token is refused, or may not make the call) is still the farm's to run again,
/// and ends as infrastructure once its budget is spent, after three submits.
#[tokio::test(start_paused = true)]
async fn a_refusal_that_is_not_about_the_job_is_still_infrastructure() {
    for refused in [Code::Unauthenticated, Code::PermissionDenied] {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
        rig.pump().await;
        for _ in 0..4 {
            rig.fake.inject_on(
                "SubmitJob",
                Fault::Status {
                    code: refused,
                    message: "not for you".to_owned(),
                },
            );
        }
        let caller = rig.submit(3);
        let finished = rig.until_done(&caller, 3).await;
        assert_eq!(code(&finished), Code::Internal, "{refused:?}: {finished:?}");
        assert!(
            message(&finished).contains("infra failure"),
            "{}",
            message(&finished)
        );
        assert!(rig.jobs().is_empty());
        assert_eq!(
            submits(&rig.fake.calls()).len(),
            3,
            "{refused:?}: the action was tried once and rerun twice"
        );
    }
}
