//! The rules of the connection manager, routes and clusters ([`super::rules`]) on the box
//! render: one planted defect at a time, each caught by exactly the check that names
//! it, and the helpers that read and print values. The helpers that plant a defect in a
//! parsed render are `tests`' too.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{Value, json};

use super::check::check_box;
use super::config::BoxSettings;
use super::failure::CheckFailure;
use super::render::{Rendered, render};
use super::rules::{Malformed, duration, show, truthy};

pub(super) const REAPI: &str = "/static_resources/listeners/0";
pub(super) const WORKER: &str = "/static_resources/listeners/1";
pub(super) const HCM: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config";
pub(super) const ROUTES: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes";
pub(super) const RC: &str = "/static_resources/clusters/0";
pub(super) const WC: &str = "/static_resources/clusters/1";
pub(super) const PROXY: &str =
    "/static_resources/listeners/1/filter_chains/0/filters/0/typed_config";
const HEALTH: &str = "/static_resources/clusters/0/health_checks/0";
const MATCH_SOCKET: &str =
    "/static_resources/clusters/0/transport_socket_matches/0/transport_socket/typed_config";
const DEFAULT_SOCKET: &str = "/static_resources/clusters/0/transport_socket/typed_config";
const H2_OPTIONS: &str = "/static_resources/clusters/0/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions";

/// The numbers the goldens and the mutants are written for: a one-host box.
pub(super) fn example() -> BoxSettings {
    BoxSettings {
        host: "kbf.example.internal".to_owned(),
        reapi_port: 8980,
        worker_port: 8981,
        admin_port: 9901,
        backend_port_base: 18980,
        cert_file: "/etc/kbf/tls/reapi.pem".to_owned(),
        key_file: "/etc/kbf/tls/reapi.key".to_owned(),
        ca_file: "/etc/kbf/tls/ca.pem".to_owned(),
        reapi_max_streams: 10000,
        stream_ceiling: 65536,
        longest_action_secs: 21600,
    }
}

pub(super) fn rendered(settings: &BoxSettings) -> Rendered {
    render(settings).expect("valid settings")
}

pub(super) fn parsed(r: &Rendered) -> Value {
    serde_json::from_str(&r.envoy).expect("JSON")
}

/// The check on `envoy` and `args` against the example's numbers.
pub(super) fn run(envoy: &Value, args: &str) -> Vec<CheckFailure> {
    check_box(envoy, args, &example())
}

pub(super) fn at<'a>(v: &'a mut Value, pointer: &str) -> &'a mut Value {
    v.pointer_mut(pointer)
        .unwrap_or_else(|| panic!("no {pointer} in the render"))
}

pub(super) fn set(v: &mut Value, pointer: &str, value: Value) {
    *at(v, pointer) = value;
}

pub(super) fn remove(v: &mut Value, pointer: &str, key: &str) {
    let removed = at(v, pointer).as_object_mut().and_then(|o| o.remove(key));
    assert!(removed.is_some(), "no {key} at {pointer}");
}

pub(super) fn push(v: &mut Value, pointer: &str, item: Value) {
    at(v, pointer).as_array_mut().expect("a list").push(item);
}

/// Set a field the render does not have.
pub(super) fn set_new(v: &mut Value, pointer: &str, value: Value) {
    let (parent, key) = pointer.rsplit_once('/').expect("a field");
    let fields = at(v, parent).as_object_mut().expect("an object");
    assert!(
        fields.insert(key.to_owned(), value).is_none(),
        "{pointer} already set"
    );
}

/// A change to the parsed `envoy.json` and the text of `envoy.args`.
pub(super) type Plant = fn(&mut Value, &mut String);

/// One planted defect and the checks that must catch it, exactly.
struct Mutant {
    checks: &'static [&'static str],
    defect: &'static str,
    plant: Plant,
}

const fn m(checks: &'static [&'static str], defect: &'static str, plant: Plant) -> Mutant {
    Mutant {
        checks,
        defect,
        plant,
    }
}

