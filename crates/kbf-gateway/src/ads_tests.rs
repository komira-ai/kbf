use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use envoy_types::pb::google::rpc::Status as RpcStatus;
use futures::channel::mpsc as requests;
use tokio::task::JoinHandle;

use super::*;
use crate::registry::{Claim, Clock};

type Requests = requests::UnboundedSender<Result<DiscoveryRequest, Status>>;
type Replies = mpsc::Receiver<Result<DiscoveryResponse, Status>>;

/// Far longer than a test runs: no entry expires while a test is looking.
const TTL: Duration = Duration::from_secs(3600);

fn registry() -> Arc<Registry> {
    Arc::new(Registry::new(TTL, Clock::system()))
}

fn claim(name: &str, port: u16, term: u64) -> Claim {
    Claim {
        name: name.to_owned(),
        address: SocketAddr::from(([192, 0, 2, 7], port)),
        is_leader: true,
        term,
    }
}

/// A stream over `registry`'s leader, with its request sender and reply receiver.
fn open(registry: &Registry) -> (JoinHandle<()>, Requests, Replies) {
    let (send, received) = requests::unbounded();
    let (out, replies) = mpsc::channel(8);
    let task = tokio::spawn(serve(received, registry.subscribe(), out));
    (task, send, replies)
}

fn request(nonce: &str, version: &str) -> DiscoveryRequest {
    DiscoveryRequest {
        version_info: version.to_owned(),
        resource_names: vec![CLUSTER.to_owned()],
        type_url: ClusterLoadAssignment::type_url(),
        response_nonce: nonce.to_owned(),
        ..DiscoveryRequest::default()
    }
}

fn send(requests: &Requests, request: DiscoveryRequest) {
    requests.unbounded_send(Ok(request)).expect("stream open");
}

async fn next(replies: &mut Replies) -> DiscoveryResponse {
    replies
        .recv()
        .await
        .expect("a response")
        .expect("not an error")
}

/// The endpoint address of the one resource in `response`, if it has one.
fn endpoint_of(response: &DiscoveryResponse) -> Option<String> {
    let [resource] = response.resources.as_slice() else {
        panic!("one resource per response");
    };
    let assignment = ClusterLoadAssignment::decode(resource.value.as_slice()).expect("decode");
    let locality = assignment.endpoints.first()?;
    let Some(lb_endpoint::HostIdentifier::Endpoint(endpoint)) =
        locality.lb_endpoints.first()?.host_identifier.clone()
    else {
        panic!("an endpoint");
    };
    let Some(address::Address::SocketAddress(socket)) = endpoint.address?.address else {
        panic!("a socket address");
    };
    let Some(socket_address::PortSpecifier::PortValue(port)) = socket.port_specifier else {
        panic!("a port");
    };
    Some(format!("{}:{port}", socket.address))
}

/// Catches: a subscription answered with a stale leader, a change of leader not pushed
/// to an open stream, a leader that is lost shown as the old one, and nonces that repeat
/// (Envoy matches an ACK to the response by its nonce).
#[tokio::test]
async fn a_subscription_is_answered_and_changes_are_pushed() {
    let registry = registry();
    registry.register(claim("a", 1, 1));
    let (_task, send_request, mut replies) = open(&registry);
    send(&send_request, request("", ""));
    let first = next(&mut replies).await;
    assert_eq!(first.version_info, "1:a@192.0.2.7:1");
    assert_eq!(first.nonce, "1");
    assert_eq!(first.type_url, ClusterLoadAssignment::type_url());
    assert_eq!(endpoint_of(&first).as_deref(), Some("192.0.2.7:1"));

    registry.register(claim("b", 2, 2));
    let second = next(&mut replies).await;
    assert_eq!(second.version_info, "2:b@192.0.2.7:2");
    assert_eq!(second.nonce, "2");
    assert_eq!(endpoint_of(&second).as_deref(), Some("192.0.2.7:2"));

    registry.register(Claim {
        is_leader: false,
        ..claim("b", 2, 2)
    });
    registry.register(Claim {
        is_leader: false,
        ..claim("a", 1, 1)
    });
    // Envoy sees the state after each change it was told of; the last is no leader.
    let mut last = next(&mut replies).await;
    while last.version_info != "none" {
        last = next(&mut replies).await;
    }
    assert_eq!(endpoint_of(&last), None);
}

