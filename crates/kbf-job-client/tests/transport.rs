//! The client's side of the wire: what each way a call can break is reported as, whom the
//! client trusts, the token, the size caps, and calls in flight together.

mod support;

use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{GetLogsResponse, ListJobsRequest};
use kbf_job_client::fake::{FakeJobServer, Fault, TOKEN};
use kbf_job_client::{Broken, Client, Config, Error, Token, Undecodable, Unsent};
use prost::Message as _;
use support::{fake, pki, server_config, spec, status_code};
use tonic::Code;

/// A request timeout the fake's [`Fault::Hang`] is sure to outlast: it never answers.
const SHORT: Duration = Duration::from_millis(300);

/// The four calls whose errors a caller acts on.
const METHODS: [&str; 4] = ["SubmitJob", "CancelJob", "GetJob", "GetResult"];

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

/// Asserts that "nothing was sent" is told apart from "the server may have acted" for every
/// kind of failure, on each of `submit_job`, `cancel_job`, `get_job` and `get_result`: a
/// connection dropped after the server applied the call and a deadline that passed are
/// `Transport`, an OK answer with bytes that do not decode and one without the job or
/// result are `Decode`, and each has `may_have_applied()` true; a connection refused, a TLS
/// handshake that fails or never completes, and a bad job id are `Unreachable` and false;
/// a definite status (`NOT_FOUND`, `UNAUTHENTICATED`, `UNIMPLEMENTED`, a refusal) is false.
/// Catches a status code classed the wrong way, a lost connection taken for one never made,
/// and a deadline taken for nothing sent.
#[tokio::test]
async fn a_transport_error_is_marked_may_have_applied() {
    let (server, client) = fake().await;
    let impatient = support::client_with_deadline(&server, SHORT);
    for method in METHODS {
        server.lose_next_answer();
        let lost = call(&client, method).await.unwrap_err();
        assert!(
            matches!(lost, Error::Transport(Broken::Status(_))),
            "{method}: {lost:?}"
        );

        server.inject(Fault::Hang);
        let late = call(&impatient, method).await.unwrap_err();
        assert!(
            matches!(late, Error::Transport(Broken::Deadline(after)) if after == SHORT),
            "{method}: {late:?}"
        );

        server.inject(Fault::Answer(Bytes::from_static(&[0x0f, 0x0f])));
        let garbled = call(&client, method).await.unwrap_err();
        assert!(
            matches!(garbled, Error::Decode(Undecodable::Bytes(_))),
            "{method}: {garbled:?}"
        );

        server.inject(Fault::Answer(Bytes::new()));
        let empty = call(&client, method).await.unwrap_err();
        let missing = if method == "GetResult" {
            "result"
        } else {
            "job"
        };
        assert!(
            matches!(empty, Error::Decode(Undecodable::Missing(field)) if field == missing),
            "{method}: {empty:?}"
        );

        for error in [lost, late, garbled, empty] {
            assert!(error.may_have_applied(), "{method}: {error:?}");
        }
    }

    let (_guard, port) = refusing_port();
    let origin = format!("http://127.0.0.1:{port}").parse().unwrap();
    let nowhere = Client::new(Config::new(origin, token())).unwrap();
    let pki = pki();
    let tls = FakeJobServer::start_tls(server_config(&pki)).await.unwrap();
    let stranger = pki_client(&tls, &support::pki().ca, Config::CONNECT_TIMEOUT);
    let (_silent, silent_origin) = silent_tls_listener().await;
    let stalled = Client::new(
        Config::new(silent_origin.parse().unwrap(), token())
            .with_ca_pem(pki.ca.as_bytes().to_vec())
            .with_timeouts(SHORT, Config::REQUEST_TIMEOUT),
    )
    .unwrap();
    for method in METHODS {
        for (what, unsent) in [
            ("refused", call(&nowhere, method).await.unwrap_err()),
            (
                "bad certificate",
                call(&stranger, method).await.unwrap_err(),
            ),
            (
                "handshake never ends",
                call(&stalled, method).await.unwrap_err(),
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

/// A listener on loopback that takes connections and never answers them: a TLS handshake
/// with it never completes. The origin names `localhost`.
async fn silent_tls_listener() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, format!("https://localhost:{port}"))
}

/// A client of the TLS fake `server` trusting `ca`.
fn pki_client(server: &FakeJobServer, ca: &str, connect: Duration) -> Client {
    let config = server.config().unwrap().with_ca_pem(ca.as_bytes().to_vec());
    Client::new(config.with_timeouts(connect, Config::REQUEST_TIMEOUT)).unwrap()
}

/// Asserts the token: each of the six calls succeeds with the fake's token, which the fake
/// requires on every call; with any other token each of the six is `Status{UNAUTHENTICATED}`
/// and the fake handles none of them; and the `Debug` of the token, the config, the client,
/// every error and the call log, and each error's text, hold neither token. Catches a method
/// that forgets the metadata and an error or log that prints the request.
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

    let wrong = Token::new("not-the-fake-token-000000000000").unwrap();
    let config = Config::new(server.origin().parse().unwrap(), wrong);
    let mut printed = format!("{config:?}");
    let outsider = Client::new(config).unwrap();
    for method in [
        "SubmitJob",
        "GetJob",
        "ListJobs",
        "CancelJob",
        "GetResult",
        "GetLogs",
    ] {
        let error = call(&outsider, method).await.unwrap_err();
        assert_eq!(status_code(&error), Code::Unauthenticated, "{method}");
        printed.push_str(&format!(" {error:?} {error}"));
    }
    assert_eq!(
        server.calls().len(),
        handled,
        "a refused token reaches no handler"
    );

    let tokens = format!(
        "{:?} {:?} {client:?} {outsider:?} {:?}",
        token(),
        server.config().unwrap(),
        server.calls()
    );
    printed.push_str(&tokens);
    assert!(!printed.contains(TOKEN), "{printed}");
    assert!(!printed.contains("not-the-fake-token"), "{printed}");
    assert!(printed.contains("Token(..)"));
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

/// Asserts both size caps at their edge: a `get_logs` answer of exactly 4 MiB is read
/// whole; one of 4 MiB + 1 byte is `Error::Transport` with the client's `OUT_OF_RANGE` (the
/// server answered, so `may_have_applied()` is true), not a hang and not a partial value; a
/// request over 4 MiB is `Unreachable` and is not sent. Catches the limit left at another
/// value, an answer over it accepted, and a request over it sent to be refused.
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

    let before = server.calls().len();
    let mut huge = spec("huge");
    huge.args = vec!["a".repeat(cap)];
    let error = client.submit_job(huge).await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::Unreachable(Unsent::TooLarge { cap: 4_194_304, .. })
        ),
        "{error:?}"
    );
    assert_eq!(server.calls().len(), before, "nothing was sent");
}

/// Asserts that sixteen clones of one client submit sixteen distinct jobs at once on a
/// runtime of several threads, all succeed, and the fake holds sixteen jobs. Catches a
/// client that serialises calls on a lock held across an await, or clones that share one
/// request.
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
