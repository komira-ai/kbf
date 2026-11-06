//! What a submitted job holds: its network, labels, constraints, needs and the task tool's
//! flags, and what the executor refuses to submit at all.

use js_proto::v1::{ExecMode, NetworkLevel, Shape};
use kbf_front::Finished;
use kbf_job_client::fake::{Fault, Machine};
use kbf_server::executor::{DISK_MARGIN, Delivery, MintError};
use tonic::Code;

use crate::real_prep::Log;
use crate::rig::{GIB, INPUT_BYTES, Posted, Rig, machine, submits};

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

/// A host of the default machine whose scratch disk is `disk` bytes.
fn host_with_disk(disk: u64) -> Machine {
    let base = machine(64, "x86_64");
    Machine {
        shape: Shape {
            disk_bytes: Some(disk),
            ..base.shape.clone()
        },
        ..base
    }
}

/// Catches a submitted job that gives the action more than it should or leaks what it must
/// not: no ports, no environment, no command of its own (the task tool is the kbf image's
/// entrypoint, not a host file the action could replace), a network of the host's allow-list
/// only, no files under argument delivery, the labels, which every list of jobs shows and
/// which are not secret-safe, hold the frontend and the process and nothing else, neither
/// secret appears in any argument but the flag that carries it, and the optional flags (a
/// plain-http `PUT`, a working directory), and the retired `--root-der`, are absent unless the
/// config asks.
#[tokio::test(start_paused = true)]
async fn a_job_mounts_nothing_and_holds_no_secret_outside_its_flags() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let job = submitted(&mut rig).await;
    let spec = &job.spec;
    assert!(spec.ports.is_empty());
    assert!(spec.env.is_empty());
    assert!(spec.command.is_empty());
    assert!(spec.files.is_empty());
    assert_eq!(spec.network(), NetworkLevel::AllowList);
    let labels = &spec.labels;
    assert_eq!(labels.len(), 2, "{labels:?}");
    assert_eq!(
        labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );
    assert_eq!(
        labels.get("kbf-instance").map(String::as_str),
        Some(rig.cfg.instance.as_str())
    );
    let (url, token) = (
        job.arg("--pack-url").expect("a url").to_owned(),
        job.arg("--grant-token").expect("a token").to_owned(),
    );
    for (at, arg) in spec.args.iter().enumerate() {
        let carries_url =
            spec.args.get(at.wrapping_sub(1)).map(String::as_str) == Some("--pack-url");
        let carries_token =
            spec.args.get(at.wrapping_sub(1)).map(String::as_str) == Some("--grant-token");
        assert!(
            carries_url || !arg.contains(&url),
            "the pack url is in {arg:?}"
        );
        assert!(
            carries_token || !arg.contains(&token),
            "the read token is in {arg:?}"
        );
    }
    assert!(
        labels
            .values()
            .all(|v| !v.contains(&url) && !v.contains(&token))
    );
    for flag in [
        "--allow-plain-http-put",
        "--allow-plain-http-grant",
        "--locate",
        "--root-der",
        "--work-dir",
    ] {
        assert!(!spec.args.iter().any(|a| a == flag), "{flag} was passed");
    }
}

/// Catches secrets leaking into the arguments when the operator chose files: under file
/// delivery the pack url and the read token are each alone in a file of the job, the
/// arguments name the files and hold neither secret, and the opt-in for secrets as arguments
/// is not passed to the task.
#[tokio::test(start_paused = true)]
async fn under_file_delivery_the_secrets_are_in_files_and_in_no_argument() {
    let mut rig = Rig::start(&[HOST], 1, |cfg| {
        cfg.delivery = Delivery::Files;
        cfg.insecure_argv_secrets = false;
    })
    .await;
    let job = submitted(&mut rig).await;
    let url = format!("https://store.test/{}?sig=s3cr3t", rig.presign.keys()[0]);
    let mut contents: Vec<&str> = job.spec.files.iter().map(|f| f.contents.as_str()).collect();
    contents.sort_unstable();
    let mut want = vec![url.as_str(), "read-token-0001"];
    want.sort_unstable();
    assert_eq!(contents, want);
    for arg in &job.spec.args {
        assert!(!arg.contains(&url), "the pack url is in {arg:?}");
        assert!(!arg.contains("read-token-0001"), "the token is in {arg:?}");
    }
    for flag in ["--pack-url-file", "--grant-token-file"] {
        assert!(job.spec.args.iter().any(|a| a == flag), "{flag} is missing");
    }
    assert!(!job.spec.args.iter().any(|a| a == "--insecure-argv-secrets"));
}

