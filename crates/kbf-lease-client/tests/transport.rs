//! The client's side of the wire: which routes it calls, whom it trusts, and what each way a
//! call can break is reported as.

mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_lease_client::fake::{FakeLeaseServer, Fault, TOKEN};
use kbf_lease_client::{Client, Config, ConfigError, Error, ErrorCode, Token};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use support::{fake, lease, pool, refused, run_of, whole_host};

const SHORT: Duration = Duration::from_millis(150);

/// A CA and a certificate for `localhost` under it.
struct Pki {
    ca: String,
    chain: String,
    key: String,
}

fn pki() -> Pki {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    Pki {
        ca: ca.pem(),
        chain: leaf.pem(),
        key: key.serialize_pem(),
    }
}

fn server_config(pki: &Pki) -> Arc<rustls::ServerConfig> {
    let chain: Vec<_> = CertificateDer::pem_slice_iter(pki.chain.as_bytes())
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_slice(pki.key.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    Arc::new(config)
}

async fn tls_server() -> (FakeLeaseServer, Pki) {
    let pki = pki();
    let server = FakeLeaseServer::start_tls(pool(), server_config(&pki)).await;
    (server, pki)
}

fn client_trusting(server: &FakeLeaseServer, ca: &str) -> Client {
    Client::new(server.config().with_ca_pem(ca.as_bytes().to_vec())).unwrap()
}

/// Asserts that each client method calls the one route the server documents for it, once, and
/// that a fresh server has seen no request at all before the first call. It catches a method
/// wired to the wrong verb or path, a client that retries on its own (a queued run would run
/// twice), and a client that contacts the server when it is only built.
#[tokio::test]
async fn each_method_makes_one_call_to_its_route() {
    let (server, client) = fake().await;
    assert!(
        server.requests().is_empty(),
        "building a client calls nothing"
    );
    let id = lease(&client).await;
    client.renew(id, None).await.unwrap();
    client.leases().await.unwrap();
    client.hosts().await.unwrap();
    let run = client.queue_run(id, &run_of(1000, 1)).await.unwrap().run_id;
    client.run(id, &run).await.unwrap();
    client.runs(id).await.unwrap();
    client.stop_run(id, &run).await.unwrap();
    client.release(id).await.unwrap();
    assert_eq!(
        server.requests(),
        [
            "POST /v1/leases",
            "POST /v1/leases/4-1/renew",
            "GET /v1/leases",
            "GET /v1/hosts",
            "POST /v1/leases/4-1/runs",
            "GET /v1/leases/4-1/runs/run-4-1",
            "GET /v1/leases/4-1/runs",
            "DELETE /v1/leases/4-1/runs/run-4-1",
            "DELETE /v1/leases/4-1",
        ]
    );
}

/// Asserts that a wrong token is a `401` refusal with no code (the server's body is empty),
/// that the right one is accepted, and that a token and a client never print the secret.
/// It catches a client that reads the 401 as a transport error (a retry loop on a bad
/// credential) and a `Debug` that leaks the token into a log.
#[tokio::test]
async fn a_wrong_token_is_a_401_and_no_token_is_ever_printed() {
    let (server, _) = fake().await;
    let wrong = Token::new("not-the-fake-lease-server-token-000000").unwrap();
    let config = Config::new(server.origin().parse().unwrap(), wrong);
    let shown = format!("{config:?}");
    let client = Client::new(config).unwrap();
    let error = client.leases().await.unwrap_err();
    let api = error.api().unwrap();
    assert_eq!((api.status, &api.code), (401, &ErrorCode::Unspecified));
    assert_eq!(error.to_string(), "the lease server answered 401 -");
    let printed = format!("{shown} {client:?} {error:?}");
    assert!(!printed.contains("not-the-fake"), "{printed}");
    assert!(!printed.contains(TOKEN), "{printed}");
    assert!(printed.contains("Token(..)"));
    server.client().unwrap().leases().await.unwrap();
}

/// Asserts the TLS contract: with the pinned CA the client talks to an `https` server; with a
/// different CA the handshake fails and the error is `Unreachable` (nothing was sent, so
/// nothing can have been applied, and the server saw no request); and a certificate for
/// another name is refused too. It catches a client that trusts any certificate or the
/// system roots, and one that skips the name check.
#[tokio::test]
async fn only_the_pinned_ca_and_the_right_name_are_trusted() {
    let (server, pki) = tls_server().await;
    let client = client_trusting(&server, &pki.ca);
    client.acquire(&whole_host()).await.unwrap();
    assert_eq!(server.requests().len(), 1);

    let stranger = self::pki();
    let distrusting = client_trusting(&server, &stranger.ca);
    let error = distrusting.leases().await.unwrap_err();
    assert!(matches!(error, Error::Unreachable(_)), "{error:?}");
    assert!(!error.may_have_applied());

    let by_address = server.origin().replace("localhost", "127.0.0.1");
    let config = Config::new(by_address.parse().unwrap(), Token::new(TOKEN).unwrap());
    let wrong_name = Client::new(config.with_ca_pem(pki.ca.clone().into_bytes())).unwrap();
    let error = wrong_name.leases().await.unwrap_err();
    assert!(matches!(error, Error::Unreachable(_)), "{error:?}");
    assert_eq!(server.requests().len(), 1, "no request reached the server");
}

/// Asserts the configuration errors of a client: `https` needs a CA, `http` refuses one, and
/// a CA text that holds no certificate or a broken one is `BadCa`. It catches an `https`
/// client built with no trust anchors (it would trust nothing and fail at the first call, or
/// worse, trust the system).
#[tokio::test]
async fn a_client_is_built_only_from_a_usable_config() {
    let token = Token::new(TOKEN).unwrap();
    let https = || {
        Config::new(
            "https://leases.farm.example".parse().unwrap(),
            token.clone(),
        )
    };
    assert!(matches!(Client::new(https()), Err(ConfigError::MissingCa)));
    let http = Config::new("http://127.0.0.1:1".parse().unwrap(), token.clone());
    let with_ca = http.with_ca_pem(pki().ca.into_bytes());
    assert!(matches!(Client::new(with_ca), Err(ConfigError::UnusedCa)));
    for pem in [
        &b""[..],
        b"not a certificate",
        b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
        b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    ] {
        let config = https().with_ca_pem(pem.to_vec());
        assert!(
            matches!(Client::new(config), Err(ConfigError::BadCa(_))),
            "{pem:?}"
        );
    }
}

/// Asserts that a name TLS cannot carry is refused when the client is built, not at the first
/// call.
#[tokio::test]
async fn an_origin_host_a_certificate_cannot_carry_is_refused_up_front() {
    let token = Token::new(TOKEN).unwrap();
    let config = Config::new("https://a..b:1".parse().unwrap(), token);
    let config = config.with_ca_pem(pki().ca.into_bytes());
    assert!(matches!(
        Client::new(config),
        Err(ConfigError::Origin { .. })
    ));
}

/// Asserts that nothing listening is `Unreachable`, and that the caller may repeat the call:
/// the request cannot have been applied. It catches a refused connection reported as
/// ambiguous, which would make a caller taint a host for nothing.
#[tokio::test]
async fn a_closed_port_is_unreachable_and_safe_to_repeat() {
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let config = Config::new(
        format!("http://{closed}").parse().unwrap(),
        Token::new(TOKEN).unwrap(),
    );
    let client = Client::new(config).unwrap();
    let error = client
        .queue_run("4-1".parse().unwrap(), &run_of(1000, 1))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unreachable(_)), "{error:?}");
    assert!(!error.may_have_applied());
    assert!(
        error
            .to_string()
            .starts_with("cannot reach the lease server")
    );
}

