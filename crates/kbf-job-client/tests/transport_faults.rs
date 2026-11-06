//! How each way a call can break is reported: a connection lost, a deadline passed, a
//! handshake that never ends, answers that are not messages, and the status a fault gives.
//! Time is the tokio clock, moved by the test once the fake has taken the call, so the
//! outcome is decided by the durations alone.

mod support;

use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{
    CancelJobRequest, GetJobRequest, GetJobResponse, GetLogsRequest, GetResultRequest, Job,
    ListJobsRequest, ListJobsResponse, SubmitJobRequest,
};
use kbf_job_client::fake::{Call, FakeJobServer, Fault, TOKEN};
use kbf_job_client::{Broken, Client, Config, Error, Token, Undecodable, Unsent};
use prost::Message as _;
use support::{fake, pki, server_config, spec, status_code};
use tonic::Code;

/// Every call of the client.
const METHODS: [&str; 6] = [
    "SubmitJob",
    "CancelJob",
    "GetJob",
    "GetResult",
    "ListJobs",
    "GetLogs",
];

/// One call of `method` about the job `j`, its answer dropped.
async fn call(client: &Client, method: &str) -> Result<(), Error> {
    match method {
        "SubmitJob" => client.submit_job(spec("j")).await.map(|_| ()),
        "CancelJob" => client.cancel_job("j").await.map(|_| ()),
        "GetJob" => client.get_job("j").await.map(|_| ()),
        "GetResult" => client.get_result("j").await.map(|_| ()),
        "ListJobs" => client
            .list_jobs(ListJobsRequest::default())
            .await
            .map(|_| ()),
        "GetLogs" => client.get_logs("j").await.map(|_| ()),
        other => panic!("no method {other}"),
    }
}

/// The call of `method` as the fake logs it for [`call`].
fn logged(method: &str) -> Call {
    let job_id = "j".to_owned();
    match method {
        "SubmitJob" => Call::SubmitJob(Box::new(SubmitJobRequest {
            spec: Some(spec("j")),
        })),
        "CancelJob" => Call::CancelJob(CancelJobRequest { job_id }),
        "GetJob" => Call::GetJob(GetJobRequest { job_id }),
        "GetResult" => Call::GetResult(GetResultRequest { job_id }),
        "ListJobs" => Call::ListJobs(ListJobsRequest::default()),
        "GetLogs" => Call::GetLogs(GetLogsRequest { job_id }),
        other => panic!("no method {other}"),
    }
}

/// The field a `method` answer without it is missing, for the methods that answer a field.
fn missing_field(method: &str) -> Option<&'static str> {
    match method {
        "GetResult" => Some("result"),
        "SubmitJob" | "CancelJob" | "GetJob" => Some("job"),
        _ => None,
    }
}

/// A loopback port that is bound and never listens: a connection to it is refused, and no
/// other socket can take the port while the guard lives.
fn refusing_port() -> (tokio::net::TcpSocket, u16) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    (socket, port)
}

fn token() -> Token {
    Token::new(TOKEN).unwrap()
}

/// A client of the TLS fake `server` trusting `ca`.
fn pki_client(server: &FakeJobServer, ca: &str) -> Client {
    let config = server.config().unwrap().with_ca_pem(ca.as_bytes().to_vec());
    Client::new(config).unwrap()
}

/// The whole deadline of a default client.
fn deadline() -> Duration {
    Config::CONNECT_TIMEOUT + Config::REQUEST_TIMEOUT
}

/// The error of `method` on `client` when the fake takes the call and never answers it. The
/// clock is the real one until the fake has the request; then it is frozen and moved past
/// the whole deadline, and unfrozen again for the next call.
async fn hung_call(server: &FakeJobServer, client: &Client, method: &'static str) -> Error {
    server.inject_on(method, Fault::Hang);
    let task = {
        let client = client.clone();
        tokio::spawn(async move { call(&client, method).await })
    };
    server.hung().await;
    tokio::time::pause();
    tokio::time::advance(deadline()).await;
    let error = task.await.unwrap().unwrap_err();
    tokio::time::resume();
    error
}

