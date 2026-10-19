//! Which child port, auth class, hold bypass and idle timeout a request path has.
//!
//! The table is closed: a path that matches no prefix is [`Route::Unknown`] and is answered
//! by the proxy, never forwarded. A child therefore never sees a path the proxy did not
//! classify, and a new child service needs a line here and a test.

use std::time::Duration;

/// A listener of a child frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// REAPI and ByteStream.
    Reapi,
    /// Worker sessions, membership and the release service.
    Worker,
    /// The operator API (`/v1/*`).
    Operator,
    /// Enrollment (`/kbf.enroll.v1.Enroll/*`): a node that has no certificate yet. The child
    /// serves it on a listener of its own that authenticates the server only; the port is
    /// fixed here so the child's unit adds the listener without a proxy change.
    Enroll,
}

impl Target {
    /// The offset of this listener's port from the slot's base port.
    #[must_use]
    pub const fn offset(self) -> u16 {
        match self {
            Self::Reapi => 0,
            Self::Worker => 1,
            Self::Operator => 2,
            Self::Enroll => 4,
        }
    }

    /// Whether the listener speaks gRPC (the operator API is plain HTTP).
    #[must_use]
    pub const fn grpc(self) -> bool {
        !matches!(self, Self::Operator)
    }
}

/// What the proxy does with a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Forward to the active child's listener.
    Child(Target),
    /// Served by the proxy: `/v1/update/frontend*`.
    Update,
    /// `/healthz`.
    Health,
    /// `/readyz`.
    Ready,
    /// `/metrics`, served by the proxy about itself.
    Metrics,
    /// Not in the table: the proxy answers 404 (HTTP) or `UNIMPLEMENTED` (gRPC).
    Unknown,
}

impl Route {
    /// Whether the route speaks gRPC, so a refusal is a gRPC status and not an HTTP one.
    #[must_use]
    pub const fn grpc(self) -> bool {
        match self {
            Self::Child(target) => target.grpc(),
            _ => false,
        }
    }
}

/// Who may call a path (see [`crate::auth`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthClass {
    /// Needs a verified worker or storage certificate.
    Worker,
    /// Follows `buck2_auth`.
    Reapi,
    /// Needs the operator token.
    Operator,
    /// Needs the update credential.
    Update,
    /// Open to a caller with no certificate: the child checks the enrollment token.
    Enroll,
    /// Open: health and readiness.
    Probe,
}

/// How long a stream may sit silent before the proxy closes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// Never: Execute and WaitExecution hold open for the whole action.
    Never,
    /// After this long.
    After(Duration),
}

/// The silent-stream bound of every path but the two above (7200 s).
pub const IDLE_DEFAULT: Duration = Duration::from_secs(7200);

const REAPI_PREFIXES: [&str; 5] = [
    "/build.bazel.remote.execution.v2.Execution/",
    "/build.bazel.remote.execution.v2.ActionCache/",
    "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
    "/build.bazel.remote.execution.v2.Capabilities/",
    "/google.bytestream.ByteStream/",
];
// `ColdStore` is the worker listener's (storage role only, which the child's role table
// enforces; the proxy checks only that the caller is a worker or storage node).
const WORKER_PREFIXES: [&str; 4] = [
    "/kbf.worker.",
    "/kbf.map.v1.Membership/",
    "/kbf.update.",
    "/kbf.enroll.v1.ColdStore/",
];
const ENROLL_PREFIX: &str = "/kbf.enroll.v1.Enroll/";
const UPDATE_PREFIX: &str = "/v1/update/frontend";
const BYTESTREAM_PREFIX: &str = "/google.bytestream.ByteStream/";

/// The path with any query string removed.
fn bare(path: &str) -> &str {
    path.split_once('?').map_or(path, |(p, _)| p)
}

