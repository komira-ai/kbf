//! The client's side of the wire: whom the client trusts and how it speaks TLS, the token,
//! the size caps, the job id check, and calls in flight together. The ways a call can break
//! are in `transport_faults.rs`.

mod support;

use std::time::Duration;

use js_proto::v1::{
    GetLogsResponse, JobSpec, ListJobsRequest, NetworkLevel, RefusalCode, SubmitJobRequest,
};
use kbf_job_client::fake::{Call, FakeJobServer, Fault, TOKEN};
use kbf_job_client::{BadId, Broken, Client, Config, Error, Token, Unsent};
use prost::Message as _;
use support::{fake, pki, server_config, server_config_with, spec, status_code};
use tonic::Code;

/// The methods whose request names a job by id.
const BY_ID: [&str; 5] = ["SubmitJob", "CancelJob", "GetJob", "GetResult", "GetLogs"];

/// One call of `method` about the job `id`, its answer dropped.
async fn call_about(client: &Client, method: &str, id: &str) -> Result<(), Error> {
    match method {
        "SubmitJob" => client.submit_job(spec(id)).await.map(|_| ()),
        "CancelJob" => client.cancel_job(id).await.map(|_| ()),
        "GetJob" => client.get_job(id).await.map(|_| ()),
        "GetResult" => client.get_result(id).await.map(|_| ()),
        "GetLogs" => client.get_logs(id).await.map(|_| ()),
        other => panic!("no method {other} that takes an id"),
    }
}

/// One call of `method` about the job `j`, its answer dropped.
async fn call(client: &Client, method: &str) -> Result<(), Error> {
    match method {
        "ListJobs" => client
            .list_jobs(ListJobsRequest::default())
            .await
            .map(|_| ()),
        other => call_about(client, other, "j").await,
    }
}

fn token() -> Token {
    Token::new(TOKEN).unwrap()
}

/// A client of the TLS fake `server` trusting `ca`.
fn pki_client(server: &FakeJobServer, ca: &str, connect: Duration) -> Client {
    let config = server.config().unwrap().with_ca_pem(ca.as_bytes().to_vec());
    Client::new(config.with_timeouts(connect, Config::REQUEST_TIMEOUT)).unwrap()
}

/// Asserts the token: each of the six calls succeeds with the fake's token, which the fake
/// requires on every call; with any other token each of the six is `Status{UNAUTHENTICATED}`
/// and the fake handles none of them. Asserts what prints: the `Debug` of the token, the
/// config and the client are exactly the texts the documentation gives; the errors of a
/// client with one wrong token and of one with another of the same length print the same
/// `Debug` and `Display` (a token that printed would tell them apart), and so is the call
/// log, which equals the requests as written. Catches a method that forgets the metadata
/// and a type, an error or a log that prints the token.
#[tokio::test]
async fn the_token_is_sent_on_every_call_and_never_printed() {
    let (server, client) = fake().await;
    client.submit_job(spec("j")).await.unwrap();
    client.cancel_job("j").await.unwrap();
    for method in [
        "GetJob",
        "ListJobs",
        "GetResult",
        "GetLogs",
        "CancelJob",
        "SubmitJob",
    ] {
        call(&client, method)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:?}"));
    }
    let handled = server.calls().len();

    let authority = server.origin().strip_prefix("http://").unwrap().to_owned();
    assert_eq!(format!("{:?}", token()), "Token(..)");
    let with_ca = server.config().unwrap().with_ca_pem(Vec::new());
    assert_eq!(
        format!("{with_ca:?}"),
        format!(
            "Config {{ origin: Origin(\"http://{authority}/\"), token: Token(..), ca: true, \
             connect_timeout: 5s, request_timeout: 30s }}"
        )
    );
    assert_eq!(
        format!(
            "{:?}",
            server
                .config()
                .unwrap()
                .with_timeouts(Duration::from_secs(1), Duration::from_millis(1500))
        ),
        format!(
            "Config {{ origin: Origin(\"http://{authority}/\"), token: Token(..), ca: false, \
             connect_timeout: 1s, request_timeout: 1.5s }}"
        )
    );
    assert_eq!(
        format!("{client:?}"),
        format!("Client {{ origin: \"{authority}\", tls: false, .. }}")
    );

    let outsider = |secret: &str| {
        let wrong = Token::new(secret).unwrap();
        Client::new(Config::new(server.origin().parse().unwrap(), wrong)).unwrap()
    };
    let first = outsider("not-the-fake-token-000000000000");
    let second = outsider("not-the-fake-token-111111111111");
    for method in [
        "SubmitJob",
        "GetJob",
        "ListJobs",
        "CancelJob",
        "GetResult",
        "GetLogs",
    ] {
        let one = call(&first, method).await.unwrap_err();
        let other = call(&second, method).await.unwrap_err();
        assert_eq!(status_code(&one), Code::Unauthenticated, "{method}");
        assert_eq!(format!("{one:?}"), format!("{other:?}"), "{method}");
        assert_eq!(one.to_string(), other.to_string(), "{method}");
        assert_eq!(
            format!("{one:?}"),
            "Status { code: Unauthenticated, message: \"the bearer token is missing or wrong\" }"
        );
        assert_eq!(
            one.to_string(),
            "the job service answered Unauthenticated: the bearer token is missing or wrong"
        );
    }
    assert_eq!(
        format!("{first:?}"),
        format!("{second:?}"),
        "a client prints no token"
    );
    assert_eq!(
        server.calls().len(),
        handled,
        "a refused token reaches no handler"
    );

    let (logged, logging) = fake().await;
    logging.get_job("j").await.unwrap_err();
    assert_eq!(
        format!("{:?}", logged.calls()),
        "[GetJob(GetJobRequest { job_id: \"j\" })]"
    );
}