/// Asserts that "nothing was sent" is told apart from "the server may have acted" for every
/// kind of failure, on each of the six calls: a connection dropped after the server applied
/// the call and a deadline that passed (the connect and request timeouts together,
/// `Broken::Deadline`) are `Transport`; an OK answer with bytes that do not decode is
/// `Decode`, and so is one without the job or result, for the four calls that answer one;
/// each has `may_have_applied()` true. A connection refused, a TLS handshake that fails, and
/// a bad job id are `Unreachable` and false; a definite status (`NOT_FOUND`,
/// `UNAUTHENTICATED`, `UNIMPLEMENTED`, a refusal) is false. Catches a status code classed
/// the wrong way, a lost connection taken for one never made, and a deadline taken for
/// nothing sent.
#[tokio::test]
async fn a_transport_error_is_marked_may_have_applied() {
    let (server, client) = fake().await;
    for method in METHODS {
        server.lose_next_answer();
        let lost = call(&client, method).await.unwrap_err();
        assert!(
            matches!(lost, Error::Transport(Broken::Status(_))),
            "{method}: {lost:?}"
        );

        let late = hung_call(&server, &client, method).await;
        assert!(
            matches!(late, Error::Transport(Broken::Deadline(after)) if after == deadline()),
            "{method}: {late:?}"
        );

        server.inject(Fault::Answer(Bytes::from_static(&[0x0f, 0x0f])));
        let garbled = call(&client, method).await.unwrap_err();
        assert!(
            matches!(garbled, Error::Decode(Undecodable::Bytes(_))),
            "{method}: {garbled:?}"
        );

        let mut broken = vec![lost, late, garbled];
        server.inject(Fault::Answer(Bytes::new()));
        match (missing_field(method), call(&client, method).await) {
            (Some(field), Err(empty)) => {
                assert!(
                    matches!(empty, Error::Decode(Undecodable::Missing(got)) if got == field),
                    "{method}: {empty:?}"
                );
                broken.push(empty);
            }
            (None, Ok(())) => {}
            (field, other) => panic!("{method}: {field:?} {other:?}"),
        }
        for error in broken {
            assert!(error.may_have_applied(), "{method}: {error:?}");
        }
    }

    let (_guard, port) = refusing_port();
    let origin = format!("http://127.0.0.1:{port}").parse().unwrap();
    let nowhere = Client::new(Config::new(origin, token())).unwrap();
    let pki = pki();
    let tls = FakeJobServer::start_tls(server_config(&pki)).await.unwrap();
    let stranger = pki_client(&tls, &support::pki().ca);
    for method in METHODS {
        for (what, unsent) in [
            ("refused", call(&nowhere, method).await.unwrap_err()),
            (
                "bad certificate",
                call(&stranger, method).await.unwrap_err(),
            ),
        ] {
            assert!(
                matches!(unsent, Error::Unreachable(Unsent::Connect(_))),
                "{method} {what}: {unsent:?}"
            );
            assert!(!unsent.may_have_applied(), "{method} {what}");
        }
    }
    let bad_id = client.cancel_job("no spaces").await.unwrap_err();
    assert!(matches!(bad_id, Error::Unreachable(Unsent::BadId(_))));
    assert!(!bad_id.may_have_applied());
    assert!(tls.calls().is_empty());

    let unknown = client.get_job("no-such-job").await.unwrap_err();
    assert_eq!(status_code(&unknown), Code::NotFound);
    let wrong = Token::new("not-the-fake-token-000000000000").unwrap();
    let outsider = Client::new(Config::new(server.origin().parse().unwrap(), wrong)).unwrap();
    let refused_token = outsider.get_job("j").await.unwrap_err();
    assert_eq!(status_code(&refused_token), Code::Unauthenticated);
    server.inject(Fault::Status {
        code: Code::Unimplemented,
        message: "no such method".to_owned(),
    });
    let older = client.get_job("j").await.unwrap_err();
    assert_eq!(status_code(&older), Code::Unimplemented);
    let mut no_wall = spec("no-wall");
    no_wall.wall_limit_seconds = None;
    let refusal = client.submit_job(no_wall).await.unwrap_err();
    assert!(refusal.refusal().is_some());
    for definite in [unknown, refused_token, older, refusal] {
        assert!(!definite.may_have_applied(), "{definite:?}");
    }
}

/// A call of `method` to a listener that takes the connection and never speaks, under a
/// client with the given timeouts and the right CA over `https`. The clock is real until the
/// connection is made and the client waits for the server's hello; then it is frozen, and
/// moved half the connect timeout (the call is still waiting), then to the connect timeout.
async fn stalled_call(method: &'static str, connect: Duration, request: Duration) -> Error {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let pki = pki();
    let config = Config::new(
        format!("https://localhost:{port}").parse().unwrap(),
        token(),
    )
    .with_ca_pem(pki.ca.into_bytes())
    .with_timeouts(connect, request);
    let client = Client::new(config).unwrap();
    let task = tokio::spawn(async move { call(&client, method).await });
    let (_held, _) = listener.accept().await.unwrap();
    tokio::time::pause();
    tokio::time::advance(connect / 2).await;
    assert!(!task.is_finished(), "{method}: before the connect timeout");
    tokio::time::advance(connect - connect / 2).await;
    let error = task.await.unwrap().unwrap_err();
    tokio::time::resume();
    error
}

