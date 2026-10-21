//! The rules of [`super::check`] that read a connection manager, its routes and a
//! cluster, and the helpers that read a parsed `envoy.json` for them.
//!
//! The model is Envoy's route choice for the calls a Buck2 or Bazel client makes
//! ([`METHODS`]): the first route whose `path` equals the call's path or whose `prefix`
//! starts it. A route that matches on anything else (headers, a regex, a runtime
//! fraction), or whose action does more than pick a cluster, set its timeouts and retry
//! what repeats no work, is a failure of its own ([`CheckFailure::RouteMatch`],
//! [`CheckFailure::RouteAction`]): the check refuses what it does not model rather than
//! mis-model it. A field the check does not read is not checked; Envoy's own
//! `--mode validate` is what refuses an unknown field.
//!
//! A value of the wrong JSON type reads as absent. A duration that is not a protobuf
//! JSON duration (`"7200s"`, `"0.5s"`, at most nine fractional digits) stops the Envoy
//! checks with [`CheckFailure::EnvoyShape`].

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use super::failure::CheckFailure;
use super::render::{HTTP_OPTIONS, RETRY_ON, ROUTER, UPSTREAM_TLS};

/// Every call a Buck2 or Bazel client makes through the box. Each must reach a route
/// with no bound on a stream's whole life (route timeout 0, no `max_stream_duration`)
/// and an idle timeout over the longest silent stream ([`SILENT`]): Execute and
/// WaitExecution are those streams, and the stream idle timeout is one value for every
/// route unless a route overrides it.
pub const METHODS: [&str; 12] = [
    "build.bazel.remote.execution.v2.Execution/Execute",
    "build.bazel.remote.execution.v2.Execution/WaitExecution",
    "build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
    "build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
    "build.bazel.remote.execution.v2.ActionCache/GetActionResult",
    "build.bazel.remote.execution.v2.ActionCache/UpdateActionResult",
    "google.bytestream.ByteStream/Read",
    "google.bytestream.ByteStream/Write",
    "google.bytestream.ByteStream/QueryWriteStatus",
];

/// The longest silent stream: a 1 h action plus the 300 s unservable queue wait.
pub const SILENT: Duration = Duration::from_secs(3900);
/// Envoy's stream idle timeout when the connection manager sets none.
pub const DEFAULT_STREAM_IDLE: Duration = Duration::from_secs(300);
/// Envoy's route timeout when a route sets none.
pub const DEFAULT_ROUTE_TIMEOUT: Duration = Duration::from_secs(15);

/// The names kbf-server's grpc.health.v1 service answers (`kbf_server::grpc_health::
/// SERVICES`, which a kbf-it test compares with this list); any other name is
/// NOT_FOUND, so the server is never healthy.
pub const HEALTH_SERVICES: [&str; 6] = [
    "",
    "build.bazel.remote.execution.v2.Capabilities",
    "build.bazel.remote.execution.v2.ContentAddressableStorage",
    "google.bytestream.ByteStream",
    "build.bazel.remote.execution.v2.ActionCache",
    "build.bazel.remote.execution.v2.Execution",
];

/// The only RouteMatch fields the route model reads.
const MATCH_KEYS: [&str; 3] = ["path", "prefix", "grpc"];
/// The only RouteAction fields the route model reads.
const ACTION_KEYS: [&str; 5] = [
    "cluster",
    "timeout",
    "idle_timeout",
    "max_stream_duration",
    "retry_policy",
];

/// A duration the Envoy checks cannot read; it becomes [`CheckFailure::EnvoyShape`].
#[derive(Debug)]
pub(super) struct Malformed(pub(super) String);

/// The failures found so far.
#[derive(Default)]
pub(super) struct Checker {
    pub(super) failures: Vec<CheckFailure>,
}

impl Checker {
    /// Record `failure()` unless `ok`; return `ok`.
    pub(super) fn require(&mut self, ok: bool, failure: impl FnOnce() -> CheckFailure) -> bool {
        if !ok {
            self.failures.push(failure());
        }
        ok
    }
}

