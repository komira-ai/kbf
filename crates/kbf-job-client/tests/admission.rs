//! Admission (design sections 3.2 and 5.3): a submit no machine could ever serve, or one
//! without a usable wall limit, is refused, and a refusal records nothing.

mod support;

use std::time::Duration;

use js_proto::v1::{Needs, RefusalCode};
use kbf_job_client::fake::FakeJobServer;
use support::{HOST, machine, spec};

/// Asserts that `no-capable-host` is an answer and not a state: a job that no host and no
/// offer fits is `Error::Refused` with that code (the client reads the refusal from the
/// `FAILED_PRECONDITION` status's details), nothing is recorded, and the same job is taken
/// once a host that fits is added; an offer of a machine that fits admits a job too. A wall
/// limit that is absent or 0 is `wall-time-not-positive`, one above the ceiling
/// `wall-time-above-ceiling`, and one at the ceiling is taken. Catches a fake that records a
/// refused job, a client that maps `FAILED_PRECONDITION` to `Error::Status` and loses the
/// refusal, and a ceiling compared the wrong way.
#[tokio::test]
async fn no_capable_host_is_an_answer_not_a_job() {
    let server = FakeJobServer::start().await.unwrap();
    let client = server.client().unwrap();
    let mut big = spec("big");
    big.needs = Some(Needs {
        cpu_millis: 64_000,
        memory_bytes: 1 << 30,
        ..Needs::default()
    });

    let error = client.submit_job(big.clone()).await.unwrap_err();
    assert!(error.is(RefusalCode::NoCapableHost), "{error:?}");
    assert!(!error.may_have_applied());
    assert!(server.jobs().is_empty(), "a refusal records nothing");

    let mut wide = machine();
    wide.shape.cpu_millis = 64_000;
    server.add_host(HOST, wide);
    assert_eq!(client.submit_job(big).await.unwrap().id, "big");

    let mut gpus = spec("gpus");
    gpus.needs = Some(Needs {
        cpu_millis: 1000,
        memory_bytes: 1 << 30,
        gpus: 8,
        ..Needs::default()
    });
    let error = client.submit_job(gpus.clone()).await.unwrap_err();
    assert!(error.is(RefusalCode::NoCapableHost), "{error:?}");
    let mut launchable = machine();
    launchable.shape.gpus = 8;
    launchable.shape.gpu_class = String::new();
    server.add_offer(launchable);
    let admitted = client.submit_job(gpus).await.unwrap();
    assert!(
        admitted.host_id.is_empty(),
        "an offer admits; it holds nothing"
    );

    for wall in [None, Some(0)] {
        let mut job = spec("no-wall");
        job.wall_limit_seconds = wall;
        let error = client.submit_job(job).await.unwrap_err();
        assert!(
            error.is(RefusalCode::WallTimeNotPositive),
            "{wall:?}: {error:?}"
        );
    }
    server.set_wall_ceiling(Duration::from_secs(7200));
    let mut long = spec("long");
    long.wall_limit_seconds = Some(7201);
    let error = client.submit_job(long).await.unwrap_err();
    assert!(error.is(RefusalCode::WallTimeAboveCeiling), "{error:?}");
    assert_eq!(
        error.refusal().unwrap().detail,
        "the wall limit 7201 s is above the ceiling of 7200 s"
    );
    let mut at_ceiling = spec("at-ceiling");
    at_ceiling.wall_limit_seconds = Some(7200);
    client.submit_job(at_ceiling).await.unwrap();

    let ids: Vec<String> = server.jobs().into_iter().map(|job| job.id).collect();
    assert_eq!(ids, ["big", "gpus", "at-ceiling"]);
}