/// Catches the config's choices not reaching the job: with a plain-http `PUT` and a working
/// directory asked for, the flags are passed, after the task tool's own, and the job's command
/// is still empty.
#[tokio::test(start_paused = true)]
async fn the_configs_choices_reach_the_job() {
    let mut rig = Rig::start(&[HOST], 1, |cfg| {
        cfg.allow_plain_http_put = true;
        cfg.work_dir = Some("/scratch/work".to_owned());
    })
    .await;
    let job = submitted(&mut rig).await;
    assert_eq!(job.spec.args[0], "--action");
    assert_eq!(job.arg("--work-dir"), Some("/scratch/work"));
    assert!(job.spec.args.iter().any(|a| a == "--allow-plain-http-put"));
    assert!(job.spec.command.is_empty());
}

/// Catches a bearer token sent in clear: a read grant that is not served over https is
/// infrastructure to rerun (the operator's to fix), and no job is submitted with the token in
/// its arguments.
#[tokio::test(start_paused = true)]
async fn a_read_grant_that_is_not_https_is_never_submitted() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    rig.inputs
        .grant
        .lock()
        .expect("grant")
        .as_mut()
        .expect("ok")
        .url = "http://grant.test:9400".to_owned();
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::Internal, "{finished:?}");
    assert!(rig.jobs().is_empty());
    assert!(submits(&rig.fake.calls()).is_empty());
}

/// Catches a read grant over plain http submitted without the operator's word, and the opt-in
/// not reaching the task: with `allow_plain_http_grant` set an `http://` grant is submitted,
/// its url is the task's `--grant-url`, and the task tool is given exactly one
/// `--allow-plain-http-grant`; an `https://` grant with the flag set is submitted too, with
/// the same one flag (the flag is the config's, not the url's).
#[tokio::test(start_paused = true)]
async fn a_plain_http_grant_is_submitted_only_when_allowed_and_the_task_is_told() {
    for url in ["http://grant.test:9400", "https://grant.test:9400"] {
        let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.allow_plain_http_grant = true).await;
        rig.pump().await;
        rig.inputs
            .grant
            .lock()
            .expect("grant")
            .as_mut()
            .expect("ok")
            .url = url.to_owned();
        let job = submitted(&mut rig).await;
        assert_eq!(job.arg("--grant-url"), Some(url));
        let given = job
            .spec
            .args
            .iter()
            .filter(|a| *a == "--allow-plain-http-grant")
            .count();
        assert_eq!(given, 1, "{:?}", job.spec.args.len());
    }
}

/// Catches the config's `locate` not reaching the task, or reaching it twice: with it set the
/// task tool is given exactly one `--locate` (the default config's jobs have none, which the
/// leak test above pins), and the grant and the other flags are those of a job without it.
#[tokio::test(start_paused = true)]
async fn executor_submits_job_with_locate_flag() {
    let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.locate = true).await;
    let job = submitted(&mut rig).await;
    let given = job.spec.args.iter().filter(|a| *a == "--locate").count();
    assert_eq!(given, 1, "{:?}", job.spec.args.len());
    assert_eq!(job.spec.args.last().map(String::as_str), Some("--locate"));
    assert_eq!(job.spec.args[0], "--action");
    assert!(job.arg("--grant-url").is_some());
}