/// Catches: a change pushed to a stream that has not subscribed (Envoy would get a
/// response before asking, with a nonce it never saw), and a subscription that does not
/// get the leader as it is when it asks.
#[tokio::test]
async fn nothing_is_pushed_before_the_subscription() {
    let registry = registry();
    let (_task, send_request, mut replies) = open(&registry);
    registry.register(claim("a", 1, 1));
    registry.register(claim("b", 2, 2));
    send(&send_request, request("", ""));
    let first = next(&mut replies).await;
    assert_eq!(first.version_info, "2:b@192.0.2.7:2", "the leader as it is");
    assert_eq!(first.nonce, "1", "the subscription's answer is the first");
    registry.register(claim("c", 3, 3));
    let second = next(&mut replies).await;
    assert_eq!(second.version_info, "3:c@192.0.2.7:3");
    assert_eq!(second.nonce, "2", "no push came before the subscription");
}

/// Catches: a farm with no leader answered with an error or no response (Envoy would
/// keep the last endpoint); an empty assignment is how the endpoint is withdrawn.
#[tokio::test]
async fn no_leader_is_an_empty_assignment() {
    let registry = registry();
    let (_task, send_request, mut replies) = open(&registry);
    send(&send_request, request("", ""));
    let first = next(&mut replies).await;
    assert_eq!(first.version_info, "none");
    assert_eq!(endpoint_of(&first), None);
}

/// What the log of one test receives.
#[derive(Clone, Default)]
struct Log(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log").clone()).expect("UTF-8")
    }
}

/// Catches: an ACK, a NACK or a request for another type being answered. A NACK answered
/// with the same bytes would loop for as long as Envoy refuses them. The proof is the
/// order of the replies: the first one after all of them is the push they did not cause.
/// And a NACK that is dropped without a trace: Envoy's message is the only report of why
/// the endpoint did not change, so it is logged.
#[tokio::test]
async fn acks_and_nacks_get_no_response() {
    let log = Log::default();
    let writer = log.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish(),
    );
    let registry = registry();
    registry.register(claim("a", 1, 1));
    let (_task, send_request, mut replies) = open(&registry);
    send(&send_request, request("", ""));
    assert_eq!(next(&mut replies).await.nonce, "1");

    send(&send_request, request("1", "1:a@192.0.2.7:1"));
    let mut nack = request("1", "");
    nack.error_detail = Some(RpcStatus {
        code: 3,
        message: "bad assignment".to_owned(),
        details: Vec::new(),
    });
    send(&send_request, nack);
    let mut other = request("", "");
    other.type_url = "type.googleapis.com/envoy.config.cluster.v3.Cluster".to_owned();
    send(&send_request, other);

    registry.register(claim("b", 2, 2));
    let pushed = next(&mut replies).await;
    assert_eq!(pushed.nonce, "2", "nothing was sent for the three requests");
    assert_eq!(pushed.version_info, "2:b@192.0.2.7:2");
    let logged = log.text();
    assert!(
        logged.contains("refused the endpoint assignment"),
        "{logged}"
    );
    assert!(logged.contains("bad assignment"), "{logged}");
    assert!(logged.contains("does not serve"), "{logged}");
}

/// Catches: a stream task that outlives its stream: Envoy hanging up, a transport error
/// on the request stream, the reply side going away, or the registry being dropped each
/// must end it (a leaked task per reconnect).
#[tokio::test]
async fn the_task_ends_with_its_stream() {
    let registry = registry();
    let (task, send_request, _replies) = open(&registry);
    drop(send_request);
    task.await.expect("ends when Envoy hangs up");

    let (task, send_request, _replies) = open(&registry);
    send_request
        .unbounded_send(Err(Status::unavailable("reset")))
        .expect("stream open");
    task.await.expect("ends on a transport error");

    let (task, send_request, replies) = open(&registry);
    send(&send_request, request("", ""));
    drop(replies);
    registry.register(claim("a", 1, 1));
    task.await.expect("ends when nobody reads the replies");

    let (task, _send_request, _replies) = open(&registry);
    drop(registry);
    task.await.expect("ends when the registry is gone");
}
