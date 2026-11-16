//! What the client makes of a connection, a TLS session and an exchange that fail, against
//! the fake and against loopback sockets of the test's own.

mod support;

use std::time::Duration;

use bytes::Bytes;
use kbf_trees_client::fake::{Call, FakeTreesServer, Fault};
use kbf_trees_client::v1::{CreateTreeResponse, Entry};
use kbf_trees_client::{Broken, Client, Config, Error, Reason, Undecodable, Unsent};
use prost::Message as _;
use support::{create, fake, fake_and_client, fixed, job, pki, server_tls, whole};
use tokio::net::TcpListener;
use tonic::Code;

/// The deadline of a call with the default timeouts: the connect and the call together.
fn deadline() -> Duration {
    Config::CONNECT_TIMEOUT + Config::CALL_TIMEOUT
}

/// Asserts a create whose answer is lost after the node applied it is `Transport` (the node
/// may have acted), and that the same request sent again answers the first placement with
/// `created` false: the repeat is safe.
#[tokio::test]
async fn a_transport_failure_is_retried_with_the_same_request() {
    let (server, client) = fake_and_client().await;
    let request = create(vec![whole("a", 1, 1)], vec![job("j")]);
    server.inject(Fault::LoseAnswer);
    let lost = client.create_tree(request.clone()).await.unwrap_err();
    assert!(
        matches!(lost, Error::Transport(Broken::Status(_))),
        "{lost:?}"
    );
    assert!(lost.may_have_applied());
    assert_eq!(server.placements().len(), 1, "the node applied the create");
    let again = client.create_tree(request).await.unwrap();
    assert!(!again.is_new());
    assert_eq!(server.placements(), [again.placement().as_str()]);
}

/// Asserts a call the node takes and never answers ends at the deadline (the connect and
/// call timeouts together) as `Transport` `Deadline`, on the test's clock: the real clock
/// runs until the fake has the request, then it is frozen and moved past the deadline.
#[tokio::test]
async fn a_call_that_is_never_answered_ends_at_its_deadline() {
    let (server, client) = fake_and_client().await;
    server.inject_on("CreateTree", Fault::Hang);
    let task = tokio::spawn(async move {
        client
            .create_tree(create(vec![whole("a", 1, 1)], vec![job("j")]))
            .await
    });
    server.hung().await;
    tokio::time::pause();
    tokio::time::advance(deadline()).await;
    let late = task.await.unwrap().unwrap_err();
    tokio::time::resume();
    assert!(
        matches!(late, Error::Transport(Broken::Deadline(after)) if after == deadline()),
        "{late:?}"
    );
    assert!(late.may_have_applied());
    assert!(
        server.placements().is_empty(),
        "the hung call was not applied"
    );
    assert_eq!(server.calls().len(), 1);
}

/// Asserts what a status the node answers becomes: a refusal by name when its message and
/// code are the node's (`busy` under `UNAVAILABLE`), a lost answer for an `UNAVAILABLE` that
/// names none, and a definite status for an `UNIMPLEMENTED` with no name (a node without the
/// interface); none of them applied.
#[tokio::test]
async fn a_status_is_a_refusal_a_lost_answer_or_definite() {
    let (server, client) = fake_and_client().await;
    let request = create(vec![whole("a", 1, 1)], vec![job("j")]);
    let fault = |code, message: &str| Fault::Status {
        code,
        message: message.to_owned(),
    };
    server.inject(fault(Code::Unavailable, "busy"));
    let busy = client.create_tree(request.clone()).await.unwrap_err();
    assert!(busy.is(Reason::Busy), "{busy:?}");
    assert!(!busy.may_have_applied());
    server.inject(fault(Code::Unavailable, "connection reset by peer"));
    let reset = client.create_tree(request.clone()).await.unwrap_err();
    assert!(
        matches!(reset, Error::Transport(Broken::Status(_))),
        "{reset:?}"
    );
    server.inject(fault(Code::Unimplemented, ""));
    let old = client.create_tree(request.clone()).await.unwrap_err();
    assert!(
        matches!(
            old,
            Error::Status {
                code: Code::Unimplemented,
                ..
            }
        ),
        "{old:?}"
    );
    assert!(server.placements().is_empty());
    assert_eq!(server.calls().len(), 3);
    assert!(matches!(&server.calls()[0], Call::CreateTree(sent) if **sent == request));
}