/// Asserts the TLS contract: with the CA that signed the server's certificate the client
/// calls an `https` fake; with another CA, and with the right CA and an origin whose name
/// (`127.0.0.1`) the certificate does not carry, every call is `Unreachable` and the server
/// handles nothing. Proves no system root is consulted and the name is checked. Catches a
/// client that falls back to platform roots or skips name verification.
#[tokio::test]
async fn only_the_pinned_ca_and_the_right_name_are_trusted() {
    let pki = pki();
    let server = FakeJobServer::start_tls(server_config(&pki)).await.unwrap();
    server.add_host(support::HOST, support::machine());
    let trusting = pki_client(&server, &pki.ca, Config::CONNECT_TIMEOUT);
    trusting.submit_job(spec("over-tls")).await.unwrap();
    assert_eq!(trusting.get_job("over-tls").await.unwrap().id, "over-tls");
    assert_eq!(server.calls().len(), 2);

    let stranger = pki_client(&server, &support::pki().ca, Config::CONNECT_TIMEOUT);
    let error = stranger.get_job("over-tls").await.unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );

    let by_address = server.origin().replace("localhost", "127.0.0.1");
    let config =
        Config::new(by_address.parse().unwrap(), token()).with_ca_pem(pki.ca.as_bytes().to_vec());
    let misnamed = Client::new(config).unwrap();
    let error = misnamed.get_job("over-tls").await.unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );
    assert_eq!(
        server.calls().len(),
        2,
        "no call through an untrusted session"
    );
}

/// Asserts the versions and the client identity: a fake that speaks TLS 1.2 only and one
/// that speaks TLS 1.3 only each take a call of the client; a fake of TLS 1.2 that demands
/// a client certificate makes the call `Unreachable` (the handshake fails, nothing sent)
/// because the client presents none, and the server handles nothing. (Under TLS 1.2 the
/// server's verdict on the client's identity comes inside the handshake, so the outcome is
/// fixed.) Catches a client restricted to one version, and one that presents an identity it
/// was not given.
#[tokio::test]
async fn the_client_speaks_tls_1_2_and_1_3_and_presents_no_certificate() {
    let pki = pki();
    for (name, version) in [
        ("1.2", &rustls::version::TLS12),
        ("1.3", &rustls::version::TLS13),
    ] {
        let server = FakeJobServer::start_tls(server_config_with(&pki, &[version], false))
            .await
            .unwrap();
        server.add_host(support::HOST, support::machine());
        let client = pki_client(&server, &pki.ca, Config::CONNECT_TIMEOUT);
        let job = client.submit_job(spec("over-tls")).await.unwrap();
        assert_eq!(job.id, "over-tls", "TLS {name}");
        assert_eq!(server.calls().len(), 1, "TLS {name}");
    }

    let strict =
        FakeJobServer::start_tls(server_config_with(&pki, &[&rustls::version::TLS12], true))
            .await
            .unwrap();
    let client = pki_client(&strict, &pki.ca, Config::CONNECT_TIMEOUT);
    let error = client.get_job("j").await.unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );
    assert!(strict.calls().is_empty());
}

