//! The xDS listener as Envoy uses it: an ADS stream over a real connection, registrations
//! arriving through the mutual-TLS listener, and the registry's TTL moved by a test clock.

mod support;

use std::time::Duration;

use envoy_types::pb::envoy::config::endpoint::v3::ClusterLoadAssignment;
use envoy_types::pb::envoy::service::discovery::v3::{
    DeltaDiscoveryRequest, DiscoveryRequest, DiscoveryResponse,
};
use futures::channel::mpsc;
use kbf_gateway::ads::CLUSTER;
use kbf_proto::gateway::RegisterRequest;
use prost::{Message as _, Name as _};
use support::{Pki, TTL, start};
use tonic::{Code, Streaming};

fn subscribe() -> DiscoveryRequest {
    DiscoveryRequest {
        resource_names: vec![CLUSTER.to_owned()],
        type_url: ClusterLoadAssignment::type_url(),
        ..DiscoveryRequest::default()
    }
}

fn claim(address: &str, term: u64) -> RegisterRequest {
    RegisterRequest {
        reapi_address: address.to_owned(),
        is_leader: true,
        term,
    }
}

async fn next(stream: &mut Streaming<DiscoveryResponse>) -> DiscoveryResponse {
    stream
        .message()
        .await
        .expect("the stream is healthy")
        .expect("a response")
}

/// Catches: a leader that registered over mutual TLS not reaching an Envoy subscribed over
/// ADS, a later leader not being pushed to the open stream, and a registration whose TTL
/// ran out (the test clock moved past it, no sleeping) not changing what Envoy is told.
#[tokio::test]
async fn envoy_follows_registrations_and_their_expiry() {
    let pki = Pki::new();
    let gateway = start(&pki);
    let mut one = gateway.registrar(&pki.server("kbf1.farm.test"));
    let mut two = gateway.registrar(&pki.server("kbf2.farm.test"));
    one.register(claim("192.0.2.10:8980", 1))
        .await
        .expect("one");

    let mut ads = gateway.ads().await;
    let (requests, received) = mpsc::unbounded();
    let mut stream = ads
        .stream_aggregated_resources(received)
        .await
        .expect("open the stream")
        .into_inner();
    requests.unbounded_send(subscribe()).expect("subscribe");
    let first = next(&mut stream).await;
    assert_eq!(first.version_info, "1:kbf1.farm.test@192.0.2.10:8980");

    two.register(claim("192.0.2.20:8980", 2))
        .await
        .expect("two");
    let second = next(&mut stream).await;
    assert_eq!(second.version_info, "2:kbf2.farm.test@192.0.2.20:8980");
    assert_eq!(second.nonce, "2");

    // Two keeps the lead, but only one renews: after the TTL the other is gone and the
    // sweep announces the survivor.
    gateway.clock.advance(Duration::from_secs(4));
    one.register(claim("192.0.2.10:8980", 1))
        .await
        .expect("renewed");
    gateway.clock.advance(TTL - Duration::from_secs(4));
    gateway.registry.sweep();
    let third = next(&mut stream).await;
    assert_eq!(third.version_info, "1:kbf1.farm.test@192.0.2.10:8980");
    assert_eq!(gateway.registry.servers(), ["kbf1.farm.test"]);

    let resource = third.resources.first().expect("the assignment");
    let assignment = ClusterLoadAssignment::decode(resource.value.as_slice()).expect("decode");
    assert_eq!(assignment.cluster_name, CLUSTER);
}

/// Catches: the delta protocol being accepted (its state would never be tracked).
#[tokio::test]
async fn the_delta_protocol_is_refused() {
    let gateway = start(&Pki::new());
    let mut ads = gateway.ads().await;
    let (_requests, received) = mpsc::unbounded::<DeltaDiscoveryRequest>();
    let refused = ads
        .delta_aggregated_resources(received)
        .await
        .expect_err("not served");
    assert_eq!(refused.code(), Code::Unimplemented);
}