/// Asserts an OK answer the client cannot use is `Decode`, which may have applied: bytes
/// that are not the message, a digest that is not 32 bytes, no placement.
#[tokio::test]
async fn an_ok_answer_the_client_cannot_use_is_undecodable() {
    let (server, client) = fake_and_client().await;
    let request = create(vec![whole("a", 1, 1)], vec![job("j")]);
    server.inject(Fault::Answer(Bytes::from_static(&[0x0f, 0x0f])));
    let garbled = client.create_tree(request.clone()).await.unwrap_err();
    assert!(
        matches!(garbled, Error::Decode(Undecodable::Bytes(_))),
        "{garbled:?}"
    );
    assert!(garbled.may_have_applied());
    let short = CreateTreeResponse {
        tree: vec![1; 3],
        placement: "p1".to_owned(),
        ..CreateTreeResponse::default()
    };
    server.inject(Fault::Answer(short.encode_to_vec().into()));
    let error = client.create_tree(request.clone()).await.unwrap_err();
    assert!(
        matches!(error, Error::Decode(Undecodable::Digest(3))),
        "{error:?}"
    );
    let unplaced = CreateTreeResponse {
        tree: vec![1; 32],
        ..CreateTreeResponse::default()
    };
    server.inject(Fault::Answer(unplaced.encode_to_vec().into()));
    let error = client.create_tree(request).await.unwrap_err();
    assert!(
        matches!(error, Error::Decode(Undecodable::NoPlacement)),
        "{error:?}"
    );
}

/// Asserts a request over the cap is not sent: `Unreachable` `TooLarge` with its size, and
/// the node reads nothing.
#[tokio::test]
async fn a_request_over_the_cap_is_not_sent() {
    let (server, client) = fake_and_client().await;
    let path = "p".repeat(Client::MAX_MESSAGE);
    let huge = create(vec![Entry { path, kind: None }], vec![]);
    let error = client.create_tree(huge).await.unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::TooLarge { len, cap }) if len > cap && cap == Client::MAX_MESSAGE),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert!(server.heads().is_empty());
}

/// Asserts a node that refuses the connection is `Unreachable` (nothing sent), not a lost
/// answer.
#[tokio::test]
async fn a_node_that_refuses_the_connection_is_unreachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = listener.local_addr().unwrap().to_string();
    drop(listener);
    let config = Config::new(
        closed.parse().unwrap(),
        config_key(),
        kbf_trees_client::fake::REGION,
    )
    .unwrap()
    .with_clock(fixed);
    let client = Client::new(config).unwrap();
    let error = client
        .create_tree(create(vec![whole("a", 1, 1)], vec![]))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );
    assert!(!error.may_have_applied());
    assert!(
        error
            .to_string()
            .starts_with("nothing was sent to the trees interface: cannot connect")
    );
}

fn config_key() -> kbf_trees_client::Key {
    kbf_trees_client::Key::new(
        kbf_trees_client::fake::KEY_ID,
        kbf_trees_client::fake::SECRET,
    )
    .unwrap()
}

/// Asserts a call over TLS to a node whose certificate the pinned CA signed for its host is
/// answered, and the node gets it signed; a connection to the node that closes before its
/// handshake leaves the node serving.
#[tokio::test]
async fn a_node_with_a_certificate_of_the_pinned_ca_is_called_over_tls() {
    let (ca, cert, key) = pki(&["localhost"]);
    let server = FakeTreesServer::start_tls(server_tls(&cert, &key))
        .await
        .unwrap();
    server.set_clock(fixed);
    let port = server.node().rsplit_once(':').unwrap().1.to_owned();
    drop(
        tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap(),
    );
    let config = server
        .config()
        .unwrap()
        .with_clock(fixed)
        .with_ca(ca.into_bytes());
    let client = Client::new(config).unwrap();
    assert!(!client.in_clear_off_loopback());
    assert!(format!("{client:?}").contains("tls: true"));
    let created = client
        .create_tree(create(vec![whole("a", 1, 1)], vec![job("j")]))
        .await
        .unwrap();
    assert!(created.is_new());
}