/// Asserts the answer cap at its edge: a `get_logs` answer of exactly 4 MiB is read whole;
/// one of 4 MiB + 1 byte is `Error::Transport` with the client's `OUT_OF_RANGE` (the server
/// answered, so `may_have_applied()` is true), not a hang and not a partial value. Catches
/// the limit left at another value and an answer over it accepted.
#[tokio::test]
async fn a_response_over_the_cap_is_an_error() {
    let (server, client) = fake().await;
    let cap = Client::MAX_MESSAGE;
    assert_eq!(cap, 4 << 20);
    let logs = |len: usize| GetLogsResponse {
        stdout_tail: vec![b'x'; len - 5],
        ..GetLogsResponse::default()
    };
    let at_cap = logs(cap);
    assert_eq!(at_cap.encoded_len(), cap);
    server.inject(Fault::Answer(at_cap.encode_to_vec().into()));
    let read = client.get_logs("j").await.unwrap();
    assert_eq!(read.stdout_tail.len(), cap - 5);

    let over = logs(cap + 1);
    assert_eq!(over.encoded_len(), cap + 1);
    server.inject(Fault::Answer(over.encode_to_vec().into()));
    let error = client.get_logs("j").await.unwrap_err();
    match &error {
        Error::Transport(Broken::Status(status)) => assert_eq!(status.code(), Code::OutOfRange),
        other => panic!("not a transport error: {other:?}"),
    }
    assert!(error.may_have_applied());
}

/// A job spec whose `SubmitJob` request encodes to exactly `len` bytes, its padding in one
/// argument.
fn spec_encoding_to(len: usize) -> JobSpec {
    let encoded = |padding: usize| {
        let mut padded = spec("padded");
        padded.args = vec!["a".repeat(padding)];
        let request = SubmitJobRequest {
            spec: Some(padded.clone()),
        };
        (padded, request.encoded_len())
    };
    let mut padding = 0;
    for _ in 0..8 {
        let (padded, got) = encoded(padding);
        if got == len {
            return padded;
        }
        padding = (padding + len).saturating_sub(got);
    }
    panic!("no padding gives a request of {len} bytes");
}

/// Asserts the request cap at its edge: a `SubmitJob` request of exactly 4 MiB is sent (the
/// fake logs it as written; what the call answers is not the point), and one of 4 MiB + 1
/// byte is `Unreachable(TooLarge{len: cap + 1, cap})` and the fake logs nothing for it.
/// Catches a cap off by one in either direction and an oversized request sent to be refused.
#[tokio::test]
async fn a_request_over_the_cap_is_not_sent() {
    let (server, client) = fake().await;
    let cap = Client::MAX_MESSAGE;
    let at_cap = spec_encoding_to(cap);
    let _ = client.submit_job(at_cap.clone()).await;
    assert_eq!(
        server.calls(),
        vec![Call::SubmitJob(Box::new(SubmitJobRequest {
            spec: Some(at_cap)
        }))],
        "a request of exactly the cap reaches the service"
    );

    let over = spec_encoding_to(cap + 1);
    let error = client.submit_job(over).await.unwrap_err();
    assert!(
        matches!(&error, Error::Unreachable(Unsent::TooLarge { len, cap: got })
            if *len == cap + 1 && *got == cap),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert_eq!(server.calls().len(), 1, "nothing was sent for the larger");
}

/// Asserts the id check on every call that takes an id: for each of `""`, 64 bytes, `a/b`
/// and `a b`, `submit_job` (the spec's id), `get_job`, `cancel_job`, `get_result` and
/// `get_logs` answer `Unreachable(BadId)` with the exact reason (length 0, length 64, the
/// character `/`, the character space), and the fake logs no call. Catches a method that
/// skips the check, and a check that reports another reason.
#[tokio::test]
async fn a_bad_job_id_is_not_sent_by_any_method() {
    let (server, client) = fake().await;
    let long = "a".repeat(64);
    let bad: [(&str, BadId); 4] = [
        ("", BadId::Length(0)),
        (long.as_str(), BadId::Length(64)),
        ("a/b", BadId::Character('/')),
        ("a b", BadId::Character(' ')),
    ];
    for (id, reason) in bad {
        for method in BY_ID {
            let error = call_about(&client, method, id).await.unwrap_err();
            assert!(
                matches!(&error, Error::Unreachable(Unsent::BadId(got)) if *got == reason),
                "{method} {id:?}: {error:?}"
            );
            assert!(!error.may_have_applied(), "{method} {id:?}");
        }
    }
    assert!(server.calls().is_empty());
}

/// Asserts that sixteen clones of one client submit sixteen distinct jobs at once on a
/// runtime of several threads, all succeed, and the fake holds exactly those sixteen jobs.
/// Catches clones that share one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_call_concurrently() {
    let (server, client) = fake().await;
    let calls: Vec<_> = (0..16)
        .map(|n| {
            let client = client.clone();
            tokio::spawn(async move { client.submit_job(spec(&format!("par-{n}"))).await })
        })
        .collect();
    for handle in calls {
        handle.await.unwrap().unwrap();
    }
    let mut ids: Vec<String> = server.jobs().into_iter().map(|job| job.id).collect();
    ids.sort();
    let mut expected: Vec<String> = (0..16).map(|n| format!("par-{n}")).collect();
    expected.sort();
    assert_eq!(ids, expected);
}