/// Whether `path` is `prefix` itself or `prefix` followed by `/`, so `/v1/update/frontendx`
/// is not an update path.
fn at_boundary(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The route of `path`.
#[must_use]
pub fn route_of(path: &str) -> Route {
    let path = bare(path);
    if REAPI_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Route::Child(Target::Reapi);
    }
    if WORKER_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Route::Child(Target::Worker);
    }
    if path.starts_with(ENROLL_PREFIX) {
        return Route::Child(Target::Enroll);
    }
    if at_boundary(path, UPDATE_PREFIX) {
        return Route::Update;
    }
    if path.starts_with("/v1/") {
        return Route::Child(Target::Operator);
    }
    match path {
        "/healthz" => Route::Health,
        "/readyz" => Route::Ready,
        "/metrics" => Route::Metrics,
        _ => Route::Unknown,
    }
}

/// The auth class of a route; `None` for [`Route::Unknown`], which is refused before auth.
#[must_use]
pub const fn auth_class(route: Route) -> Option<AuthClass> {
    match route {
        Route::Child(Target::Reapi) => Some(AuthClass::Reapi),
        Route::Child(Target::Worker) => Some(AuthClass::Worker),
        Route::Child(Target::Operator) => Some(AuthClass::Operator),
        Route::Child(Target::Enroll) => Some(AuthClass::Enroll),
        Route::Update => Some(AuthClass::Update),
        Route::Health | Route::Ready => Some(AuthClass::Probe),
        Route::Metrics => Some(AuthClass::Operator),
        Route::Unknown => None,
    }
}

/// Whether the hold gate lets the route through while it holds. Only a stream going to a
/// backend is held; everything the proxy answers itself bypasses: the update API (so `abort`
/// cannot deadlock), the probes, the metrics, and a path outside the table (answered 404 or
/// `UNIMPLEMENTED` at once, never forwarded). An operator call (`/v1/*` but the update API) is
/// held like any backend stream: it waits in the bounded hold queue during a swap hold and is
/// answered 503 at the bound.
#[must_use]
pub const fn bypasses_hold(route: Route) -> bool {
    !matches!(route, Route::Child(_))
}

/// The idle timeout of `path`. Execute and WaitExecution are matched by their full method
/// path, so a generic route table needs no per-stream state.
#[must_use]
pub fn idle_of(path: &str) -> Idle {
    match bare(path) {
        "/build.bazel.remote.execution.v2.Execution/Execute"
        | "/build.bazel.remote.execution.v2.Execution/WaitExecution" => Idle::Never,
        _ => Idle::After(IDLE_DEFAULT),
    }
}

