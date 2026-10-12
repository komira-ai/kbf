//! `envoy check` on the example render and on renders Envoy reads the same way, its
//! failure lines and its helpers; and the helpers that plant a defect in a parsed
//! render, which the value mutants (`check_mutant_tests`) use.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::EnvoyConfig;
use std::time::Duration;

use super::check::{CheckFailure, Malformed, check, duration, show, truthy};
use super::render::{CONFIG_HASH_ANNOTATION, render, sha256_hex};

pub(super) const EXAMPLE: &str = include_str!("../../templates/envoy/config.example");
pub(super) const TWO: &str = include_str!("../../tests/golden/envoy/two-servers/config");
/// The example's `KBF_TS_OPERATOR_NAMESPACE`.
pub(super) const OPERATOR_NS: &str = "tailscale";

pub(super) const POD: &str = "/items/1/spec/template";
pub(super) const CONTAINER: &str = "/items/1/spec/template/spec/containers/0";
pub(super) const SERVICE: &str = "/items/2";
pub(super) const NETPOL: &str = "/items/3/spec";
pub(super) const INGRESS: &str = "/items/4/spec";
pub(super) const LISTENER: &str = "/static_resources/listeners/0";
pub(super) const HCM: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config";
pub(super) const ROUTES: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes";
pub(super) const CLUSTER: &str = "/static_resources/clusters/0";
pub(super) const HEALTH: &str = "/static_resources/clusters/0/health_checks/0";
pub(super) const SOCKET: &str =
    "/static_resources/clusters/0/transport_socket_matches/0/transport_socket/typed_config";
pub(super) const DEFAULT_SOCKET: &str =
    "/static_resources/clusters/0/transport_socket/typed_config";
pub(super) const ENDPOINT: &str =
    "/static_resources/clusters/0/load_assignment/endpoints/0/lb_endpoints/0";

pub(super) fn manifests(config: &str) -> Value {
    let c = EnvoyConfig::parse(config).expect("valid config");
    serde_json::from_str(&render(&c).manifests).expect("JSON")
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

/// Change the Envoy config the ConfigMap carries, then re-hash it into the pod
/// template as render does, so only the planted defect differs.
pub(super) fn envoy(doc: &mut Value, change: impl FnOnce(&mut Value)) {
    let text = at(doc, "/items/0/data/envoy.json")
        .as_str()
        .expect("text")
        .to_owned();
    let mut e: Value = serde_json::from_str(&text).expect("envoy JSON");
    change(&mut e);
    let text = serde_json::to_string_pretty(&e).expect("print");
    let hash = sha256_hex(&text);
    set(doc, "/items/0/data/envoy.json", Value::String(text));
    let annotations = format!("{POD}/metadata/annotations");
    let mut a = serde_json::Map::new();
    a.insert(CONFIG_HASH_ANNOTATION.to_owned(), Value::String(hash));
    set(doc, &annotations, Value::Object(a));
}

/// A change to a parsed render.
pub(super) type Plant = fn(&mut Value);

/// Set a field the render does not have.
pub(super) fn set_new(v: &mut Value, pointer: &str, value: Value) {
    let (parent, key) = pointer.rsplit_once('/').expect("a field");
    let fields = at(v, parent).as_object_mut().expect("an object");
    assert!(
        fields.insert(key.to_owned(), value).is_none(),
        "{pointer} already set"
    );
}

/// Catches a check that fails a correct render (it would then catch every mutant
/// in `check_mutant_tests` for nothing): the example and the two-server render pass
/// every check.
#[test]
fn the_example_and_two_server_renders_pass_every_check() {
    assert_eq!(check(&manifests(EXAMPLE), OPERATOR_NS), vec![]);
    assert_eq!(check(&manifests(TWO), OPERATOR_NS), vec![]);
}

/// Renders that differ from the example in ways Envoy reads the same: each must
/// still pass. Catches a check stricter than Envoy, which would refuse a correct
/// front: a cluster no route uses (only the used ones are checked), a CA Secret that
/// projects every key (no items), a trailing slash on a mount path, an explicit
/// HTTP2 codec or no codec field, timeouts written as `0s` or with a fraction, a panic
/// threshold written as `0.0`.
#[test]
fn renders_envoy_reads_the_same_way_pass() {
    let controls: [(&str, Plant); 9] = [
        ("a cluster no route uses, with no health check", |d| {
            envoy(d, |e| {
                let mut spare = at(e, CLUSTER).clone();
                set(&mut spare, "/name", json!("spare"));
                set(&mut spare, "/health_checks", json!([]));
                push(e, "/static_resources/clusters", spare);
            })
        }),
        ("secret without items", |d| {
            remove(d, &format!("{POD}/spec/volumes/1/secret"), "items")
        }),
        ("mount path with a trailing slash", |d| {
            set(
                d,
                &format!("{CONTAINER}/volumeMounts/0/mountPath"),
                json!("/etc/envoy/"),
            )
        }),
        ("codec HTTP2", |d| {
            envoy(d, |e| set(e, &format!("{HCM}/codec_type"), json!("HTTP2")))
        }),
        ("no codec field", |d| {
            envoy(d, |e| remove(e, HCM, "codec_type"))
        }),
        ("idle timeout disabled", |d| {
            envoy(d, |e| {
                set(e, &format!("{HCM}/stream_idle_timeout"), json!("0s"))
            })
        }),
        ("a route's idle timeout over the silent stream", |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{ROUTES}/0/route/idle_timeout"),
                    json!("3900.000000001s"),
                )
            })
        }),
        ("zero max stream duration and request timeout", |d| {
            envoy(d, |e| {
                set_new(e, &format!("{HCM}/request_timeout"), json!("0.000s"));
                set_new(
                    e,
                    &format!("{ROUTES}/0/route/max_stream_duration"),
                    json!({"max_stream_duration": "0s"}),
                );
            })
        }),
        ("panic threshold written as 0.0", |d| {
            let value = format!("{CLUSTER}/common_lb_config/healthy_panic_threshold/value");
            envoy(d, |e| set(e, &value, json!(0.0)))
        }),
    ];
    for (name, plant) in controls {
        let mut doc = manifests(EXAMPLE);
        plant(&mut doc);
        assert_eq!(check(&doc, OPERATOR_NS), vec![], "{name}");
    }
}