/// Catches a write url sent in clear without the operator's word: a pack url that is not
/// https is infrastructure to rerun while `allow_plain_http_put` is unset, and no job is
/// submitted with it; with the flag set the same url is submitted.
#[tokio::test(start_paused = true)]
async fn a_pack_url_that_is_not_https_is_submitted_only_when_plain_http_is_allowed() {
    for allowed in [false, true] {
        let mut rig = Rig::start(&[HOST], 1, move |cfg| cfg.allow_plain_http_put = allowed).await;
        rig.presign
            .plain
            .store(true, std::sync::atomic::Ordering::Relaxed);
        rig.pump().await;
        let caller = rig.submit(3);
        match allowed {
            false => {
                let finished = rig.until_done(&caller, 3).await;
                assert_eq!(code(&finished), Code::Internal, "{finished:?}");
                assert!(rig.jobs().is_empty());
            }
            true => {
                let job = submitted(&mut rig).await;
                let url = job.arg("--pack-url").expect("a pack url");
                assert!(url.starts_with("http://store.test/"), "{url}");
            }
        }
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
    assert!(rig.jobs().is_empty(), "submitted without a key");
    assert!(submits(&rig.fake.calls()).is_empty());
}

/// Catches a key the mint would not issue being submitted anyway: the job needs a key to
/// write, and without one it is infrastructure, so nothing is submitted and the farm runs the
/// action again, which the mint then answers.
#[tokio::test(start_paused = true)]
async fn a_pack_key_that_cannot_be_issued_is_rerun_not_submitted() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    *rig.ingest.mint_fails.lock().expect("mint") =
        Some(MintError::Unavailable("no secret".to_owned()));
    let caller = rig.submit(3);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.exec.step().await;
    assert!(rig.jobs().is_empty(), "submitted without a key");
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
    for arch in ["x86_64", "aarch64"] {
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

/// Catches a job that needs the wrong scratch disk: the job carries the bytes of the input
/// closure the grant reports, the operator's output allowance and the fixed margin, each once.
/// The allowance is set to a number the default is not, so a job that ignores the config
/// cannot match; the input bytes are not a whole GiB, so a sum that drops them cannot either.
#[tokio::test(start_paused = true)]
async fn a_job_needs_its_inputs_plus_the_output_allowance_plus_the_margin() {
    let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.output_disk_bytes = 5 * GIB + 7).await;
    let job = submitted(&mut rig).await;
    let needs = job.spec.needs.as_ref().expect("needs");
    assert_eq!(
        needs.disk_bytes,
        Some(INPUT_BYTES + 5 * GIB + 7 + DISK_MARGIN)
    );
}

/// Catches a grant that does not say how large its closure is booking nothing, or booking the
/// wrong thing: the job needs the allowance and the margin, and the executor says in its log
/// that it did not know the inputs, so an operator who finds a full host can see why.
#[tokio::test(start_paused = true)]
async fn a_job_whose_inputs_are_not_measured_needs_the_allowance_and_says_so() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let _logged = tracing::dispatcher::set_default(&dispatch);
    let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.output_disk_bytes = 5 * GIB + 7).await;
    rig.pump().await;
    rig.inputs
        .grant
        .lock()
        .expect("grant")
        .as_mut()
        .expect("ok")
        .input_bytes = None;
    let _caller = rig.submit(3);
    rig.pump().await;
    let job = rig.jobs_of(3).remove(0);
    let needs = job.spec.needs.as_ref().expect("needs");
    assert_eq!(needs.disk_bytes, Some(5 * GIB + 7 + DISK_MARGIN));
    let said = log.text();
    assert!(
        said.contains("size of the inputs is not known")
            && said.contains("needs the allowance and the margin"),
        "{said}"
    );
}

/// Catches a job whose disk no host has being rerun for ever: the job service refuses it
/// `no-capable-host`, which is the caller's, not the farm's. Nothing is queued, the worker
/// stays (a refusal is not a reason to drop or silence it), and the action ends
/// `InvalidArgument` (no rerun), the message naming the code and none of the job's secrets.
/// With a host whose disk holds the job, the same executor submits it.
#[tokio::test(start_paused = true)]
async fn a_job_whose_disk_no_host_has_is_refused_cleanly() {
    let booked = INPUT_BYTES + 8 * GIB + DISK_MARGIN;
    let mut rig = Rig::start(&[], 1, |_| {}).await;
    rig.fake.add_host(HOST, host_with_disk(booked - 1));
    rig.pump().await;
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
    let said = message(&finished);
    assert!(
        said.contains("no-capable-host"),
        "the message does not name the code: {said}"
    );
    for secret in ["read-token-0001", "s3cr3t", "--pack-url", "--grant-token"] {
        assert!(!said.contains(secret), "the message holds {secret}: {said}");
    }
    assert!(rig.jobs().is_empty(), "a job no host can hold");
    assert_eq!(submits(&rig.fake.calls()).len(), 1, "the action was rerun");
    let workers = rig.exec.workers();
    assert_eq!(workers.len(), 1, "the worker was dropped");
    assert!(!workers[0].silent && !workers[0].draining);

    let mut rig = Rig::start(&[], 1, |_| {}).await;
    rig.fake.add_host(HOST, host_with_disk(booked));
    let job = submitted(&mut rig).await;
    let needs = job.spec.needs.as_ref().expect("needs");
    assert_eq!(needs.disk_bytes, Some(booked));
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
