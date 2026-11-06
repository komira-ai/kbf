//! Reading, listing and cancelling jobs (design section 5.3), and what `GetJob` shows of a
//! silent host (design sections 3.2 and 6.3).

mod support;

use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{Job, JobState, ListJobsRequest, ListJobsResponse};
use kbf_job_client::fake::{Call, FakeJobServer, Fault};
use kbf_job_client::{Client, Error, Stuck};
use prost::Message as _;
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
    assert!(server.place("starting", HOST) && server.starting("starting"));
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

/// Asserts each branch of the design's cancel: a queued job is cancelled at once and keeps
/// `cancel_requested`, as the released service answers it (kbf-integration-test's
/// tests/js_real_service.rs holds the fake to that service); a running job answers with
/// `cancel_requested` set and stays running until its host reports it stopped, then is
/// cancelled; a placed and a starting job wait the same way; an ended job is
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
        cancelled.cancel_requested,
        "a queued job cancelled keeps the intent, as the service answers"
    );
    assert!(cancelled.times.unwrap().ended_at_ms.is_some());
    assert_eq!(
        client.cancel_job("queued").await.unwrap(),
        cancelled,
        "twice is once"
    );

    assert!(server.place("placed", HOST));
    assert!(server.place("starting", HOST) && server.starting("starting"));
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

/// A scripted answer of `ListJobs`: one job for each id in `ids`, and `token` to go on with.
fn page(ids: &[String], token: &str) -> Fault {
    let jobs = ids
        .iter()
        .map(|id| Job {
            id: id.clone(),
            ..Job::default()
        })
        .collect();
    let response = ListJobsResponse {
        jobs,
        next_page_token: token.to_owned(),
    };
    Fault::Answer(Bytes::from(response.encode_to_vec()))
}

/// A scripted page of one job `id` that goes on with `token`.
fn full(id: &str, token: &str) -> Fault {
    page(&[id.to_owned()], token)
}

/// A scripted empty page that goes on with `token`.
fn empty(token: &str) -> Fault {
    page(&[], token)
}

/// Queues `pages` as the answers of the next calls of `ListJobs`, in order.
fn script(server: &FakeJobServer, pages: impl IntoIterator<Item = Fault>) {
    for fault in pages {
        server.inject_on("ListJobs", fault);
    }
}

/// How many calls of `ListJobs` the fake read.
fn list_calls(server: &FakeJobServer) -> usize {
    let listed = |call: &Call| matches!(call, Call::ListJobs(_));
    server.calls().iter().filter(|call| listed(call)).count()
}

/// Asserts that a listing that follows its pages stops, with the token, when the service
/// gives a token it gave before: a nonempty page with `t1` followed by a nonempty page with
/// `t1` again, and a cycle `t1`, `t2`, `t1`, are each `Error::Stuck(RepeatedToken("t1"))`
/// after exactly the pages needed to see the repeat (2 and 3 calls), and the error is not a
/// transport error a caller would retry. Catches a listing that asks for ever, one that
/// compares only with the previous token, and one that reports the wrong token.
#[tokio::test]
async fn list_all_jobs_stops_on_a_repeated_token() {
    let (server, client) = fake().await;
    script(&server, [full("x", "t1"), full("y", "t1")]);
    let error = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Stuck(Stuck::RepeatedToken(token)) if token == "t1"),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert_eq!(list_calls(&server), 2);

    let (server, client) = fake().await;
    script(&server, [full("x", "t1"), full("y", "t2"), full("z", "t1")]);
    let error = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Stuck(Stuck::RepeatedToken(token)) if token == "t1"),
        "{error:?}"
    );
    assert_eq!(list_calls(&server), 3);
}

/// Asserts the bound on empty pages: `Client::MAX_EMPTY_PAGES` empty pages in a row, each
/// with a fresh token, are `Stuck(EmptyPages(100))` after 100 calls; 99 empty pages then an
/// empty page with no token are an empty listing after 100 calls; and a nonempty page resets
/// the count, so 99 empty pages, a page of one job, two empty pages and a last page of one
/// job are both jobs after 103 calls. Catches a bound off by one either way, a count that
/// never resets, and a count that includes the last page.
#[tokio::test]
async fn list_all_jobs_bounds_the_empty_pages() {
    let max = Client::MAX_EMPTY_PAGES;
    assert_eq!(max, 100);
    let token = |n: usize| format!("t{n}");

    let (server, client) = fake().await;
    script(&server, (0..max).map(|n| empty(&token(n))));
    let error = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Stuck(Stuck::EmptyPages(100))),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert_eq!(list_calls(&server), 100);

    let (server, client) = fake().await;
    script(&server, (0..max - 1).map(|n| empty(&token(n))));
    script(&server, [empty("")]);
    let all = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert!(all.is_empty());
    assert_eq!(list_calls(&server), 100);

    let (server, client) = fake().await;
    script(&server, (0..max - 1).map(|n| empty(&token(n))));
    script(&server, [full("first", &token(max))]);
    script(&server, [empty(&token(max + 1)), empty(&token(max + 2))]);
    script(&server, [full("last", "")]);
    let all = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    let ids: Vec<&str> = all.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, ["first", "last"]);
    assert_eq!(list_calls(&server), 103);
}

