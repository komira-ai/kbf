//! A submit is idempotent on the job id (design section 5.3, step 2): what a caller relies on
//! to retry after a lost answer.

mod support;

use js_proto::v1::{JobFile, JobSpec, JobState, Needs, NetworkLevel, PortForward, RefusalCode};
use kbf_job_client::Error;
use kbf_job_client::fake::Call;
use support::{HOST, IMAGE, fake, full_spec, map, spec, status_code, to_running};

/// A change made to a job spec.
type Change = fn(&mut JobSpec);

/// Asserts the retry rule the executor relies on: when the fake applies a submit and then
/// loses its answer, the call is `Error::Transport` with `may_have_applied()` true; the same
/// spec submitted again with the same id answers a job with that id in the state the first
/// submit left it in, and the fake holds exactly one job. The fake saw the two submits with
/// the same request. Catches a client that mints a new id on retry, a fake that makes a
/// second job, and a lost answer classed as "nothing sent", after which a caller would not
/// know to look.
#[tokio::test]
async fn submit_retry_after_a_lost_answer_makes_one_job() {
    let (server, client) = fake().await;
    server.lose_next_answer();
    let lost = client.submit_job(spec("build-7")).await.unwrap_err();
    assert!(matches!(lost, Error::Transport(_)), "{lost:?}");
    assert!(lost.may_have_applied());
    assert_eq!(server.jobs().len(), 1, "the lost submit was applied");

    let job = client.submit_job(spec("build-7")).await.unwrap();
    assert_eq!(job.id, "build-7");
    assert_eq!(job.state(), JobState::Queued);
    let jobs = server.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0], job);
    let calls = server.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1], "the retry sent the same request");

    // A retry answers the job in whatever state it is now.
    to_running(&server, "build-7");
    let again = client.submit_job(spec("build-7")).await.unwrap();
    assert_eq!(
        (again.state(), again.host_id.as_str()),
        (JobState::Running, HOST)
    );
    assert_eq!(server.jobs().len(), 1);
}

/// Asserts the idempotency boundary: under an id already held, a job with another image,
/// command or needs is `Error::Refused` with `job-id-conflict` and the service's detail, and
/// the held job is unchanged; a job that differs only in its priority is answered from the
/// held job, whose priority stays the first one. Catches a client that folds every
/// `ALREADY_EXISTS` into success, a refusal whose details are dropped, and a fake that
/// compares the wrong fields.
#[tokio::test]
async fn same_id_different_job_is_a_conflict() {
    let (server, client) = fake().await;
    let first = client.submit_job(spec("build-8")).await.unwrap();
    let different: [(&str, Change); 3] = [
        ("image", |job| {
            job.image = IMAGE.replace("kbf-task", "kbf-other")
        }),
        ("command", |job| job.command = "/bin/test".to_owned()),
        ("needs", |job| {
            job.needs = Some(Needs {
                cpu_millis: 2000,
                memory_bytes: 1 << 30,
                ..Needs::default()
            });
        }),
    ];
    for (change, apply) in different {
        let mut job = spec("build-8");
        apply(&mut job);
        let error = client.submit_job(job).await.unwrap_err();
        assert!(error.is(RefusalCode::JobIdConflict), "{change}: {error:?}");
        assert!(!error.may_have_applied());
        let refusal = error.refusal().unwrap();
        assert_eq!(refusal.detail, "build-8 names another job");
    }
    assert_eq!(
        server.jobs(),
        std::slice::from_ref(&first),
        "the held job is unchanged"
    );

    let mut reprioritised = spec("build-8");
    reprioritised.priority = 9;
    let answered = client.submit_job(reprioritised).await.unwrap();
    assert_eq!(answered, first);
    assert_eq!(answered.spec.unwrap().priority, 0);
    assert_eq!(server.jobs().len(), 1);
}

