//! The canary's verdict on an injected job, and its run against `kbf-job-client`'s in-process
//! job service, whose jobs move only when a test moves them: the pause between two asks is
//! the test's hook, so no test reads a clock or sleeps.

use js_proto::v1::{ExecMode, Shape};
use kbf_job_client::fake::{Call, FakeJobServer, Fault, Machine};

use super::*;

const IMAGE: &str =
    "registry.example/kbf@sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn canary() -> Canary {
    Canary {
        id: "kbf-canary-1767225600".to_owned(),
        image: IMAGE.to_owned(),
        arch: "x86_64".to_owned(),
        frontend: "fe-a".to_owned(),
    }
}

fn job(state: JobState, host: &str) -> Job {
    Job {
        id: canary().id,
        state: state.into(),
        host_id: host.to_owned(),
        ..Job::default()
    }
}

/// Behaviour: a job that succeeded passes, naming its host.
#[test]
fn a_succeeded_job_passes() {
    let id = canary().id;
    assert_eq!(
        judge(&id, Some(&job(JobState::Succeeded, "host-a"))),
        Verdict::Passed(
            "pass: the canary job kbf-canary-1767225600 succeeded on host host-a".to_owned()
        )
    );
}

/// Behaviour: every ended state but success fails, naming the state and the host; a start
/// failure's detail is quoted; a state this build does not know is a failure, never a pass.
#[test]
fn every_other_ended_state_fails_and_names_how() {
    let id = canary().id;
    for (state, word) in [
        (JobState::Failed, "failed"),
        (JobState::Cancelled, "cancelled"),
        (JobState::Lost, "lost"),
        (JobState::Unspecified, "unspecified"),
    ] {
        assert_eq!(
            judge(&id, Some(&job(state, "host-a"))),
            Verdict::Failed(format!("the canary job {id} ended {word} on host host-a")),
        );
    }
    let mut refused = job(JobState::FailedToStart, "");
    refused.start_failure_detail = "the image could not be pulled".to_owned();
    assert_eq!(
        judge(&id, Some(&refused)),
        Verdict::Failed(format!(
            "the canary job {id} ended failed to start on no host: the image could not be pulled"
        )),
    );
    let mut unknown = job(JobState::Succeeded, "host-a");
    unknown.state = 99;
    assert_eq!(
        judge(&id, Some(&unknown)),
        Verdict::Failed(format!(
            "the canary job {id} ended unspecified on host host-a"
        )),
    );
}

/// Behaviour: a job the service does not know fails.
#[test]
fn a_missing_job_fails() {
    assert_eq!(
        judge("kbf-canary-1", None),
        Verdict::Failed("the job service does not know the canary job kbf-canary-1".to_owned())
    );
}

/// Behaviour: a live job is pending, with its state.
#[test]
fn a_live_job_is_pending() {
    for (state, word) in [
        (JobState::Queued, "queued"),
        (JobState::Placed, "placed"),
        (JobState::Starting, "starting"),
        (JobState::Running, "running"),
    ] {
        assert_eq!(judge("j", Some(&job(state, ""))), Verdict::Pending(word));
    }
}

/// Behaviour: the canary is the kbf image's `--version`, with no network and no
/// `kbf-frontend` label (the executor stops on another frontend's labelled job), on the
/// executor's kind of host, needing 1 core and 1 GiB of memory and no disk.
#[test]
fn the_canary_job_has_no_network_and_no_frontend_label() {
    let spec = canary().spec();
    assert_eq!(spec.job_id, "kbf-canary-1767225600");
    assert_eq!(spec.image, IMAGE);
    assert_eq!(spec.command, "");
    assert_eq!(spec.args, ["--version"]);
    assert_eq!(spec.network, i32::from(NetworkLevel::None));
    assert_eq!(spec.wall_limit_seconds, Some(WALL_SECS));
    assert_eq!(spec.labels.len(), 1);
    assert_eq!(
        spec.labels.get("kbf-canary").map(String::as_str),
        Some("fe-a")
    );
    assert!(!spec.labels.contains_key("kbf-frontend"));
    let c = spec.constraints.unwrap();
    assert_eq!((c.os.as_str(), c.arch.as_str()), ("linux", "x86_64"));
    assert_eq!(c.exec, i32::from(ExecMode::Container));
    let needs = spec.needs.unwrap();
    assert_eq!((needs.cpu_millis, needs.memory_bytes), (1000, 1 << 30));
    assert_eq!(needs.disk_bytes, None, "the canary books no disk");
}

/// A service with one host the canary fits on, which reports no disk, as a host of a pool that
/// books none.
async fn served() -> FakeJobServer {
    let server = FakeJobServer::start().await.unwrap();
    server.add_host(
        "host-a",
        Machine {
            shape: Shape {
                cpu_millis: 4000,
                memory_bytes: 16 << 30,
                disk_bytes: None,
                ..Shape::default()
            },
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container,
        },
    );
    server
}