/// Whether `path` is a ByteStream call, which is never retried.
#[must_use]
pub fn is_bytestream(path: &str) -> bool {
    bare(path).starts_with(BYTESTREAM_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listener_has_its_paths() {
        let reapi = "/build.bazel.remote.execution.v2.Execution/Execute";
        assert_eq!(route_of(reapi), Route::Child(Target::Reapi));
        assert_eq!(
            route_of("/google.bytestream.ByteStream/Write"),
            Route::Child(Target::Reapi)
        );
        for p in [
            "/kbf.worker.v1.Worker/Session",
            "/kbf.map.v1.Membership/Join",
            "/kbf.update.v1.ReleaseService/Fetch",
        ] {
            assert_eq!(route_of(p), Route::Child(Target::Worker), "{p}");
        }
        assert_eq!(route_of("/v1/nodes"), Route::Child(Target::Operator));
        assert_eq!(
            route_of("/v1/update/desired"),
            Route::Child(Target::Operator)
        );
        assert_eq!(route_of("/v1/update"), Route::Child(Target::Operator));
        assert_eq!(
            Target::Reapi.offset() + Target::Worker.offset() + Target::Operator.offset(),
            3
        );
        assert_eq!(Target::Enroll.offset(), 4, "past the admin port at 3");
        for target in [Target::Reapi, Target::Worker, Target::Enroll] {
            assert!(target.grpc(), "{target:?}");
        }
        assert!(!Target::Operator.grpc());
        assert!(route_of(reapi).grpc());
        for route in [
            Route::Update,
            Route::Health,
            Route::Ready,
            Route::Metrics,
            Route::Unknown,
        ] {
            assert!(!route.grpc(), "{route:?}");
        }
    }

    #[test]
    fn enrollment_is_its_own_listener_and_cold_store_is_the_workers() {
        let enroll = route_of("/kbf.enroll.v1.Enroll/Enroll");
        assert_eq!(enroll, Route::Child(Target::Enroll));
        assert_eq!(route_of("/kbf.enroll.v1.Enroll/Renew"), enroll);
        assert_eq!(auth_class(enroll), Some(AuthClass::Enroll));
        let cold = route_of("/kbf.enroll.v1.ColdStore/GetKeys");
        assert_eq!(cold, Route::Child(Target::Worker));
        assert_eq!(auth_class(cold), Some(AuthClass::Worker));
        assert_eq!(route_of("/kbf.enroll.v1.Other/X"), Route::Unknown);
        assert!(!bypasses_hold(enroll));
    }

    #[test]
    fn the_update_api_is_the_proxys_own_and_matches_on_a_boundary() {
        for p in [
            "/v1/update/frontend",
            "/v1/update/frontend/abort",
            "/v1/update/frontend?x=1",
        ] {
            assert_eq!(route_of(p), Route::Update, "{p}");
        }
        assert_eq!(
            route_of("/v1/update/frontendx"),
            Route::Child(Target::Operator)
        );
    }

    #[test]
    fn a_path_outside_the_table_is_never_forwarded() {
        for p in [
            "/",
            "/kbf.enroll.v1/Enroll",
            "/grpc.health.v1.Health/Check",
            "/x",
        ] {
            assert_eq!(route_of(p), Route::Unknown, "{p}");
            assert_eq!(auth_class(route_of(p)), None);
        }
    }

    #[test]
    fn auth_classes_and_hold_bypass() {
        assert_eq!(
            auth_class(Route::Child(Target::Reapi)),
            Some(AuthClass::Reapi)
        );
        assert_eq!(
            auth_class(Route::Child(Target::Worker)),
            Some(AuthClass::Worker)
        );
        assert_eq!(
            auth_class(Route::Child(Target::Operator)),
            Some(AuthClass::Operator)
        );
        assert_eq!(auth_class(Route::Update), Some(AuthClass::Update));
        for (path, bypass) in [
            ("/v1/update/frontend/abort", true),
            ("/healthz", true),
            ("/readyz", true),
            ("/metrics", true),
            ("/not/in/the/table", true),
            ("/v1/update/frontendx", false),
            ("/v1/nodes", false),
            ("/kbf.worker.v1.Worker/Session", false),
            ("/google.bytestream.ByteStream/Read", false),
        ] {
            assert_eq!(bypasses_hold(route_of(path)), bypass, "{path}");
        }
        assert_eq!(auth_class(Route::Health), Some(AuthClass::Probe));
        assert_eq!(auth_class(Route::Ready), Some(AuthClass::Probe));
        assert_eq!(auth_class(Route::Metrics), Some(AuthClass::Operator));
    }

    #[test]
    fn only_execute_and_wait_execution_are_never_idle() {
        let exec = "/build.bazel.remote.execution.v2.Execution/";
        assert_eq!(idle_of(&format!("{exec}Execute")), Idle::Never);
        assert_eq!(idle_of(&format!("{exec}WaitExecution")), Idle::Never);
        assert_eq!(
            idle_of("/google.bytestream.ByteStream/Write"),
            Idle::After(IDLE_DEFAULT)
        );
        assert_eq!(
            idle_of("/kbf.worker.v1.Worker/Session"),
            Idle::After(IDLE_DEFAULT)
        );
        assert!(is_bytestream("/google.bytestream.ByteStream/Write"));
        assert!(!is_bytestream(&format!("{exec}Execute")));
    }
}