/// Asserts that calls overlap: a `GetJob` that the fake has taken and never answers is in
/// flight, and another clone of the client completes a `SubmitJob` while it is still
/// pending; then the first is aborted. Catches a client that serialises calls on a lock
/// held across an await.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_in_flight_does_not_hold_up_another_clone() {
    let (server, client) = fake().await;
    server.inject_on("GetJob", Fault::Hang);
    let stuck = {
        let client = client.clone();
        tokio::spawn(async move { client.get_job("j").await })
    };
    server.hung().await;
    let other = client.clone();
    let job = other.submit_job(spec("while-hung")).await.unwrap();
    assert_eq!(job.id, "while-hung");
    assert!(!stuck.is_finished(), "the first call is still in flight");
    stuck.abort();
    assert!(stuck.await.unwrap_err().is_cancelled());
}

/// Asserts the submits of one id made at once: eight identical specs all answer the same
/// job and the fake holds one job; eight submits alternating two specs that share an id
/// leave exactly one job, and each answer is either that job (when its spec is the one
/// recorded) or `job-id-conflict` (when it is the other). Catches an idempotency that
/// records twice, a conflict taken for a success, and an answer that disagrees with what
/// was recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submits_of_one_id_at_once_agree_with_what_was_recorded() {
    let (server, client) = fake().await;
    let same: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.submit_job(spec("same")).await })
        })
        .collect();
    let mut answers = Vec::new();
    for handle in same {
        answers.push(handle.await.unwrap().unwrap());
    }
    let jobs = server.jobs();
    assert_eq!(jobs.len(), 1);
    assert!(answers.iter().all(|job| *job == jobs[0]), "{answers:?}");

    // Both name the network level `none`, which the fake records an unspecified one as, so
    // the spec it recorded compares equal to the one that won.
    let mut first = spec("contested");
    first.set_network(NetworkLevel::None);
    let mut other = first.clone();
    other.command = "/bin/other".to_owned();
    let specs = [first, other];
    let racing: Vec<_> = (0..8)
        .map(|n| {
            let client = client.clone();
            let spec = specs[n % 2].clone();
            tokio::spawn(async move { (n % 2, client.submit_job(spec).await) })
        })
        .collect();
    let mut outcomes = Vec::new();
    for handle in racing {
        outcomes.push(handle.await.unwrap());
    }
    let recorded = server.submitted("contested").unwrap();
    let contested: Vec<_> = server
        .jobs()
        .into_iter()
        .filter(|job| job.id == "contested")
        .collect();
    assert_eq!(contested.len(), 1);
    for (which, outcome) in outcomes {
        match outcome {
            Ok(job) => {
                assert_eq!(job, contested[0]);
                assert_eq!(
                    recorded, specs[which],
                    "an Ok answer is of the recorded spec"
                );
            }
            Err(error) => {
                assert!(error.is(RefusalCode::JobIdConflict), "{error:?}");
                assert_ne!(
                    recorded, specs[which],
                    "a refused spec is not the recorded one"
                );
            }
        }
    }
}

/// Asserts that a fault aimed at one method waits for that method: a `SubmitJob` is served
/// as usual while a fault for `GetJob` is queued, and the next `GetJob` takes it. Catches a
/// fault that breaks whichever call comes first.
#[tokio::test]
async fn a_fault_for_one_method_waits_for_it() {
    let (server, client) = fake().await;
    server.inject_on(
        "GetJob",
        Fault::Status {
            code: Code::PermissionDenied,
            message: "not yours".to_owned(),
        },
    );
    client.submit_job(spec("j")).await.unwrap();
    let error = client.get_job("j").await.unwrap_err();
    assert_eq!(status_code(&error), Code::PermissionDenied);
    assert_eq!(client.get_job("j").await.unwrap().id, "j");
    assert!(format!("{:?}", Fault::LoseAnswer).contains("LoseAnswer"));
}
