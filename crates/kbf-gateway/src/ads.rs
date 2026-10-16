//! The xDS aggregated discovery service (state of the world) for the local Envoy.
//!
//! One resource is served: the `ClusterLoadAssignment` named [`CLUSTER`], whose single
//! endpoint is the current leader (no endpoint when there is none, which makes Envoy
//! answer 503 until a server leads). Envoy's other resources are static in the
//! generated bootstrap ([`crate::envoy`]), so a request for any other type is logged and
//! left unanswered.
//!
//! **Per stream.** A request that carries no response nonce is a subscription (Envoy's
//! first request on the stream, or its first after a gateway restart): it is answered
//! with the current assignment. A request that carries a nonce is the answer to one of
//! ours: an ACK needs nothing, and a NACK (`error_detail` set) is logged and not retried,
//! because the same bytes would be refused again. Every later change of the leader is
//! pushed to the streams that have subscribed.
//!
//! The version is `<term>:<name>@<address>` of the leader, `none` without one, so a
//! response with a version Envoy has already accepted is never a new leader.

use std::pin::Pin;

use envoy_types::pb::envoy::config::core::v3::{Address, SocketAddress, address, socket_address};
use envoy_types::pb::envoy::config::endpoint::v3::{
    ClusterLoadAssignment, Endpoint, LbEndpoint, LocalityLbEndpoints, lb_endpoint,
};
use envoy_types::pb::envoy::service::discovery::v3::aggregated_discovery_service_server::AggregatedDiscoveryService;
use envoy_types::pb::envoy::service::discovery::v3::{
    DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse,
};
use envoy_types::pb::google::protobuf::Any;
use futures::{Stream, StreamExt as _, stream};
use prost::{Message as _, Name as _};
use tokio::sync::{mpsc, watch};
use tonic::{Request, Response, Status, Streaming};

use crate::registry::{Leader, Registry};

/// The cluster whose endpoints the gateway serves; Envoy's EDS subscription for the
/// cluster of this name asks for it.
pub const CLUSTER: &str = "kbf_leader";

/// How many responses wait for a slow Envoy before the stream waits on it.
const OUTBOUND: usize = 4;

/// A stream of the service's replies.
pub type Replies<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// The assignment for `leader`: its address as the one endpoint, its node name as the
/// endpoint's hostname.
#[must_use]
pub fn assignment(leader: Option<&Leader>) -> ClusterLoadAssignment {
    let endpoints = leader
        .map(|l| LocalityLbEndpoints {
            lb_endpoints: vec![LbEndpoint {
                host_identifier: Some(lb_endpoint::HostIdentifier::Endpoint(Endpoint {
                    address: Some(Address {
                        address: Some(address::Address::SocketAddress(SocketAddress {
                            address: l.address.ip().to_string(),
                            port_specifier: Some(socket_address::PortSpecifier::PortValue(
                                u32::from(l.address.port()),
                            )),
                            ..SocketAddress::default()
                        })),
                    }),
                    hostname: l.name.clone(),
                    ..Endpoint::default()
                })),
                ..LbEndpoint::default()
            }],
            ..LocalityLbEndpoints::default()
        })
        .into_iter()
        .collect();
    ClusterLoadAssignment {
        cluster_name: CLUSTER.to_owned(),
        endpoints,
        ..ClusterLoadAssignment::default()
    }
}

/// The version string of an answer for `leader`.
#[must_use]
pub fn version(leader: Option<&Leader>) -> String {
    leader.map_or_else(
        || "none".to_owned(),
        |l| format!("{}:{}@{}", l.term, l.name, l.address),
    )
}

/// The response that tells Envoy `leader` is the endpoint, with `nonce`.
#[must_use]
pub fn response(leader: Option<&Leader>, nonce: u64) -> DiscoveryResponse {
    DiscoveryResponse {
        version_info: version(leader),
        resources: vec![Any {
            type_url: ClusterLoadAssignment::type_url(),
            value: assignment(leader).encode_to_vec(),
        }],
        type_url: ClusterLoadAssignment::type_url(),
        nonce: nonce.to_string(),
        ..DiscoveryResponse::default()
    }
}

