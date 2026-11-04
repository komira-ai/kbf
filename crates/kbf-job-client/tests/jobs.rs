//! Reading, listing and cancelling jobs (design section 5.3), and what `GetJob` shows of a
//! silent host (design sections 3.2 and 6.3).

mod support;

use std::time::Duration;

use js_proto::v1::{JobState, ListJobsRequest};
use kbf_job_client::Error;
use support::{HOST, fake, machine, map, spec, status_code, to_running};
use tonic::Code;

/// Asserts host silence: `silence_host` makes `host_silent_since_ms` the fake's time of the
/// silence on the placed, starting and running jobs of that host, and on no job of another
/// host and no queued job; no state changes; `unsilence_host` clears it; silence never ends
/// a job, and `lose` does, after which the field is gone. Catches the field missing, set on
/// the wrong jobs, stamped with the wrong time, or silence moving a job.
#[tokio::test]
async fn host_silence_shows_on_get_job_and_changes_no_state() {
    let (server, client) = fake().await;
    server.add_host("host-b", machine());
    for id in ["placed", "starting", "running", "other", "queued"] {
        client.submit_job(spec(id)).await.unwrap();
    }
    assert!(server.place("placed", HOST));
    assert!(server.place("starting", HOST) && server.start("starting"));
    to_running(&server, "running");
    assert!(server.place("other", "host-b"));

    server.advance(Duration::from_secs(30));
    assert!(server.silence_host(HOST));
    let since = server.now_ms();
    let states = [
        ("placed", JobState::Placed, Some(since)),
        ("starting", JobState::Starting, Some(since)),
        ("running", JobState::Running, Some(since)),
        ("other", JobState::Placed, None),
        ("queued", JobState::Queued, None),
    ];
    server.advance(Duration::from_secs(600));
    for (id, state, silent) in states {
        let job = client.get_job(id).await.unwrap();
        assert_eq!(
            (job.state(), job.host_silent_since_ms),
            (state, silent),
            "{id}"
        );
    }

    assert!(server.unsilence_host(HOST));
    for id in ["placed", "starting", "running"] {
        let job = client.get_job(id).await.unwrap();
        assert_eq!(job.host_silent_since_ms, None, "{id}");
    }

    assert!(server.silence_host(HOST));
    assert!(server.lose("running"));
    let lost = client.get_job("running").await.unwrap();
    assert_eq!(
        (lost.state(), lost.host_silent_since_ms),
        (JobState::Lost, None)
    );
    assert!(!server.silence_host("host-z"));
    assert!(!server.unsilence_host("host-z"));
}

/// Asserts each branch of the design's cancel: a queued job is cancelled at once; a running
/// job answers with `cancel_requested` set and stays running until its host reports it
/// stopped, then is cancelled; a placed and a starting job wait the same way; an ended job is
/// unchanged; cancelling twice answers as once; an unknown id is `NOT_FOUND`. Catches a
/// cancel that ends a running job before its host stops it, one that changes an ended job,
/// and a second cancel that is refused.
#[tokio::test]
async fn cancel_follows_the_states_of_the_design() {
    let (server, client) = fake().await;
    for id in ["queued", "placed", "starting", "running", "done"] {
        client.submit_job(spec(id)).await.unwrap();
    }

    let cancelled = client.cancel_job("queued").await.unwrap();
    assert_eq!(cancelled.state(), JobState::Cancelled);
    assert!(
        !cancelled.cancel_requested,
        "an ended job has no pending cancel"
    );
    assert!(cancelled.times.unwrap().ended_at_ms.is_some());
    assert_eq!(
        client.cancel_job("queued").await.unwrap(),
        cancelled,
        "twice is once"
    );

    assert!(server.place("placed", HOST));
    assert!(server.place("starting", HOST) && server.start("starting"));
    to_running(&server, "running");
    for (id, state) in [
        ("placed", JobState::Placed),
        ("starting", JobState::Starting),
        ("running", JobState::Running),
    ] {
        let asked = client.cancel_job(id).await.unwrap();
        assert_eq!(
            (asked.state(), asked.cancel_requested),
            (state, true),
            "{id}"
        );
        assert_eq!(
            client.cancel_job(id).await.unwrap(),
            asked,
            "{id}: twice is once"
        );
        assert!(server.host_stopped(id), "{id}");
        let stopped = client.get_job(id).await.unwrap();
        assert_eq!(stopped.state(), JobState::Cancelled, "{id}");
        let result = client.get_result(id).await.unwrap();
        assert_eq!(
            result.reason(),
            js_proto::v1::ResultReason::Cancelled,
            "{id}"
        );
    }

    to_running(&server, "done");
    assert!(server.finish("done", 0));
    let ended = client.get_job("done").await.unwrap();
    assert_eq!(client.cancel_job("done").await.unwrap(), ended);

    let unknown = client.cancel_job("no-such-job").await.unwrap_err();
    assert_eq!(status_code(&unknown), Code::NotFound);
    assert!(!unknown.may_have_applied());
}