/// Asserts that a TLS handshake that never completes is `Unreachable(Connect)`, nothing
/// sent and `may_have_applied()` false, on each of the six calls, whichever of the two
/// timeouts is longer: with a connect timeout of 10 s and a request timeout of 4 s (the
/// whole deadline is 14 s, so the connect timeout comes first), and with 2 s and 60 s. The
/// call is still pending before the connect timeout. Catches a deadline that is the request
/// timeout alone, which would class the stalled handshake as a transport error.
#[tokio::test]
async fn a_handshake_that_never_ends_is_unreachable_whichever_timeout_is_longer() {
    for (connect, request) in [(10, 4), (2, 60)] {
        let (connect, request) = (Duration::from_secs(connect), Duration::from_secs(request));
        for method in METHODS {
            let error = stalled_call(method, connect, request).await;
            assert!(
                matches!(error, Error::Unreachable(Unsent::Connect(_))),
                "{method} {connect:?}/{request:?}: {error:?}"
            );
            assert!(!error.may_have_applied(), "{method}");
        }
    }
}

/// The framing of one gRPC message: a flag byte (not compressed), the length in four
/// bytes, the bytes.
fn framed(message: &[u8]) -> Bytes {
    let mut frame = vec![0];
    frame.extend(u32::try_from(message.len()).unwrap().to_be_bytes());
    frame.extend(message);
    frame.into()
}

/// A well-formed answer of `method` (`GetJob` or `ListJobs`) holding the job `j`.
fn valid_body(method: &str) -> Bytes {
    let job = Some(Job {
        id: "j".to_owned(),
        ..Job::default()
    });
    match method {
        "GetJob" => framed(&GetJobResponse { job }.encode_to_vec()),
        "ListJobs" => framed(
            &ListJobsResponse {
                jobs: job.into_iter().collect(),
                next_page_token: String::new(),
            }
            .encode_to_vec(),
        ),
        other => panic!("no body for {other}"),
    }
}

/// Asserts the answers whose framing is wrong, on `GetJob` and `ListJobs`: a well-formed
/// message with status OK is a value (the control); an empty body with status OK, and a
/// frame that says 10 bytes and holds 3 with status OK, are `Transport` with `INTERNAL`
/// (tonic reports no message), not a value and not `Decode`; a well-formed message followed
/// by trailers of `INTERNAL` is `Transport` with `INTERNAL`, not the value it carried. Each
/// has `may_have_applied()` true. Catches a status in the trailers ignored once a message
/// came, and an answer without a message taken for a default value.
#[tokio::test]
async fn an_answer_with_broken_framing_is_a_transport_error() {
    let (server, client) = fake().await;
    for method in ["GetJob", "ListJobs"] {
        server.inject_on(
            method,
            Fault::Raw {
                body: valid_body(method),
                code: Code::Ok,
            },
        );
        call(&client, method)
            .await
            .unwrap_or_else(|e| panic!("{method}: a well-formed answer: {e:?}"));

        let truncated: Vec<u8> = [&[0, 0, 0, 0, 10][..], &[1, 2, 3][..]].concat();
        let cases = [
            ("empty body", Bytes::new(), Code::Ok),
            ("truncated frame", Bytes::from(truncated), Code::Ok),
            ("message then INTERNAL", valid_body(method), Code::Internal),
        ];
        for (what, body, code) in cases {
            server.inject_on(method, Fault::Raw { body, code });
            let error = call(&client, method).await.unwrap_err();
            match &error {
                Error::Transport(Broken::Status(status)) => {
                    assert_eq!(status.code(), Code::Internal, "{method} {what}");
                }
                other => panic!("{method} {what}: {other:?}"),
            }
            assert!(error.may_have_applied(), "{method} {what}");
        }
    }
}

/// Asserts that a `Status` fault, on each of the six methods, answers the status as a
/// definite error, logs the decoded request in the fake's calls in arrival order, and
/// changes no job. Catches a fake that decodes the request of some method wrong or not at
/// all, and a fault that applies the call it answers.
#[tokio::test]
async fn a_status_fault_logs_the_request_of_every_method_and_applies_nothing() {
    let (server, client) = fake().await;
    server.record(spec("kept")).unwrap();
    let before = server.jobs();
    for (n, method) in METHODS.into_iter().enumerate() {
        server.inject_on(
            method,
            Fault::Status {
                code: Code::FailedPrecondition,
                message: "held".to_owned(),
            },
        );
        let error = call(&client, method).await.unwrap_err();
        assert_eq!(status_code(&error), Code::FailedPrecondition, "{method}");
        let calls = server.calls();
        assert_eq!(calls.len(), n + 1, "{method}");
        assert_eq!(calls[n], logged(method), "{method}");
        assert_eq!(server.jobs(), before, "{method}");
    }
}