/// Asserts what the client refuses before sending a submit, and what the service refuses
/// that the client does not check: an empty id and one with a slash are
/// `Unreachable` with nothing sent, so the client never lets the service mint an id the
/// caller would not know; an image without a digest reaches the service and is
/// `INVALID_ARGUMENT` without a refusal. The fake mints `j-<counter>` for a job recorded
/// without an id. Catches a client that sends an empty id (a retry would then run the work
/// twice) and an image check moved into the client that the service would not share.
#[tokio::test]
async fn an_id_is_checked_before_sending_and_an_image_by_the_service() {
    let (server, client) = fake().await;
    for id in ["", "a/b"] {
        let error = client.submit_job(spec(id)).await.unwrap_err();
        assert!(
            matches!(error, Error::Unreachable(kbf_job_client::Unsent::BadId(_))),
            "{id:?}: {error:?}"
        );
        assert!(!error.may_have_applied());
    }
    assert!(server.calls().is_empty(), "nothing was sent");

    let mut tagged = spec("tagged");
    tagged.image = "registry.example/kbf-task:latest".to_owned();
    let error = client.submit_job(tagged).await.unwrap_err();
    assert_eq!(status_code(&error), tonic::Code::InvalidArgument);
    assert!(error.refusal().is_none());
    assert!(matches!(server.calls()[..], [Call::SubmitJob(_)]));

    let minted = server.record(spec("")).unwrap();
    assert_eq!(minted.id, "j-1");
    assert_eq!(server.jobs().len(), 1);
}

/// Asserts that every field of a fully populated job but its priority is part of its
/// identity: under the id of a held job, a spec that differs in its image, command, args,
/// env, needs, constraints, wall limit, network level, ports, files (their names, their
/// contents alone, and their number) or labels, one field at a time, is `job-id-conflict`
/// and the held job is unchanged; one that differs in its priority only is answered from the
/// held job; and once the held job has ended the same spec answers the ended job, unchanged,
/// and records no new job. Catches a comparison that skips a field (the second submit would
/// be taken for a retry and its work lost) and one that compares the priority.
#[tokio::test]
async fn each_field_but_the_priority_is_part_of_the_job() {
    let (server, client) = fake().await;
    let held = client
        .submit_job(full_spec("https://a.example/x"))
        .await
        .unwrap();
    assert_eq!(held.id, "job-1");
    let changes: [(&str, Change); 16] = [
        ("image", |job| job.image = digest_of('f')),
        ("command", |job| job.command = "/bin/other".to_owned()),
        ("args", |job| job.args.push("more".to_owned())),
        ("env", |job| {
            job.env.insert("C_VAR".to_owned(), "3".to_owned());
        }),
        ("env value", |job| {
            job.env.insert("A_VAR".to_owned(), "9".to_owned());
        }),
        ("needs", |job| job.needs.as_mut().unwrap().cpu_millis += 1),
        ("constraints", |job| {
            job.constraints.as_mut().unwrap().arch = String::new();
        }),
        ("wall limit", |job| job.wall_limit_seconds = Some(3601)),
        ("network", |job| job.network = NetworkLevel::Open.into()),
        ("ports", |job| {
            job.ports.push(PortForward {
                host_port: 9090,
                container_port: 90,
            });
        }),
        ("file name", |job| {
            job.files[0].name = "other-name".to_owned()
        }),
        ("file contents", |job| {
            job.files[0].contents = "https://b.example/y".to_owned();
        }),
        ("another file", |job| {
            job.files.push(JobFile {
                name: "second".to_owned(),
                contents: String::new(),
            });
        }),
        ("no file", |job| job.files.clear()),
        ("labels", |job| {
            job.labels.insert("extra".to_owned(), "1".to_owned());
        }),
        ("labels value", |job| {
            job.labels = map(&[("team", "other"), ("kind", "test")]);
        }),
    ];
    for (field, apply) in changes {
        let mut job = full_spec("https://a.example/x");
        apply(&mut job);
        assert_ne!(
            job,
            full_spec("https://a.example/x"),
            "{field} changed nothing"
        );
        let error = client.submit_job(job).await.unwrap_err();
        assert!(error.is(RefusalCode::JobIdConflict), "{field}: {error:?}");
        assert!(!error.may_have_applied());
        assert_eq!(server.jobs(), std::slice::from_ref(&held), "{field}");
    }

    let mut reprioritised = full_spec("https://a.example/x");
    reprioritised.priority = 1;
    assert_eq!(client.submit_job(reprioritised).await.unwrap(), held);
    assert_eq!(server.jobs(), std::slice::from_ref(&held));

    to_running(&server, "job-1");
    assert!(server.finish("job-1", 0));
    let ended = client.get_job("job-1").await.unwrap();
    assert_eq!(ended.state(), JobState::Succeeded);
    let replay = client
        .submit_job(full_spec("https://a.example/x"))
        .await
        .unwrap();
    assert_eq!(replay, ended);
    assert_eq!(server.jobs(), [ended]);
}

/// The image of the full sample with the digest made of `digit` instead.
fn digest_of(digit: char) -> String {
    format!(
        "registry.example/app@sha256:{}",
        digit.to_string().repeat(64)
    )
}