/// Asserts that a TLS handshake the server never answers is `Unreachable` once the connect
/// timeout passes: nothing was sent. It catches a connect step with no timeout, which would
/// hang the executor on one dead host.
#[tokio::test]
async fn a_handshake_that_never_finishes_times_out_as_unreachable() {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap();
    let config = Config::new(
        format!("https://localhost:{}", addr.port())
            .parse()
            .unwrap(),
        Token::new(TOKEN).unwrap(),
    )
    .with_ca_pem(pki().ca.into_bytes())
    .with_timeouts(SHORT, Duration::from_secs(30));
    let client = Client::new(config).unwrap();
    let error = client.hosts().await.unwrap_err();
    assert!(matches!(error, Error::Unreachable(_)), "{error:?}");
    assert!(
        error.to_string().contains("timed out connecting"),
        "{error}"
    );
    drop(silent);
}

/// Asserts how each way the exchange can break after the request left is reported: a dropped
/// connection, a server that never answers (the request timeout), an answer cut short, and an
/// oversized answer are all `Transport` (the server may have acted: for a queued run that is
/// the ambiguous case); a success status with a body that is not the expected JSON is
/// `Decode`. It catches each being mistaken for `Unreachable` (a double-run on retry).
#[tokio::test]
async fn a_break_after_sending_may_have_applied() {
    let (server, _) = fake().await;
    let config = server.config().with_timeouts(Duration::from_secs(5), SHORT);
    let client = Client::new(config).unwrap();
    let id = "4-1".parse().unwrap();
    let spec = run_of(1000, 1);

    server.inject(Fault::Close);
    let error = client.queue_run(id, &spec).await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");
    assert!(error.may_have_applied());
    assert!(error.to_string().contains("after the request was sent"));

    // A fault aimed at a route waits for it, and the requests before it are served.
    server.inject_on("GET /v1/hosts", Fault::Close);
    client.leases().await.unwrap();
    let error = client.hosts().await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");
    client.hosts().await.unwrap();

    server.inject(Fault::Hang);
    let error = client.queue_run(id, &spec).await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("timed out waiting for the answer"),
        "{error}"
    );

    server.inject(Fault::ShortBody);
    let error = client.leases().await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");

    server.inject(Fault::Reply {
        status: 200,
        body: Bytes::from(vec![b' '; (16 << 20) + 1]),
    });
    let error = client.leases().await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)), "{error:?}");
    assert!(
        error.to_string().contains("length limit exceeded"),
        "{error}"
    );

    server.inject(Fault::Reply {
        status: 202,
        body: Bytes::from_static(b"<html>queued</html>"),
    });
    let error = client.queue_run(id, &spec).await.unwrap_err();
    assert!(
        matches!(error, Error::Decode { status: 202, .. }),
        "{error:?}"
    );
    assert!(error.may_have_applied());
}