/// Catches a failure line that loses the check's name, the call it is about or the
/// value at fault: the line is all an operator reading the apply's output sees.
#[test]
fn failure_lines_name_the_check_and_the_value_at_fault() {
    let mut doc = manifests(EXAMPLE);
    envoy(&mut doc, |e| {
        at(e, ROUTES).as_array_mut().expect("routes").remove(1);
        remove(e, &format!("{ROUTES}/0/route"), "timeout");
        remove(e, CLUSTER, "common_lb_config");
    });
    let lines: Vec<String> = check(&doc, OPERATOR_NS)
        .iter()
        .map(CheckFailure::line)
        .collect();
    assert_eq!(
        lines,
        [
            "FAIL route-timeout: /build.bazel.remote.execution.v2.Execution/Execute is cut 15 s into its response (route timeout); it must be 0, with the idle timeout bounding silence",
            "FAIL route: no route for /build.bazel.remote.execution.v2.Execution/WaitExecution",
            "FAIL panic-threshold: cluster kbf_servers: healthy_panic_threshold absent (Envoy's default 50) sends traffic to unhealthy servers",
        ]
    );
}

/// Catches a duration parser that takes a value Envoy refuses (no unit, no digits
/// on a side of the point, more than nine fractional digits, a sign, a number
/// that is not a string, seconds past u64) or reads a fraction at the wrong scale.
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

/// Catches a message that prints a duration other than as seconds (a whole number
/// with a fraction, or a fraction with trailing zeros).
#[test]
fn durations_print_as_seconds() {
    assert_eq!(show(Duration::from_secs(15)), "15");
    assert_eq!(show(Duration::from_millis(1500)), "1.5");
    assert_eq!(show(Duration::new(0, 1)), "0.000000001");
}

/// Catches a truth test that differs from the first checker's (Python's) on any
/// JSON type: a transport socket of `{}` or `""` must read as none.
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

/// Catches a check that reads objects out of a document that is not a v1 List (a
/// lone object, or a List of another API's shape): it must find none, and say so
/// through the checks that need them, not pass.
#[test]
fn a_document_that_is_not_a_list_has_no_objects() {
    let mut doc = manifests(EXAMPLE);
    set(&mut doc, "/kind", json!("ConfigMapList"));
    let names: BTreeSet<&str> = check(&doc, OPERATOR_NS)
        .iter()
        .map(CheckFailure::name)
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["kinds", "namespace", "envoy-config"])
    );
}