/// Behaviour: the canary submits its job and asks after it until it ends: placed, starting,
/// running, then exit 0 is a pass, after one ask per move.
#[tokio::test]
async fn a_job_that_runs_to_exit_0_passes() {
    let server = served().await;
    let client = server.client().unwrap();
    let c = canary();
    let mut moves = 0;
    let mut pause = || {
        moves += 1;
        match moves {
            1 => assert!(server.place(&c.id, "host-a")),
            2 => assert!(server.starting(&c.id)),
            3 => assert!(server.run(&c.id)),
            _ => assert!(server.finish(&c.id, 0)),
        }
    };
    let line = run(&client, &c, 10, &mut pause).await.unwrap();
    assert_eq!(
        line,
        format!("pass: the canary job {} succeeded on host host-a", c.id)
    );
    assert_eq!(moves, 4);
    assert_eq!(server.submitted(&c.id), Some(c.spec()));
}

/// Behaviour: a job that exits 1 fails the canary.
#[tokio::test]
async fn a_job_that_exits_1_fails() {
    let server = served().await;
    let client = server.client().unwrap();
    let c = canary();
    let mut pause = || {
        server.place(&c.id, "host-a");
        server.starting(&c.id);
        server.run(&c.id);
        server.finish(&c.id, 1);
    };
    let why = run(&client, &c, 10, &mut pause).await.unwrap_err();
    assert_eq!(
        why,
        format!("the canary job {} ended failed on host host-a", c.id)
    );
}

/// Behaviour: a job that never starts (its host refused it) fails with the host's words.
#[tokio::test]
async fn a_job_that_fails_to_start_fails_with_the_detail() {
    let server = served().await;
    let client = server.client().unwrap();
    let c = canary();
    let mut pause = || {
        server.place(&c.id, "host-a");
        server.refuse_start(&c.id, "the image could not be pulled");
    };
    let why = run(&client, &c, 10, &mut pause).await.unwrap_err();
    assert!(
        why.starts_with(&format!("the canary job {} ended ", c.id)),
        "{why}"
    );
    assert!(why.ends_with(": the image could not be pulled"), "{why}");
}

/// Behaviour: a job still live after the last ask is cancelled, and the canary fails naming
/// its last state; a deadline of one ask never pauses.
#[tokio::test]
async fn a_job_that_does_not_end_in_time_is_cancelled() {
    let server = served().await;
    let client = server.client().unwrap();
    let c = canary();
    let mut paused = 0;
    let why = run(&client, &c, 3, &mut || paused += 1).await.unwrap_err();
    assert_eq!(
        why,
        format!(
            "the canary job {} did not end in time (still queued); it was cancelled",
            c.id
        )
    );
    assert_eq!(paused, 2);
    assert!(
        server
            .calls()
            .iter()
            .any(|call| matches!(call, Call::CancelJob(_)))
    );
    let why = run(
        &client,
        &Canary {
            id: "kbf-canary-2".to_owned(),
            ..c
        },
        0,
        &mut || panic!("one ask needs no pause"),
    )
    .await
    .unwrap_err();
    assert!(why.contains("(still queued); it was cancelled"), "{why}");
}

/// Behaviour: a cancel that fails is said, not hidden.
#[tokio::test]
async fn a_cancel_that_fails_is_reported() {
    let server = served().await;
    let client = server.client().unwrap();
    server.inject_on(
        "CancelJob",
        Fault::Status {
            code: tonic::Code::PermissionDenied,
            message: "no".to_owned(),
        },
    );
    let why = run(&client, &canary(), 1, &mut || {}).await.unwrap_err();
    assert!(why.contains("(still queued); its cancel failed: "), "{why}");
}

/// Behaviour: a submit the service refuses fails before any ask, with the service's answer.
#[tokio::test]
async fn a_refused_submit_fails() {
    let server = served().await;
    let client = server.client().unwrap();
    server.inject_on(
        "SubmitJob",
        Fault::Status {
            code: tonic::Code::PermissionDenied,
            message: "not yours".to_owned(),
        },
    );
    let why = run(&client, &canary(), 5, &mut || panic!("no ask"))
        .await
        .unwrap_err();
    assert!(
        why.starts_with("the job service did not take the canary job kbf-canary-1767225600: "),
        "{why}"
    );
}

/// Behaviour: a job the service no longer knows (`NOT_FOUND`) fails as missing, and any
/// other failed ask fails as unreadable.
#[tokio::test]
async fn a_job_that_is_gone_or_unreadable_fails() {
    let server = served().await;
    let client = server.client().unwrap();
    server.inject_on(
        "GetJob",
        Fault::Status {
            code: tonic::Code::NotFound,
            message: "gone".to_owned(),
        },
    );
    let why = run(&client, &canary(), 5, &mut || {}).await.unwrap_err();
    assert_eq!(
        why,
        "the job service does not know the canary job kbf-canary-1767225600"
    );
    server.inject_on(
        "GetJob",
        Fault::Status {
            code: tonic::Code::PermissionDenied,
            message: "no".to_owned(),
        },
    );
    let c = Canary {
        id: "kbf-canary-3".to_owned(),
        ..canary()
    };
    let why = run(&client, &c, 5, &mut || {}).await.unwrap_err();
    assert!(
        why.starts_with("cannot read the canary job kbf-canary-3: "),
        "{why}"
    );
}