/// `v` as a list; anything else is an empty one.
pub(super) fn list(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// `v` for a message: a string as itself, anything else as JSON.
pub(super) fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// Python's truth: absent, null, false, 0, `""`, `[]` and `{}` are false.
pub(super) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The keys of the object `v` outside `known`, sorted.
fn extra_keys(v: &Value, known: &[&str]) -> Vec<String> {
    let keys = v.as_object().into_iter().flat_map(|o| o.keys());
    keys.filter(|k| !known.contains(&k.as_str()))
        .cloned()
        .collect()
}

/// A protobuf JSON duration (`"7200s"`, `"0.5s"`).
pub(super) fn duration(v: &Value) -> Result<Duration, Malformed> {
    let bad = || Malformed(format!("not a duration: {v}"));
    let num = v
        .as_str()
        .and_then(|s| s.strip_suffix('s'))
        .ok_or_else(bad)?;
    let (whole, frac) = num.split_once('.').unwrap_or((num, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(frac) || frac.len() > 9 {
        return Err(bad());
    }
    let secs: u64 = whole.parse().map_err(|_| bad())?;
    let nanos = format!("{frac:0<9}")
        .bytes()
        .fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
    Ok(Duration::new(secs, nanos))
}

/// A duration in seconds for a message: `15`, `0.5`.
pub(super) fn show(d: Duration) -> String {
    let frac = format!("{:09}", d.subsec_nanos());
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        d.as_secs().to_string()
    } else {
        format!("{}.{frac}", d.as_secs())
    }
}

/// A name-keyed view of a list, as a Python dict built from it: the first position of
/// each name, holding the last item with that name.
pub(super) fn by_name<'a>(
    items: impl Iterator<Item = (&'a Value, &'a Value)>,
) -> Vec<(&'a Value, &'a Value)> {
    let mut out: Vec<(&Value, &Value)> = Vec::new();
    for (k, v) in items {
        match out.iter_mut().find(|(seen, _)| *seen == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out
}

pub(super) fn get<'a>(dict: &[(&Value, &'a Value)], key: &Value) -> Option<&'a Value> {
    dict.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// A stream's whole life must not be bounded: `max_stream_duration` absent or 0 in
/// `opts` (an HttpProtocolOptions or a RouteAction's MaxStreamDuration).
fn unbounded(c: &mut Checker, opts: &Value, at: &str) -> Result<(), Malformed> {
    let d = &opts["max_stream_duration"];
    let ok = d.is_null() || duration(d)?.is_zero();
    c.require(ok, || CheckFailure::MaxStreamDuration {
        at: at.to_owned(),
        duration: text(d),
    });
    Ok(())
}

/// Envoy's route choice for a gRPC call on `path`: the first route whose match takes
/// it. Exact for a match on `path` or `prefix` only; the route-match check refuses any
/// other key.
fn first_route<'a>(routes: &'a [Value], path: &str) -> Result<Option<&'a Value>, Malformed> {
    for r in routes {
        let m = &r["match"];
        if m["path"] == path {
            return Ok(Some(r));
        }
        match &m["prefix"] {
            Value::Null => {}
            Value::String(p) if path.starts_with(p.as_str()) => return Ok(Some(r)),
            Value::String(_) => {}
            other => return Err(Malformed(format!("a route prefix {other}"))),
        }
    }
    Ok(None)
}