/// A route to `/` that does what route 0 does, without a retry policy.
fn catch_all(e: &mut Value, prefix: &str) {
    let mut all = at(e, &format!("{ROUTES}/0")).clone();
    set(&mut all, "/match", json!({"prefix": prefix, "grpc": {}}));
    remove(&mut all, "/route", "retry_policy");
    at(e, ROUTES).as_array_mut().expect("routes").insert(0, all);
}

/// The rules the box shares with every route and cluster of an Envoy in front of kbf
/// (the routes, the stream bounds, the health checks, the TLS of the hop). The box's own
/// rules have their rows in `tests`.
const MUTANTS: &[Mutant] = &[
    m(
        &["route"],
        "WaitExecution not routed: Buck2's reconnect to a running action gets 404",
        |e, _| {
            at(e, ROUTES).as_array_mut().expect("routes").remove(1);
        },
    ),
    m(&["route"], "Execute not routed", |e, _| {
        at(e, ROUTES).as_array_mut().expect("routes").remove(0);
    }),
    m(
        &["idle-timeout"],
        "stream idle timeout under the 3900 s silent stream",
        |e, _| set(e, &format!("{HCM}/stream_idle_timeout"), json!("3600s")),
    ),
    m(
        &["idle-timeout"],
        "a route's own idle timeout cuts a silent stream after a minute",
        |e, _| set_new(e, &format!("{ROUTES}/2/route/idle_timeout"), json!("60s")),
    ),
    m(
        &["idle-timeout"],
        "an idle timeout of exactly the silent stream's 3900 s cuts it at its last second",
        |e, _| set_new(e, &format!("{ROUTES}/2/route/idle_timeout"), json!("3900s")),
    ),
    m(
        &["route-timeout"],
        "route timeout 4000 s, over the silent stream: it still ends every Execute 4000 s in",
        |e, _| set(e, &format!("{ROUTES}/0/route/timeout"), json!("4000s")),
    ),
    m(
        &["route-timeout"],
        "an earlier prefix route with the default 15 s timeout takes Execute: Envoy picks the first match, not the good route after it",
        |e, _| {
            let mut early = at(e, &format!("{ROUTES}/0")).clone();
            set(
                &mut early,
                "/match",
                json!({"prefix": "/build.bazel.remote.execution.v2.Execution/", "grpc": {}}),
            );
            remove(&mut early, "/route", "timeout");
            at(e, ROUTES)
                .as_array_mut()
                .expect("routes")
                .insert(0, early);
        },
    ),
    m(
        &["max-stream-duration"],
        "a route ends its streams at 300 s",
        |e, _| {
            set_new(
                e,
                &format!("{ROUTES}/0/route/max_stream_duration"),
                json!({"max_stream_duration": "300s"}),
            )
        },
    ),
    m(
        &["max-stream-duration"],
        "the upstream connections end every stream at 300 s",
        |e, _| {
            set_new(
                e,
                &format!("{H2_OPTIONS}/common_http_protocol_options"),
                json!({"max_stream_duration": "300.5s"}),
            )
        },
    ),
    m(
        &["route-match"],
        "a route also needs a header no client sends: its calls get 404",
        |e, _| {
            set_new(
                e,
                &format!("{ROUTES}/0/match/headers"),
                json!([{"name": "x-kbf-never", "present_match": true}]),
            )
        },
    ),
    m(
        &["route-match"],
        "a route matches on both a path and a prefix",
        |e, _| set_new(e, &format!("{ROUTES}/0/match/prefix"), json!("/build.")),
    ),
    m(
        &["route-match"],
        "a route's grpc match carries options the model does not read",
        |e, _| {
            set(
                e,
                &format!("{ROUTES}/0/match/grpc"),
                json!({"presented": true}),
            )
        },
    ),
    m(
        &["route-action"],
        "a route rewrites the path to /: the server sees no REAPI method",
        |e, _| set_new(e, &format!("{ROUTES}/0/route/prefix_rewrite"), json!("/")),
    ),
    m(
        &["no-catch-all"],
        "a catch-all route forwards any path to the servers",
        |e, _| catch_all(e, "/"),
    ),
    m(
        &["no-catch-all"],
        "a catch-all route on the empty prefix",
        |e, _| catch_all(e, ""),
    ),
    m(
        &["filters"],
        "a buffering filter in front of the router caps a 64 MiB write",
        |e, _| {
            let buffer = json!({"name": "envoy.filters.http.buffer", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.buffer.v3.Buffer", "max_request_bytes": 1_048_576}});
            at(e, &format!("{HCM}/http_filters"))
                .as_array_mut()
                .expect("filters")
                .insert(0, buffer);
        },
    ),
    m(
        &["filters"],
        "the router replaced by another filter: no call reaches a cluster",
        |e, _| {
            let buffer = "type.googleapis.com/envoy.extensions.filters.http.buffer.v3.Buffer";
            set(
                e,
                &format!("{HCM}/http_filters/0/typed_config/@type"),
                json!(buffer),
            )
        },
    ),
    m(
        &["vhost"],
        "the virtual host pins an authority a client does not send",
        |e, _| {
            set(
                e,
                &format!("{HCM}/route_config/virtual_hosts/0/domains"),
                json!(["kbf"]),
            )
        },
    ),
    m(&["vhost"], "a second virtual host", |e, _| {
        let v = at(e, &format!("{HCM}/route_config/virtual_hosts/0")).clone();
        push(e, &format!("{HCM}/route_config/virtual_hosts"), v);
    }),
    m(
        &["hcm"],
        "a second filter chain: which one serves a call is not what the check models",
        |e, _| {
            let chain = at(e, &format!("{REAPI}/filter_chains/0")).clone();
            push(e, &format!("{REAPI}/filter_chains"), chain);
        },
    ),
    m(&["hcm"], "no connection manager", |e, _| {
        set(e, &format!("{REAPI}/filter_chains/0/filters"), json!([]))
    }),
    m(&["codec"], "an HTTP/1 codec cannot take gRPC", |e, _| {
        set(e, &format!("{HCM}/codec_type"), json!("HTTP1"))
    }),
    m(
        &["route-cluster"],
        "a route sends to a cluster that is not defined",
        |e, _| set(e, &format!("{ROUTES}/0/route/cluster"), json!("other")),
    ),
    m(
        &["health-check-grpc"],
        "a health check that is not grpc.health.v1",
        |e, _| {
            remove(e, HEALTH, "grpc_health_check");
            set_new(
                e,
                &format!("{HEALTH}/http_health_check"),
                json!({"path": "/readyz"}),
            );
        },
    ),
    m(
        &["health-check-service"],
        "the health check asks for service \"kbf\": kbf-server answers NOT_FOUND, so no instance is ever healthy",
        |e, _| {
            set(
                e,
                &format!("{HEALTH}/grpc_health_check/service_name"),
                json!("kbf"),
            )
        },
    ),
    m(
        &["health-check-service"],
        "the health check's service name is not a string",
        |e, _| {
            set(
                e,
                &format!("{HEALTH}/grpc_health_check/service_name"),
                json!(7),
            )
        },
    ),
    m(
        &["health-check-tls"],
        "the health check picks its own transport socket",
        |e, _| {
            set_new(
                e,
                &format!("{HEALTH}/transport_socket_match_criteria"),
                json!({"server": "x"}),
            )
        },
    ),
    m(
        &["panic-threshold"],
        "panic threshold back to Envoy's default 50 %: an unhealthy instance still gets traffic",
        |e, _| remove(e, RC, "common_lb_config"),
    ),
    m(
        &["panic-threshold"],
        "panic threshold set to 50 %",
        |e, _| {
            set(
                e,
                &format!("{RC}/common_lb_config/healthy_panic_threshold/value"),
                json!(50.0),
            )
        },
    ),
    m(
        &["upstream-ca"],
        "the hop's default socket trusts the system CAs, not the settings' CA",
        |e, _| {
            set(
                e,
                &format!(
                    "{DEFAULT_SOCKET}/common_tls_context/validation_context/trusted_ca/filename"
                ),
                json!("/etc/ssl/certs/ca-certificates.crt"),
            )
        },
    ),
    m(
        &["upstream-san"],
        "any certificate from the CA is accepted for any instance",
        |e, _| {
            remove(
                e,
                &format!("{MATCH_SOCKET}/common_tls_context/validation_context"),
                "match_typed_subject_alt_names",
            )
        },
    ),
    m(
        &["upstream-tls"],
        "an endpoint without a match falls back to plain text",
        |e, _| remove(e, RC, "transport_socket"),
    ),
    m(
        &["endpoint-tls-match"],
        "an instance's endpoint matches no transport socket, so its name is never checked",
        |e, _| {
            set(
                e,
                &format!("{RC}/load_assignment/endpoints/0/lb_endpoints/0/metadata"),
                json!({}),
            )
        },
    ),
    m(
        &["upstream-alpn"],
        "the instance's TLS offers no h2",
        |e, _| {
            set(
                e,
                &format!("{MATCH_SOCKET}/common_tls_context/alpn_protocols"),
                json!(["http/1.1"]),
            )
        },
    ),
    m(
        &["upstream-sni"],
        "an instance is dialled with another name than its own",
        |e, _| set(e, &format!("{MATCH_SOCKET}/sni"), json!("other.example")),
    ),
    m(
        &["upstream-h2", "upstream-keepalive", "upstream-streams"],
        "the hop to the instances is HTTP/1 (and so has no HTTP/2 keepalive or stream limit)",
        |e, _| {
            set(
                e,
                &format!("{H2_OPTIONS}/explicit_http_config"),
                json!({"http_protocol_options": {}}),
            )
        },
    ),
    m(
        &["endpoints", "instances"],
        "a cluster with no instance",
        |e, _| {
            set(
                e,
                &format!("{RC}/load_assignment/endpoints/0/lb_endpoints"),
                json!([]),
            )
        },
    ),
    m(&["envoy-shape"], "a duration without its unit", |e, _| {
        set(e, &format!("{HCM}/stream_idle_timeout"), json!("7200"))
    }),
    m(
        &["envoy-shape"],
        "a route prefix that is not a string",
        |e, _| set(e, &format!("{ROUTES}/2/match/prefix"), json!(7)),
    ),
    m(
        &["envoy-shape"],
        "a cluster's max_stream_duration without its unit",
        |e, _| {
            set_new(
                e,
                &format!("{H2_OPTIONS}/common_http_protocol_options"),
                json!({"max_stream_duration": "300"}),
            )
        },
    ),
];

/// The shared rules' mutants: each planted defect makes exactly its checks fail. Catches
/// a check that cannot fail (its row passes) and a check that fires on a neighbour's
/// defect (the row names two checks, so the line would name the wrong cause).
#[test]
fn each_shared_rule_mutant_is_caught_by_exactly_its_checks() {
    let mut wrong = Vec::new();
    for mutant in MUTANTS {
        let r = rendered(&example());
        let mut envoy = parsed(&r);
        let mut args = r.args.clone();
        (mutant.plant)(&mut envoy, &mut args);
        assert!(
            envoy != parsed(&r) || args != r.args,
            "{}: the plant changed nothing",
            mutant.defect
        );
        let failures = run(&envoy, &args);
        for f in &failures {
            let line = f.line();
            let why = line.strip_prefix(&format!("FAIL {}: ", f.name()));
            assert!(why.is_some_and(|w| !w.is_empty()), "{line}");
        }
        let got: BTreeSet<&str> = failures.iter().map(CheckFailure::name).collect();
        let want: BTreeSet<&str> = mutant.checks.iter().copied().collect();
        if got != want {
            wrong.push(format!("{} : failed {got:?}", mutant.defect));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

/// The names of both mutant tables together: every check of [`CheckFailure::NAMES`] is
/// reached by some row, and no name is listed twice. Catches a check with no row.
#[test]
fn every_check_has_a_mutant_row() {
    let mut caught: BTreeSet<&str> = BTreeSet::new();
    for mutant in MUTANTS {
        caught.extend(mutant.checks.iter().copied());
    }
    caught.extend(super::tests::box_mutant_checks());
    let names: BTreeSet<&str> = CheckFailure::NAMES.into_iter().collect();
    assert_eq!(names.len(), CheckFailure::NAMES.len(), "a name twice");
    assert_eq!(caught, names, "a check no mutant reaches, or a stray name");
}

/// Catches a failure line that loses the check's name, the call it is about or the value
/// at fault: the line is all an operator reading the output sees.
#[test]
fn failure_lines_name_the_check_and_the_value_at_fault() {
    let r = rendered(&example());
    let mut e = parsed(&r);
    at(&mut e, ROUTES).as_array_mut().expect("routes").remove(1);
    remove(&mut e, &format!("{ROUTES}/0/route"), "timeout");
    remove(&mut e, RC, "common_lb_config");
    let lines: Vec<String> = run(&e, &r.args).iter().map(CheckFailure::line).collect();
    assert_eq!(
        lines,
        [
            "FAIL route-timeout: /build.bazel.remote.execution.v2.Execution/Execute is cut 15 s into its response (route timeout); it must be 0, with the idle timeout bounding silence",
            "FAIL route: no route for /build.bazel.remote.execution.v2.Execution/WaitExecution",
            "FAIL panic-threshold: cluster kbf_reapi: healthy_panic_threshold absent (Envoy's default 50) sends traffic to unhealthy servers",
        ]
    );
}

/// Catches a duration parser that takes a value Envoy refuses (no unit, no digits on a
/// side of the point, more than nine fractional digits, a sign, a number that is not a
/// string, seconds past u64) or reads a fraction at the wrong scale.
#[test]
fn durations_parse_as_protobuf_json_durations() {
    let ok = |s: &str| duration(&json!(s)).map_err(|Malformed(m)| m);
    assert_eq!(ok("7200s"), Ok(Duration::from_secs(7200)));
    assert_eq!(ok("0.5s"), Ok(Duration::from_millis(500)));
    assert_eq!(ok("1.000000001s"), Ok(Duration::new(1, 1)));
    for bad in [
        "7200",
        "s",
        ".5s",
        "1.s",
        "1.0000000001s",
        "-1s",
        "1e3s",
        "99999999999999999999s",
    ] {
        assert!(ok(bad).is_err(), "{bad} parsed");
    }
    let why = duration(&json!(7200)).err().map(|Malformed(m)| m);
    assert_eq!(why.as_deref(), Some("not a duration: 7200"));
}

/// Catches a message that prints a duration other than as seconds (a whole number with a
/// fraction, or a fraction with trailing zeros).
#[test]
fn durations_print_as_seconds() {
    assert_eq!(show(Duration::from_secs(15)), "15");
    assert_eq!(show(Duration::from_millis(1500)), "1.5");
    assert_eq!(show(Duration::new(0, 1)), "0.000000001");
}

/// Catches a truth test that differs from the first checker's (Python's) on any JSON
/// type: a transport socket of `{}` or `""` must read as none.
#[test]
fn truth_is_pythons() {
    for falsy in [
        json!(null),
        json!(false),
        json!(0),
        json!(0.0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        assert!(!truthy(&falsy), "{falsy}");
    }
    for t in [
        json!(true),
        json!(1),
        json!("x"),
        json!([0]),
        json!({"a": 0}),
    ] {
        assert!(truthy(&t), "{t}");
    }
}