/// Asserts the filters and the pages of a listing: by state, by every label given, by the
/// time queued (from inclusive, to exclusive) and all together each return exactly the
/// matching jobs; pages of two chain by `next_page_token` to every job once, in the order
/// they were recorded, the last with an empty token; and a page token the service did not
/// write is `INVALID_ARGUMENT`. Catches an off-by-one in the token (a job missed or repeated
/// at a page's edge) and a filter that ORs what it should AND.
#[tokio::test]
async fn list_jobs_filters_and_pages() {
    let (server, client) = fake().await;
    let labelled = [
        ("a", "build", "ci"),
        ("b", "build", "ci"),
        ("c", "test", "ci"),
        ("d", "build", "local"),
        ("e", "test", "local"),
    ];
    for (id, team, kind) in labelled {
        let mut job = spec(id);
        job.labels = map(&[("team", team), ("kind", kind)]);
        client.submit_job(job).await.unwrap();
        server.advance(Duration::from_secs(1));
    }
    to_running(&server, "b");
    to_running(&server, "d");
    let ids = |jobs: Vec<js_proto::v1::Job>| -> Vec<String> {
        jobs.into_iter().map(|job| job.id).collect()
    };
    let cases = [
        (
            ListJobsRequest {
                states: vec![JobState::Running.into()],
                ..ListJobsRequest::default()
            },
            vec!["b", "d"],
        ),
        (
            ListJobsRequest {
                labels: map(&[("team", "build"), ("kind", "ci")]),
                ..ListJobsRequest::default()
            },
            vec!["a", "b"],
        ),
        (
            ListJobsRequest {
                queued_after_ms: Some(1000),
                queued_before_ms: Some(3000),
                ..ListJobsRequest::default()
            },
            vec!["b", "c"],
        ),
        (
            ListJobsRequest {
                states: vec![JobState::Queued.into(), JobState::Running.into()],
                labels: map(&[("team", "build")]),
                queued_after_ms: Some(1000),
                ..ListJobsRequest::default()
            },
            vec!["b", "d"],
        ),
        (
            ListJobsRequest {
                labels: map(&[("team", "nobody")]),
                ..ListJobsRequest::default()
            },
            vec![],
        ),
    ];
    for (filter, expected) in cases {
        let page = client.list_jobs(filter.clone()).await.unwrap();
        assert_eq!(ids(page.jobs), expected, "{filter:?}");
        assert_eq!(page.next_page_token, "");
        assert_eq!(ids(client.list_all_jobs(filter).await.unwrap()), expected);
    }

    let mut request = ListJobsRequest {
        page_size: 2,
        ..ListJobsRequest::default()
    };
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let page = client.list_jobs(request.clone()).await.unwrap();
        pages += 1;
        assert!(page.jobs.len() <= 2);
        seen.extend(ids(page.jobs));
        if page.next_page_token.is_empty() {
            break;
        }
        request.page_token = page.next_page_token;
    }
    assert_eq!(
        (seen, pages),
        (["a", "b", "c", "d", "e"].map(String::from).to_vec(), 3)
    );
    let paged = ListJobsRequest {
        page_size: 2,
        ..ListJobsRequest::default()
    };
    assert_eq!(ids(client.list_all_jobs(paged).await.unwrap()).len(), 5);

    let forged = ListJobsRequest {
        page_token: "page-two".to_owned(),
        ..ListJobsRequest::default()
    };
    let error = client.list_jobs(forged).await.unwrap_err();
    assert_eq!(status_code(&error), Code::InvalidArgument);
}

/// Asserts the cap of a listing that follows its pages: 10000 jobs come back whole, and the
/// 10001st makes it `Error::TooMany` with the cap, not a listing cut short without saying
/// so. Catches a cap off by one either way and a truncation that hides jobs.
#[tokio::test]
async fn list_all_jobs_stops_at_the_cap() {
    let (server, client) = fake().await;
    assert_eq!(kbf_job_client::Client::MAX_LISTED, 10_000);
    for n in 0..10_000 {
        server.record(spec(&format!("job-{n}"))).unwrap();
    }
    let pages = ListJobsRequest {
        page_size: 1000,
        ..ListJobsRequest::default()
    };
    let all = client.list_all_jobs(pages.clone()).await.unwrap();
    assert_eq!(all.len(), 10_000);
    assert_eq!(
        (all[0].id.as_str(), all[9_999].id.as_str()),
        ("job-0", "job-9999")
    );

    server.record(spec("job-10000")).unwrap();
    let error = client.list_all_jobs(pages).await.unwrap_err();
    assert!(matches!(error, Error::TooMany(10_000)), "{error:?}");
    assert!(!error.may_have_applied());
}