/// Asserts a node whose certificate another CA signed, or whose certificate does not carry
/// its host, is `Unreachable`: the handshake fails and nothing is sent. Catches a client that
/// trusts any certificate, or system roots.
#[tokio::test]
async fn a_node_the_pinned_ca_does_not_vouch_for_is_unreachable() {
    let (_ca, cert, key) = pki(&["localhost"]);
    let (other_ca, _, _) = pki(&["localhost"]);
    let (ca, wrong_name_cert, wrong_name_key) = pki(&["node.farm"]);
    let cases = [(cert, key, other_ca), (wrong_name_cert, wrong_name_key, ca)];
    for (cert, key, trusted) in cases {
        let server = FakeTreesServer::start_tls(server_tls(&cert, &key))
            .await
            .unwrap();
        let config = server.config().unwrap().with_ca(trusted.into_bytes());
        let error = Client::new(config)
            .unwrap()
            .create_tree(create(vec![whole("a", 1, 1)], vec![]))
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Unreachable(Unsent::Connect(_))),
            "{error:?}"
        );
        assert!(server.heads().is_empty());
    }
}

/// Asserts a CA file that holds no certificate, or a broken one, is refused when the client
/// is made, and a host a certificate cannot name is refused for TLS.
#[tokio::test]
async fn an_unusable_ca_or_host_is_refused_when_the_client_is_made() {
    let (_server, config) = fake().await;
    let empty = Client::new(config.clone().with_ca(Vec::new())).unwrap_err();
    assert_eq!(empty.to_string(), "the CA PEM is unusable: no certificate");
    let broken = b"-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n".to_vec();
    let refused = Client::new(config.clone().with_ca(broken)).unwrap_err();
    assert!(
        refused.to_string().starts_with("the CA PEM is unusable"),
        "{refused}"
    );
    let (ca, _, _) = pki(&["localhost"]);
    let odd = Config::new("a..b:1".parse().unwrap(), config_key(), "r").unwrap();
    let named = Client::new(odd.with_ca(ca.into_bytes())).unwrap_err();
    assert!(
        matches!(&named, kbf_trees_client::ConfigError::Address { reason, .. } if *reason == "not a name a certificate can carry"),
        "{named:?}"
    );
}

/// Asserts a node that takes the connection and never finishes the TLS handshake fails at
/// the connect timeout as `Unreachable`, on the test's clock.
#[tokio::test(start_paused = true)]
async fn a_node_that_never_finishes_the_handshake_is_unreachable_at_the_connect_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let holding = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = listener.accept().await {
            held.push(tcp);
        }
    });
    let (ca, _, _) = pki(&["localhost"]);
    let node = format!("localhost:{}", addr.port()).parse().unwrap();
    let config = Config::new(node, config_key(), "r")
        .unwrap()
        .with_ca(ca.into_bytes())
        .with_timeouts(Duration::from_secs(2), Duration::from_secs(60));
    let error = Client::new(config)
        .unwrap()
        .create_tree(create(vec![whole("a", 1, 1)], vec![]))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Unreachable(Unsent::Connect(_))),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("no connection within 2s"),
        "{error}"
    );
    holding.abort();
}

/// Asserts a node off loopback with no CA is refused unless plaintext is allowed, naming the
/// node, and that a client made with the opt-in says it calls in clear off loopback; a
/// loopback node needs no opt-in and does not. Catches a remote call in clear without the
/// operator's say-so.
#[tokio::test]
async fn a_remote_node_without_tls_is_refused_unless_plaintext_is_allowed() {
    let remote = Config::new("192.0.2.5:9600".parse().unwrap(), config_key(), "farm").unwrap();
    let refused = Client::new(remote.clone()).unwrap_err();
    assert!(
        matches!(&refused, kbf_trees_client::ConfigError::Plaintext(n) if n == "192.0.2.5:9600")
    );
    assert_eq!(
        refused.to_string(),
        "trees address \"192.0.2.5:9600\" is off loopback and no CA is pinned; the calls would cross the network in clear"
    );
    let allowed = Client::new(remote.allow_plaintext()).unwrap();
    assert!(allowed.in_clear_off_loopback());
    let local = Config::new("127.0.0.1:9600".parse().unwrap(), config_key(), "farm").unwrap();
    let local = Client::new(local).unwrap();
    assert!(!local.in_clear_off_loopback());
    assert_eq!(
        format!("{local:?}"),
        "Client { node: \"127.0.0.1:9600\", tls: false, .. }"
    );
}