/// The connection manager `hcm`, its routes and the stream bounds every method of
/// [`METHODS`] meets; the names of the clusters the routes use.
pub(super) fn check_hcm(
    c: &mut Checker,
    hcm: &Value,
    clusters: &[(&Value, &Value)],
) -> Result<BTreeSet<String>, Malformed> {
    let codec = hcm.get("codec_type");
    let h2 = codec.is_none_or(|v| *v == "AUTO" || *v == "HTTP2");
    c.require(h2, || CheckFailure::Codec {
        codec: codec.map_or_else(String::new, text),
    });
    let idle = match hcm.get("stream_idle_timeout") {
        Some(d) => duration(d)?,
        None => DEFAULT_STREAM_IDLE,
    };
    unbounded(
        c,
        &hcm["common_http_protocol_options"],
        "the connection manager",
    )?;
    let rt = &hcm["request_timeout"];
    let ok = rt.is_null() || duration(rt)?.is_zero();
    c.require(ok, || CheckFailure::RequestTimeout { timeout: text(rt) });
    let filters: Vec<&Value> = list(&hcm["http_filters"])
        .iter()
        .map(|f| &f["typed_config"]["@type"])
        .collect();
    c.require(matches!(filters.as_slice(), [f] if **f == ROUTER), || {
        CheckFailure::Filters {
            filters: json!(filters).to_string(),
        }
    });
    let vhosts = list(&hcm["route_config"]["virtual_hosts"]);
    let every_authority = vhosts.first().is_some_and(|v| v["domains"] == json!(["*"]));
    c.require(vhosts.len() == 1 && every_authority, || CheckFailure::Vhost);
    let routes = vhosts.first().map_or(&[][..], |v| list(&v["routes"]));
    for r in routes {
        check_route_shape(c, r);
    }
    let mut used = BTreeSet::new();
    for method in METHODS {
        let path = format!("/{method}");
        let Some(r) = first_route(routes, &path)? else {
            c.failures.push(CheckFailure::Route { path });
            continue;
        };
        let action = &r["route"];
        let cluster = &action["cluster"];
        let defined = get(clusters, cluster).is_some();
        if defined {
            used.insert(cluster.to_string());
        }
        c.require(defined, || CheckFailure::RouteCluster {
            path: path.clone(),
            cluster: text(cluster),
        });
        // The route timeout bounds the whole response (from the end of the request to
        // the end of the response stream), not silence: any value cuts an Execute or
        // a ByteStream Read that lives longer, however busy.
        let timeout = match action.get("timeout") {
            Some(d) => duration(d)?,
            None => DEFAULT_ROUTE_TIMEOUT,
        };
        c.require(timeout.is_zero(), || CheckFailure::RouteTimeout {
            path: path.clone(),
            secs: show(timeout),
        });
        unbounded(c, &action["max_stream_duration"], &path)?;
        let route_idle = action.get("idle_timeout").map(duration).transpose()?;
        let idle = route_idle.unwrap_or(idle);
        c.require(idle.is_zero() || idle > SILENT, || {
            CheckFailure::IdleTimeout {
                path: path.clone(),
                secs: show(idle),
            }
        });
        // An action's own timeout is uncapped, so a bound on silence that the
        // connection manager's value gives these two can cut a long one.
        let long = method.ends_with("/Execute") || method.ends_with("/WaitExecution");
        c.require(!long || route_idle.is_some_and(|d| d.is_zero()), || {
            CheckFailure::ExecuteIdle {
                path: path.clone(),
                got: route_idle.map_or_else(|| "absent".to_owned(), show),
            }
        });
        check_retry(c, r, &path);
    }
    Ok(used)
}

/// A route's `retry_policy`: only conditions that repeat no work ([`RETRY_ON`]), and
/// none on a ByteStream call, whose body streams.
fn check_retry(c: &mut Checker, r: &Value, path: &str) {
    let policy = &r["route"]["retry_policy"];
    if policy.is_null() {
        return;
    }
    let bytestream = path.starts_with("/google.bytestream.");
    let on: Vec<&str> = policy["retry_on"]
        .as_str()
        .map_or_else(Vec::new, |s| s.split(',').collect());
    let allowed = RETRY_ON.split(',').collect::<Vec<_>>();
    let extra = extra_keys(policy, &["retry_on", "num_retries"]);
    let why = if bytestream {
        Some("a ByteStream body streams and cannot be replayed".to_owned())
    } else if on.is_empty() || !on.iter().all(|c| allowed.contains(c)) {
        Some(format!(
            "retry_on {} is not within {RETRY_ON}",
            text(&policy["retry_on"])
        ))
    } else if !extra.is_empty() {
        Some(format!("unmodelled keys {}", json!(extra)))
    } else {
        None
    };
    if let Some(why) = why {
        c.failures.push(CheckFailure::RetryRoutes {
            path: path.to_owned(),
            why,
        });
    }
}

/// A route matches on `path` or `prefix` alone, is not a catch-all, and its action
/// only picks a cluster, sets timeouts and retries.
fn check_route_shape(c: &mut Checker, r: &Value) {
    let m = &r["match"];
    let prefix = m["prefix"].as_str();
    c.require(prefix != Some("") && prefix != Some("/"), || {
        CheckFailure::NoCatchAll
    });
    let extra = extra_keys(m, &MATCH_KEYS);
    let one_kind = usize::from(m.get("path").is_some()) + usize::from(m.get("prefix").is_some());
    let grpc = m.get("grpc").is_none_or(|g| *g == json!({}));
    c.require(extra.is_empty() && one_kind == 1 && grpc, || {
        CheckFailure::RouteMatch {
            matched: m.to_string(),
            extra: json!(extra).to_string(),
        }
    });
    let extra = extra_keys(&r["route"], &ACTION_KEYS);
    c.require(extra.is_empty(), || CheckFailure::RouteAction {
        matched: m.to_string(),
        extra: json!(extra).to_string(),
    });
}

