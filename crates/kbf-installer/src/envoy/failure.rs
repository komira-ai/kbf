//! The failures [`super::check`] reports: one variant per check, its name the word after
//! `FAIL`, its message the defect it lets through.

use super::rules::SILENT;

/// One broken property of a render, and the defect it lets through.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckFailure {
    #[error("the Envoy config is malformed ({why})")]
    EnvoyShape { why: String },
    #[error("want one chain with one HTTP connection manager")]
    Hcm,
    #[error("codec {codec} cannot take gRPC's HTTP/2")]
    Codec { codec: String },
    #[error(
        "{at}: max_stream_duration {duration} ends every stream older than that, silent or not"
    )]
    MaxStreamDuration { at: String, duration: String },
    #[error("request_timeout {timeout} cuts a ByteStream Write still uploading after that")]
    RequestTimeout { timeout: String },
    #[error(
        "http_filters {filters} is not the router alone (a buffering filter caps a 64 MiB write)"
    )]
    Filters { filters: String },
    #[error("want one virtual host for every authority (a client's Host is not ours to pin)")]
    Vhost,
    #[error("a catch-all route sends any path (grpc.health.v1, the operator API) to the servers")]
    NoCatchAll,
    #[error(
        "match {matched} is not a path or prefix alone (unmodelled keys {extra}): the route choice this check models is not Envoy's"
    )]
    RouteMatch { matched: String, extra: String },
    #[error("route {matched} has {extra}: a rewrite, retry or redirect this check does not model")]
    RouteAction { matched: String, extra: String },
    #[error("no route for {path}")]
    Route { path: String },
    #[error("{path} goes to {cluster}, not a defined cluster")]
    RouteCluster { path: String, cluster: String },
    #[error(
        "{path} is cut {secs} s into its response (route timeout); it must be 0, with the idle timeout bounding silence"
    )]
    RouteTimeout { path: String, secs: String },
    #[error("{path} is cut after {secs} s of silence, under the {silent} s silent stream", silent = SILENT.as_secs())]
    IdleTimeout { path: String, secs: String },
    #[error("cluster {cluster} has no health check")]
    HealthCheck { cluster: String },
    #[error("cluster {cluster}: a health check that is not grpc.health.v1")]
    HealthCheckGrpc { cluster: String },
    #[error("cluster {cluster}: the health check picks its own transport socket")]
    HealthCheckTls { cluster: String },
    #[error(
        "cluster {cluster}: kbf-server answers service {service} NOT_FOUND, so no server is ever healthy"
    )]
    HealthCheckService { cluster: String, service: String },
    #[error(
        "cluster {cluster}: healthy_panic_threshold {value} sends traffic to unhealthy servers"
    )]
    PanicThreshold { cluster: String, value: String },
    #[error("cluster {cluster}: upstream is not HTTP/2")]
    UpstreamH2 { cluster: String },
    #[error("{at}: not TLS (the hop to the servers must verify the internal CA)")]
    UpstreamTls { at: String },
    #[error("{at}: trusted_ca {got} is not the settings' CA {want}")]
    UpstreamCa {
        at: String,
        got: String,
        want: String,
    },
    #[error("{at}: ALPN without h2")]
    UpstreamAlpn { at: String },
    #[error("{at}: SNI {sni}")]
    UpstreamSni { at: String, sni: String },
    #[error("{at}: the certificate's name is not checked against {host}")]
    UpstreamSan { at: String, host: String },
    #[error("cluster {cluster} has no server")]
    Endpoints { cluster: String },
    #[error("server {host}: no transport socket match, so its name is not checked")]
    EndpointTlsMatch { host: String },
    #[error("the box config has {got} listeners, not the REAPI one and the worker one")]
    Listeners { got: String },
    #[error(
        "the REAPI listener has no TLS with a certificate and h2: REAPI clients dial it with TLS"
    )]
    ListenerTls,
    #[error(
        "{path} is cut after {got} s of silence, not never: an action's own timeout is uncapped, so Execute and WaitExecution need a route idle_timeout of 0s"
    )]
    ExecuteIdle { path: String, got: String },
    #[error("{path}: retry policy outside what repeats no work ({why})")]
    RetryRoutes { path: String, why: String },
    #[error(
        "cluster {cluster}: close_connections_on_host_health_failure cuts the open streams of an instance that is only draining"
    )]
    HealthKeepsStreams { cluster: String },
    #[error("cluster {cluster}: {why}, so a concurrent stream past it is refused")]
    Breakers { cluster: String, why: String },
    #[error("cluster {cluster}: no HTTP/2 keepalive on the upstream connections")]
    UpstreamKeepalive { cluster: String },
    #[error(
        "cluster {cluster}: upstream max_concurrent_streams is {got}, not at most kbf's {want}: a burst of streams is sent down one fresh connection and refused"
    )]
    UpstreamStreams {
        cluster: String,
        got: String,
        want: String,
    },
    #[error("cluster {cluster}: {why}")]
    Instances { cluster: String, why: String },
    #[error("port {port} is used twice ({first} and {second})")]
    InstancePorts {
        port: String,
        first: String,
        second: String,
    },
    #[error("the worker listener is not a tcp_proxy alone to kbf_workers ({why})")]
    WorkerListener { why: String },
    #[error("{at}: TLS in Envoy on the worker hop loses the daemon's certificate identity")]
    WorkerTls { at: String },
    #[error(
        "the worker proxy's idle_timeout is {got}, not 0s: a daemon's session is quiet between heartbeats"
    )]
    WorkerIdle { got: String },
    #[error("cluster kbf_workers: {why}")]
    WorkerHealth { why: String },
    #[error("the admin interface listens on {address}, not loopback")]
    AdminLoopback { address: String },
    #[error(
        "--parent-shutdown-time-s is {got}, under the longest action ({want} s): the old epoch is closed with its streams open"
    )]
    ParentShutdown { got: String, want: String },
    #[error("--drain-time-s {got} is not below --parent-shutdown-time-s {parent}")]
    DrainTime { got: String, parent: String },
}