/// Asserts how answers that are not the server's own are reported: a status the route does
/// not take with an unreadable body is a refusal with no code, a body with a code this client
/// does not know keeps the code's text, a 500 `state-write-failed` is its own code, and a
/// success status of the wrong kind (200 where 202 is documented) is a refusal, not a decode
/// of the wrong thing. It catches a proxy's HTML 502 being decoded as JSON and a new server
/// code being lost.
#[tokio::test]
async fn answers_that_are_not_the_expected_ones_are_refusals() {
    let (server, client) = fake().await;
    let id = "4-1".parse().unwrap();
    for (status, body, code) in [
        (
            502,
            &b"<html>bad gateway</html>"[..],
            ErrorCode::Unspecified,
        ),
        (
            409,
            br#"{"error":"something-new"}"#,
            ErrorCode::Other("something-new".to_owned()),
        ),
        (
            500,
            br#"{"error":"state-write-failed"}"#,
            ErrorCode::StateWriteFailed,
        ),
        (
            200,
            br#"{"run_id":"run-4-1","state":"queued"}"#,
            ErrorCode::Unspecified,
        ),
    ] {
        server.inject(Fault::Reply {
            status,
            body: Bytes::from_static(body),
        });
        let error = client.queue_run(id, &run_of(1000, 1)).await.unwrap_err();
        assert_eq!(refused(error), (status, code));
    }
}

/// Asserts that the client is usable from many tasks at once, each call on its own
/// connection: concurrent acquires get distinct hosts. It catches shared connection state.
#[tokio::test]
async fn clones_call_concurrently() {
    let (_server, client) = fake().await;
    let other = client.clone();
    let request = whole_host();
    let (a, b) = tokio::join!(client.acquire(&request), other.acquire(&request));
    let host = |acquired| match acquired {
        kbf_lease_client::Acquired::Granted(granted) => granted.host,
        other => panic!("{other:?}"),
    };
    let mut hosts = [host(a.unwrap()), host(b.unwrap())];
    hosts.sort();
    assert_eq!(hosts, ["node1", "node2"]);
}