/// Asserts a filtered listing in pages of one: by label, states and queued times with
/// `page_size` 1, `list_all_jobs` returns every match once in order, and each of the three
/// requests the fake read carries the same states, labels, times and page size, with the
/// page token empty, then the tokens the service gave; a listing started from a continuation
/// token returns the rest only. Catches a follow-up page that drops the filters or the page
/// size, and a continuation that starts again from the first page.
#[tokio::test]
async fn a_filtered_listing_keeps_its_filters_on_every_page() {
    let (server, client) = fake().await;
    let teams = [
        ("a", "build"),
        ("b", "build"),
        ("c", "test"),
        ("d", "build"),
        ("e", "build"),
        ("f", "build"),
    ];
    for (id, team) in teams {
        let mut job = spec(id);
        job.labels = map(&[("team", team)]);
        client.submit_job(job).await.unwrap();
        server.advance(Duration::from_secs(1));
    }
    let filter = ListJobsRequest {
        states: vec![JobState::Queued.into()],
        labels: map(&[("team", "build")]),
        queued_after_ms: Some(1000),
        queued_before_ms: Some(5000),
        page_size: 1,
        page_token: String::new(),
    };
    let ids = |jobs: Vec<Job>| -> Vec<String> { jobs.into_iter().map(|job| job.id).collect() };

    let all = client.list_all_jobs(filter.clone()).await.unwrap();
    assert_eq!(ids(all), ["b", "d", "e"]);
    let requests: Vec<ListJobsRequest> = server
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::ListJobs(request) => Some(request),
            _ => None,
        })
        .collect();
    let tokens: Vec<&str> = requests.iter().map(|r| r.page_token.as_str()).collect();
    assert_eq!(tokens, ["", "p1", "p2"]);
    for request in &requests {
        let same = ListJobsRequest {
            page_token: String::new(),
            ..request.clone()
        };
        assert_eq!(same, filter);
    }

    let first = client.list_jobs(filter.clone()).await.unwrap();
    assert_eq!(first.next_page_token, "p3");
    let rest = ListJobsRequest {
        page_token: first.next_page_token,
        ..filter
    };
    let before = list_calls(&server);
    assert_eq!(ids(client.list_all_jobs(rest).await.unwrap()), ["d", "e"]);
    assert_eq!(list_calls(&server) - before, 2);
}

/// Asserts that a failure on a later page is the error of the listing and not a short list:
/// a scripted first page with a token, then `UNAVAILABLE` on the second, is
/// `Error::Transport`, with two calls read. Catches a loop that returns the jobs collected so
/// far when a page fails.
#[tokio::test]
async fn a_later_page_that_fails_fails_the_listing() {
    let (server, client) = fake().await;
    script(&server, [full("x", "t1")]);
    server.inject_on(
        "ListJobs",
        Fault::Status {
            code: Code::Unavailable,
            message: "draining".to_owned(),
        },
    );
    let error = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");
    assert!(error.may_have_applied());
    assert_eq!(list_calls(&server), 2);
}

/// Asserts that a page token is the service's: `p999`, written like a real token but never
/// given, is `INVALID_ARGUMENT`; a real token given with another listing's labels is too,
/// and with its own labels it answers the next page. Catches a fake that reads any
/// well-formed token as a place in the list, and a token that continues a different listing.
#[tokio::test]
async fn a_page_token_belongs_to_its_listing() {
    let (server, client) = fake().await;
    for id in ["a", "b", "c"] {
        let mut job = spec(id);
        job.labels = map(&[("team", "build")]);
        client.submit_job(job).await.unwrap();
    }
    let forged = ListJobsRequest {
        page_token: "p999".to_owned(),
        ..ListJobsRequest::default()
    };
    let error = client.list_jobs(forged).await.unwrap_err();
    assert_eq!(status_code(&error), Code::InvalidArgument);

    let build = ListJobsRequest {
        labels: map(&[("team", "build")]),
        page_size: 1,
        ..ListJobsRequest::default()
    };
    let first = client.list_jobs(build.clone()).await.unwrap();
    assert_eq!(first.next_page_token, "p1");
    let other = ListJobsRequest {
        labels: map(&[("team", "test")]),
        page_token: first.next_page_token.clone(),
        ..build.clone()
    };
    let error = client.list_jobs(other).await.unwrap_err();
    assert_eq!(status_code(&error), Code::InvalidArgument);
    let unfiltered = ListJobsRequest {
        labels: map(&[]),
        page_token: first.next_page_token.clone(),
        ..build.clone()
    };
    let error = client.list_all_jobs(unfiltered).await.unwrap_err();
    assert_eq!(status_code(&error), Code::InvalidArgument);

    let own = ListJobsRequest {
        page_token: first.next_page_token,
        ..build
    };
    let second = client.list_jobs(own).await.unwrap();
    let ids: Vec<&str> = second.jobs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, ["b"]);
    let held: Vec<String> = server.jobs().into_iter().map(|job| job.id).collect();
    assert_eq!(held, ["a", "b", "c"], "a refused listing changes no job");
}