impl CheckFailure {
    /// Every check's name, in the order [`check`](super::check::check) can report them:
    /// the connection manager's, routes' and clusters' rules first, the box's own after.
    pub const NAMES: [&'static str; 44] = [
        "envoy-shape",
        "hcm",
        "codec",
        "max-stream-duration",
        "request-timeout",
        "filters",
        "vhost",
        "no-catch-all",
        "route-match",
        "route-action",
        "route",
        "route-cluster",
        "route-timeout",
        "idle-timeout",
        "health-check",
        "health-check-grpc",
        "health-check-tls",
        "health-check-service",
        "panic-threshold",
        "upstream-h2",
        "upstream-tls",
        "upstream-ca",
        "upstream-alpn",
        "upstream-sni",
        "upstream-san",
        "endpoints",
        "endpoint-tls-match",
        "listeners",
        "listener-tls",
        "execute-idle-off",
        "retry-routes",
        "health-keeps-streams",
        "breakers",
        "upstream-keepalive",
        "upstream-streams",
        "instances",
        "instance-ports",
        "worker-listener",
        "worker-tls",
        "worker-idle",
        "worker-health",
        "admin-loopback",
        "parent-shutdown",
        "drain-time",
    ];

    /// The check's name, the word after `FAIL`.
    pub fn name(&self) -> &'static str {
        match self {
            CheckFailure::EnvoyShape { .. } => "envoy-shape",
            CheckFailure::Hcm => "hcm",
            CheckFailure::Codec { .. } => "codec",
            CheckFailure::MaxStreamDuration { .. } => "max-stream-duration",
            CheckFailure::RequestTimeout { .. } => "request-timeout",
            CheckFailure::Filters { .. } => "filters",
            CheckFailure::Vhost => "vhost",
            CheckFailure::NoCatchAll => "no-catch-all",
            CheckFailure::RouteMatch { .. } => "route-match",
            CheckFailure::RouteAction { .. } => "route-action",
            CheckFailure::Route { .. } => "route",
            CheckFailure::RouteCluster { .. } => "route-cluster",
            CheckFailure::RouteTimeout { .. } => "route-timeout",
            CheckFailure::IdleTimeout { .. } => "idle-timeout",
            CheckFailure::HealthCheck { .. } => "health-check",
            CheckFailure::HealthCheckGrpc { .. } => "health-check-grpc",
            CheckFailure::HealthCheckTls { .. } => "health-check-tls",
            CheckFailure::HealthCheckService { .. } => "health-check-service",
            CheckFailure::PanicThreshold { .. } => "panic-threshold",
            CheckFailure::UpstreamH2 { .. } => "upstream-h2",
            CheckFailure::UpstreamTls { .. } => "upstream-tls",
            CheckFailure::UpstreamCa { .. } => "upstream-ca",
            CheckFailure::UpstreamAlpn { .. } => "upstream-alpn",
            CheckFailure::UpstreamSni { .. } => "upstream-sni",
            CheckFailure::UpstreamSan { .. } => "upstream-san",
            CheckFailure::Endpoints { .. } => "endpoints",
            CheckFailure::EndpointTlsMatch { .. } => "endpoint-tls-match",
            CheckFailure::Listeners { .. } => "listeners",
            CheckFailure::ListenerTls => "listener-tls",
            CheckFailure::ExecuteIdle { .. } => "execute-idle-off",
            CheckFailure::RetryRoutes { .. } => "retry-routes",
            CheckFailure::HealthKeepsStreams { .. } => "health-keeps-streams",
            CheckFailure::Breakers { .. } => "breakers",
            CheckFailure::UpstreamKeepalive { .. } => "upstream-keepalive",
            CheckFailure::UpstreamStreams { .. } => "upstream-streams",
            CheckFailure::Instances { .. } => "instances",
            CheckFailure::InstancePorts { .. } => "instance-ports",
            CheckFailure::WorkerListener { .. } => "worker-listener",
            CheckFailure::WorkerTls { .. } => "worker-tls",
            CheckFailure::WorkerIdle { .. } => "worker-idle",
            CheckFailure::WorkerHealth { .. } => "worker-health",
            CheckFailure::AdminLoopback { .. } => "admin-loopback",
            CheckFailure::ParentShutdown { .. } => "parent-shutdown",
            CheckFailure::DrainTime { .. } => "drain-time",
        }
    }

    /// The line a check prints: `FAIL <check>: <why>`.
    pub fn line(&self) -> String {
        format!("FAIL {}: {self}", self.name())
    }
}
