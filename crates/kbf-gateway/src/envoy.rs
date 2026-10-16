//! The Envoy bootstrap that goes with the gateway: `kbf-gateway envoy-config` prints it.
//!
//! Envoy's REAPI listener sends every call to the cluster `kbf_leader`, whose one
//! endpoint comes from the gateway over ADS (`kbf_xds`, a static cluster at the
//! gateway's local address). Everything else is static, so the gateway serves one
//! resource ([`crate::ads`]).
//!
//! **Streams.** Execute and WaitExecution streams are silent for as long as an action
//! runs, so the route has no timeout and the stream idle timeout is [`LONG_STREAM`],
//! above the longest silent stream (the container driver's default action timeout plus
//! the unservable queue wait), the same bound the Kubernetes front's config uses.
//!
//! **The stale-claim guard.** A server that lost the role may still hold a claim at the
//! gateway for up to the TTL. The cluster health-checks its endpoint with
//! `grpc.health.v1.Health/Check` on the REAPI listener, which answers with `/readyz`'s
//! checks (the `leader` check among them), so Envoy sends nothing to an endpoint that is
//! not ready, however fresh its claim.
//!
//! **TLS.** With `--upstream-ca` the cluster speaks TLS to the servers' REAPI listeners:
//! the certificate must chain to that CA and carry a DNS name ending in the given
//! suffix (the endpoints are IP addresses, so the name cannot be the endpoint's own).
//! Without it the cluster speaks plain text, for servers on loopback.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::ads::CLUSTER;

/// The stream idle timeout: above the longest silent REAPI stream.
pub const LONG_STREAM: &str = "7200s";

const HTTP_OPTIONS: &str = "envoy.extensions.upstreams.http.v3.HttpProtocolOptions";

/// What the bootstrap depends on.
#[derive(Clone, Debug)]
pub struct Options {
    /// Where Envoy's REAPI listener binds.
    pub listen: SocketAddr,
    /// Where the gateway's xDS listener is.
    pub xds: SocketAddr,
    /// Envoy's admin listener.
    pub admin: SocketAddr,
    /// TLS to the servers, or plain text.
    pub upstream: Option<Upstream>,
}

/// How Envoy checks the servers' certificates.
#[derive(Clone, Debug)]
pub struct Upstream {
    /// PEM file of the CA the certificates chain to.
    pub ca: PathBuf,
    /// A DNS name suffix every server certificate has a name under, such as
    /// `.farm.example`.
    pub san_suffix: String,
}

fn socket(address: SocketAddr) -> Value {
    json!({"socket_address": {"address": address.ip().to_string(), "port_value": address.port()}})
}

fn http2_options() -> Value {
    json!({
        HTTP_OPTIONS: {
            "@type": format!("type.googleapis.com/{HTTP_OPTIONS}"),
            "explicit_http_config": {"http2_protocol_options": {}}
        }
    })
}

fn tls(upstream: &Upstream) -> Value {
    json!({
        "name": "envoy.transport_sockets.tls",
        "typed_config": {
            "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext",
            "common_tls_context": {
                "alpn_protocols": ["h2"],
                "validation_context": {
                    "trusted_ca": {"filename": upstream.ca.to_string_lossy()},
                    "match_typed_subject_alt_names": [
                        {"san_type": "DNS", "matcher": {"suffix": upstream.san_suffix}}
                    ]
                }
            }
        }
    })
}

fn leader_cluster(upstream: Option<&Upstream>) -> Value {
    let mut cluster = json!({
        "name": CLUSTER,
        "type": "EDS",
        "connect_timeout": "5s",
        "lb_policy": "ROUND_ROBIN",
        "eds_cluster_config": {"eds_config": {"ads": {}, "resource_api_version": "V3"}},
        // Never send to an endpoint that fails its health check, not even when it is
        // the only one (Envoy's default would).
        "common_lb_config": {"healthy_panic_threshold": {"value": 0}},
        "typed_extension_protocol_options": http2_options(),
        "health_checks": [{
            "timeout": "1s",
            "interval": "2s",
            "unhealthy_threshold": 1,
            "healthy_threshold": 1,
            "always_log_health_check_failures": true,
            "grpc_health_check": {"service_name": ""}
        }]
    });
    if let Some(upstream) = upstream {
        cluster["transport_socket"] = tls(upstream);
    }
    cluster
}

fn xds_cluster(xds: SocketAddr) -> Value {
    json!({
        "name": "kbf_xds",
        "type": "STATIC",
        "connect_timeout": "1s",
        "typed_extension_protocol_options": http2_options(),
        "load_assignment": {
            "cluster_name": "kbf_xds",
            "endpoints": [{"lb_endpoints": [{"endpoint": {"address": socket(xds)}}]}]
        }
    })
}

fn listener(listen: SocketAddr) -> Value {
    json!({
        "name": "kbf_reapi",
        "address": socket(listen),
        "filter_chains": [{"filters": [{
            "name": "envoy.filters.network.http_connection_manager",
            "typed_config": {
                "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                "stat_prefix": "kbf_reapi",
                "codec_type": "AUTO",
                "stream_idle_timeout": LONG_STREAM,
                "route_config": {
                    "name": "kbf_reapi",
                    "virtual_hosts": [{
                        "name": "kbf",
                        "domains": ["*"],
                        "routes": [{
                            "match": {"prefix": "/", "grpc": {}},
                            "route": {"cluster": CLUSTER, "timeout": "0s"}
                        }]
                    }]
                },
                // Only the router: nothing buffers a request, so a large ByteStream
                // write streams through.
                "http_filters": [{
                    "name": "envoy.filters.http.router",
                    "typed_config": {
                        "@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"
                    }
                }]
            }
        }]}]
    })
}

/// The bootstrap as JSON text whose bytes depend only on `options`.
#[must_use]
pub fn render(options: &Options) -> String {
    let config = json!({
        "admin": {"address": socket(options.admin)},
        "dynamic_resources": {
            "ads_config": {
                "api_type": "GRPC",
                "transport_api_version": "V3",
                "set_node_on_first_message_only": true,
                "grpc_services": [{"envoy_grpc": {"cluster_name": "kbf_xds"}}]
            }
        },
        "static_resources": {
            "listeners": [listener(options.listen)],
            "clusters": [xds_cluster(options.xds), leader_cluster(options.upstream.as_ref())]
        }
    });
    let mut text = serde_json::to_string_pretty(&config).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
#[path = "envoy_tests.rs"]
mod tests;
