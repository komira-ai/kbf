//! A job's completion callback through the client and the fake: the key is sent, kept for
//! the test to sign with, and never answered; the delivery status a read shows; and the
//! service's checks of a callback at submit.

mod support;

use js_proto::v1::{Callback, CallbackState, JobSpec, ListJobsRequest, RefusalCode};
use kbf_job_client::Error;
use support::{fake, spec, status_code, to_running};
use tonic::Code;

/// The 32-byte key the tests submit: the bytes 0xa0 to 0xbf.
fn key() -> Vec<u8> {
    (0xa0..0xc0).collect()
}

/// [`spec`] of `id` with a callback to `url`, signed with `key`, and a deadline of 300 s.
fn called_back(id: &str, url: &str, key: Vec<u8>) -> JobSpec {
    JobSpec {
        callback: Some(Callback {
            url: url.to_owned(),
            signing_key: key,
            deadline_seconds: Some(300),
        }),
        ..spec(id)
    }
}

const URL: &str = "https://kbf.example/js-done/ref-1";

/// The callback a read answers: the URL and deadline submitted, and no key.
fn answered() -> Option<Callback> {
    Some(Callback {
        url: URL.to_owned(),
        signing_key: Vec::new(),
        deadline_seconds: Some(300),
    })
}

/// Asserts that the key of a job's callback never comes back while the fake keeps it: the
/// submit's answer, `GetJob`, `ListJobs` and the fake's own list of jobs carry the callback
/// with its URL and deadline and an empty key, while `submitted` holds the key as sent. The
/// status is `ARMED` while the job is queued and running and `PENDING` once it has ended; a
/// job with no callback has no status and no callback. Catches a fake that echoes the key (so
/// no test of kbf-server would see a key leak through a read), and a status that does not
/// follow the job.
#[tokio::test]
async fn a_callback_is_answered_without_its_key_and_with_its_status() {
    let (server, client) = fake().await;
    let job = client
        .submit_job(called_back("cb-1", URL, key()))
        .await
        .unwrap();
    assert_eq!(job.spec.unwrap().callback, answered());
    let state = job.callback_status.map(|status| status.state());
    assert_eq!(state, Some(CallbackState::Armed));
    assert_eq!(
        server
            .submitted("cb-1")
            .unwrap()
            .callback
            .unwrap()
            .signing_key,
        key()
    );

    to_running(&server, "cb-1");
    let read = client.get_job("cb-1").await.unwrap();
    assert_eq!(read.spec.unwrap().callback, answered());
    let state = read.callback_status.map(|status| status.state());
    assert_eq!(state, Some(CallbackState::Armed));

    assert!(server.finish("cb-1", 0));
    let read = client.get_job("cb-1").await.unwrap();
    let state = read.callback_status.as_ref().map(|status| status.state());
    assert_eq!(state, Some(CallbackState::Pending));
    let listed = client
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert_eq!(listed, [read.clone()]);
    assert_eq!(server.jobs(), [read]);

    let plain = client.submit_job(spec("plain")).await.unwrap();
    assert_eq!(plain.spec.unwrap().callback, None);
    assert_eq!(plain.callback_status, None);
}

/// Asserts that the callback is part of what a job is, as the service compares it: the same
/// callback under the same id is the same job, and another key or another URL is
/// `job-id-conflict`. Catches a fake that leaves the callback out of the comparison, so a
/// kbf-server that minted a new key on a resend would pass against it and fail against the
/// service.
#[tokio::test]
async fn a_resubmit_with_another_callback_key_or_url_conflicts() {
    let (_server, client) = fake().await;
    let first = client
        .submit_job(called_back("cb-2", URL, key()))
        .await
        .unwrap();
    let again = client
        .submit_job(called_back("cb-2", URL, key()))
        .await
        .unwrap();
    assert_eq!(again, first);
    let other_key: Vec<u8> = (0..32).collect();
    for other in [
        called_back("cb-2", URL, other_key),
        called_back("cb-2", "https://kbf.example/js-done/ref-2", key()),
    ] {
        let error = client.submit_job(other).await.unwrap_err();
        assert!(error.is(RefusalCode::JobIdConflict), "{error:?}");
    }
}

/// Asserts the service's checks of a callback that the fake keeps, at their bounds: a key of
/// 31 or 65 bytes, or empty, and a URL that is plain `http`, has no scheme or is shorter than
/// a scheme, are each `INVALID_ARGUMENT` with exactly the service's text, which holds neither
/// the URL nor the key, and record no job; a key of 32 and of 64 bytes, and an `HTTPS` scheme
/// in capitals, are taken. Catches a fake that takes a callback the service refuses (so a
/// kbf-server test would pass on a job that never runs), and an error text that repeats the
/// key.
#[tokio::test]
async fn a_bad_callback_is_refused_with_the_services_text() {
    let (server, client) = fake().await;
    let bad_length = "field: spec.callback.signingKey: bad-length";
    let not_https = "field: spec.callback.url: not-https";
    let refused = [
        (called_back("b1", URL, vec![7; 31]), bad_length),
        (called_back("b2", URL, vec![7; 65]), bad_length),
        (called_back("b3", URL, Vec::new()), bad_length),
        (called_back("b4", "http://kbf.example/r", key()), not_https),
        (called_back("b5", "kbf.example/r", key()), not_https),
        (called_back("b6", "https:", key()), not_https),
    ];
    for (asked, text) in refused {
        let error = client.submit_job(asked).await.unwrap_err();
        assert_eq!(status_code(&error), Code::InvalidArgument);
        let Error::Status { message, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(message, text);
    }
    assert!(server.jobs().is_empty());

    for (id, url, key) in [
        ("g1", URL, vec![7; 32]),
        ("g2", URL, vec![7; 64]),
        ("g3", "HTTPS://kbf.example/r", key()),
    ] {
        let job = client.submit_job(called_back(id, url, key)).await.unwrap();
        assert_eq!(job.id, id);
    }
}