/// A cluster a route uses: health-checked by grpc.health.v1 on a name kbf-server
/// answers, never sending to an unhealthy server, HTTP/2 over TLS to the settings' CA,
/// the one name checked on every instance.
pub(super) fn check_cluster(
    c: &mut Checker,
    cl: &Value,
    name: &str,
    ca_file: &str,
) -> Result<(), Malformed> {
    let cluster = || name.to_owned();
    let hcs = list(&cl["health_checks"]);
    c.require(!hcs.is_empty(), || CheckFailure::HealthCheck {
        cluster: cluster(),
    });
    for hc in hcs {
        c.require(hc.get("grpc_health_check").is_some(), || {
            CheckFailure::HealthCheckGrpc { cluster: cluster() }
        });
        c.require(hc.get("transport_socket_match_criteria").is_none(), || {
            CheckFailure::HealthCheckTls { cluster: cluster() }
        });
        let service = hc["grpc_health_check"].get("service_name");
        let known =
            service.is_none_or(|s| s.as_str().is_some_and(|s| HEALTH_SERVICES.contains(&s)));
        c.require(known, || CheckFailure::HealthCheckService {
            cluster: cluster(),
            service: service.map_or_else(String::new, |s| s.to_string()),
        });
    }
    let panic = &cl["common_lb_config"]["healthy_panic_threshold"]["value"];
    c.require(panic.as_f64() == Some(0.0), || {
        CheckFailure::PanicThreshold {
            cluster: cluster(),
            value: if panic.is_null() {
                "absent (Envoy's default 50)".to_owned()
            } else {
                panic.to_string()
            },
        }
    });
    let opts = &cl["typed_extension_protocol_options"][HTTP_OPTIONS];
    c.require(
        opts["explicit_http_config"]
            .get("http2_protocol_options")
            .is_some(),
        || CheckFailure::UpstreamH2 { cluster: cluster() },
    );
    unbounded(
        c,
        &opts["common_http_protocol_options"],
        &format!("cluster {name}"),
    )?;
    let matches = list(&cl["transport_socket_matches"]);
    let default = format!("cluster {name} default");
    tls(c, &cl["transport_socket"], &default, ca_file, None);
    let eps = endpoints(cl);
    c.require(!eps.is_empty(), || CheckFailure::Endpoints {
        cluster: cluster(),
    });
    for e in eps {
        // An instance is dialled by loopback port and checked by the one name it was
        // given: the endpoint's hostname.
        let host = &e["endpoint"]["hostname"];
        let criteria = &e["metadata"]["filter_metadata"]["envoy.transport_socket_match"];
        let m = matches
            .iter()
            .find(|m| truthy(criteria) && m["match"] == *criteria);
        let at = format!("server {}", text(host));
        match m {
            Some(m) => tls(c, &m["transport_socket"], &at, ca_file, Some(host)),
            None => c
                .failures
                .push(CheckFailure::EndpointTlsMatch { host: text(host) }),
        }
    }
    Ok(())
}

/// Every endpoint of a cluster, across its locality groups.
pub(super) fn endpoints(cl: &Value) -> Vec<&Value> {
    list(&cl["load_assignment"]["endpoints"])
        .iter()
        .flat_map(|g| list(&g["lb_endpoints"]))
        .collect()
}

/// A transport socket of the hop to the instances: TLS that trusts the settings' CA and
/// offers h2; for one instance (`host`), its SNI and the certificate's name too.
fn tls(c: &mut Checker, ts: &Value, at: &str, ca_file: &str, host: Option<&Value>) {
    let tc = &ts["typed_config"];
    if !c.require(tc["@type"] == UPSTREAM_TLS, || CheckFailure::UpstreamTls {
        at: at.to_owned(),
    }) {
        return;
    }
    let ctx = &tc["common_tls_context"];
    let vc = &ctx["validation_context"];
    let ca = &vc["trusted_ca"]["filename"];
    c.require(ca.as_str() == Some(ca_file), || CheckFailure::UpstreamCa {
        at: at.to_owned(),
        got: text(ca),
        want: ca_file.to_owned(),
    });
    let h2 = list(&ctx["alpn_protocols"]).iter().any(|p| *p == "h2");
    c.require(h2, || CheckFailure::UpstreamAlpn { at: at.to_owned() });
    let Some(host) = host else {
        return;
    };
    c.require(tc["sni"] == *host, || CheckFailure::UpstreamSni {
        at: at.to_owned(),
        sni: text(&tc["sni"]),
    });
    let want = json!([{"san_type": "DNS", "matcher": {"exact": host}}]);
    c.require(vc["match_typed_subject_alt_names"] == want, || {
        CheckFailure::UpstreamSan {
            at: at.to_owned(),
            host: text(host),
        }
    });
}