/// The ADS service over a [`Registry`]'s leader.
#[derive(Debug)]
pub struct AdsService {
    leaders: watch::Receiver<Option<Leader>>,
}

impl AdsService {
    /// The service that answers with `registry`'s leader.
    #[must_use]
    pub fn new(registry: &Registry) -> Self {
        Self {
            leaders: registry.subscribe(),
        }
    }
}

#[tonic::async_trait]
impl AggregatedDiscoveryService for AdsService {
    type StreamAggregatedResourcesStream = Replies<DiscoveryResponse>;

    async fn stream_aggregated_resources(
        &self,
        request: Request<Streaming<DiscoveryRequest>>,
    ) -> Result<Response<Self::StreamAggregatedResourcesStream>, Status> {
        let (out, replies) = mpsc::channel(OUTBOUND);
        tokio::spawn(serve(request.into_inner(), self.leaders.clone(), out));
        Ok(Response::new(Box::pin(stream::unfold(
            replies,
            |mut replies| async move { replies.recv().await.map(|reply| (reply, replies)) },
        ))))
    }

    type DeltaAggregatedResourcesStream = Replies<DeltaDiscoveryResponse>;

    async fn delta_aggregated_resources(
        &self,
        _request: Request<Streaming<DeltaDiscoveryRequest>>,
    ) -> Result<Response<Self::DeltaAggregatedResourcesStream>, Status> {
        Err(Status::unimplemented(
            "only the state-of-the-world protocol is served",
        ))
    }
}

/// One stream: answers its requests and pushes each change of `leaders` until either
/// side ends. Generic over the request stream so a test can feed it directly.
pub async fn serve<S>(
    mut requests: S,
    mut leaders: watch::Receiver<Option<Leader>>,
    out: mpsc::Sender<Result<DiscoveryResponse, Status>>,
) where
    S: Stream<Item = Result<DiscoveryRequest, Status>> + Unpin,
{
    let (mut subscribed, mut nonce) = (false, 0_u64);
    loop {
        let reply = tokio::select! {
            request = requests.next() => match request {
                Some(Ok(request)) => match classify(&request) {
                    Kind::Subscribe => {
                        subscribed = true;
                        true
                    }
                    Kind::Ack => false,
                    Kind::Nack => {
                        tracing::warn!(
                            version = %request.version_info,
                            nonce = %request.response_nonce,
                            reason = request.error_detail.as_ref().map_or("", |e| e.message.as_str()),
                            "Envoy refused the endpoint assignment; not sending it again"
                        );
                        false
                    }
                    Kind::Other => {
                        tracing::warn!(type_url = %request.type_url, "an xDS type the gateway does not serve");
                        false
                    }
                },
                Some(Err(status)) => {
                    tracing::info!(%status, "the xDS stream failed");
                    return;
                }
                None => return,
            },
            changed = leaders.changed() => {
                if changed.is_err() {
                    return;
                }
                subscribed
            }
        };
        if reply {
            nonce += 1;
            let current = leaders.borrow_and_update().clone();
            if out
                .send(Ok(response(current.as_ref(), nonce)))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// What a request asks of the gateway.
enum Kind {
    /// Envoy's first request for the endpoints on this stream.
    Subscribe,
    /// Envoy accepted a response of ours.
    Ack,
    /// Envoy refused a response of ours.
    Nack,
    /// A resource type this gateway does not serve.
    Other,
}

fn classify(request: &DiscoveryRequest) -> Kind {
    if request.type_url != ClusterLoadAssignment::type_url() {
        Kind::Other
    } else if request.error_detail.is_some() {
        Kind::Nack
    } else if request.response_nonce.is_empty() {
        Kind::Subscribe
    } else {
        Kind::Ack
    }
}

#[cfg(test)]
#[path = "ads_tests.rs"]
mod tests;
