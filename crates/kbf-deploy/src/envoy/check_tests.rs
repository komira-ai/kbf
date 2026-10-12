//! `envoy check` on the example render and on value mutants of it. Each mutant
//! plants one defect in the rendered JSON and must be caught by exactly the check
//! that names that defect: caught by another check, the failure line would name the
//! wrong cause; caught by none, the check cannot fail.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::EnvoyConfig;
use std::time::Duration;

use super::check::{CheckFailure, Malformed, check, duration, show, truthy};
use super::render::{CONFIG_HASH_ANNOTATION, render, sha256_hex};

const EXAMPLE: &str = include_str!("../../templates/envoy/config.example");
const TWO: &str = include_str!("../../tests/golden/envoy/two-servers/config");
/// The example's `KBF_TS_OPERATOR_NAMESPACE`.
const OPERATOR_NS: &str = "tailscale";

const POD: &str = "/items/1/spec/template";
const CONTAINER: &str = "/items/1/spec/template/spec/containers/0";
const SERVICE: &str = "/items/2";
const NETPOL: &str = "/items/3/spec";
const INGRESS: &str = "/items/4/spec";
const LISTENER: &str = "/static_resources/listeners/0";
const HCM: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config";
const ROUTES: &str = "/static_resources/listeners/0/filter_chains/0/filters/0/typed_config/route_config/virtual_hosts/0/routes";
const CLUSTER: &str = "/static_resources/clusters/0";
const HEALTH: &str = "/static_resources/clusters/0/health_checks/0";
const SOCKET: &str =
    "/static_resources/clusters/0/transport_socket_matches/0/transport_socket/typed_config";
const DEFAULT_SOCKET: &str = "/static_resources/clusters/0/transport_socket/typed_config";
const ENDPOINT: &str = "/static_resources/clusters/0/load_assignment/endpoints/0/lb_endpoints/0";

fn manifests(config: &str) -> Value {
    let c = EnvoyConfig::parse(config).expect("valid config");
    serde_json::from_str(&render(&c).manifests).expect("JSON")
}

fn at<'a>(v: &'a mut Value, pointer: &str) -> &'a mut Value {
    v.pointer_mut(pointer)
        .unwrap_or_else(|| panic!("no {pointer} in the render"))
}

fn set(v: &mut Value, pointer: &str, value: Value) {
    *at(v, pointer) = value;
}

fn remove(v: &mut Value, pointer: &str, key: &str) {
    let removed = at(v, pointer).as_object_mut().and_then(|o| o.remove(key));
    assert!(removed.is_some(), "no {key} at {pointer}");
}

fn push(v: &mut Value, pointer: &str, item: Value) {
    at(v, pointer).as_array_mut().expect("a list").push(item);
}

/// Change the Envoy config the ConfigMap carries, then re-hash it into the pod
/// template as render does, so only the planted defect differs.
fn envoy(doc: &mut Value, change: impl FnOnce(&mut Value)) {
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
type Plant = fn(&mut Value);

/// One planted defect and the one check that must catch it.
struct Mutant {
    check: &'static str,
    defect: &'static str,
    plant: Plant,
}

const fn m(check: &'static str, defect: &'static str, plant: Plant) -> Mutant {
    Mutant {
        check,
        defect,
        plant,
    }
}

/// The value mutants. Rows carried over from the first checker's mutants (a defect
/// planted in its renderer) come first; then one or more rows per remaining check, so
/// every check is shown able to fail.
const MUTANTS: &[Mutant] = &[
    m(
        "route",
        "WaitExecution not routed: Buck2's reconnect to a running action gets 404",
        |d| {
            envoy(d, |e| {
                at(e, ROUTES).as_array_mut().expect("routes").remove(1);
            })
        },
    ),
    m("route", "Execute not routed", |d| {
        envoy(d, |e| {
            at(e, ROUTES).as_array_mut().expect("routes").remove(0);
        })
    }),
    m(
        "route",
        "ByteStream not routed: no blob over 4 MiB moves",
        |d| {
            envoy(d, |e| {
                at(e, ROUTES).as_array_mut().expect("routes").remove(5);
            })
        },
    ),
    m(
        "idle-timeout",
        "stream idle timeout back to Envoy's default 300 s: a silent Execute is cut",
        |d| envoy(d, |e| remove(e, HCM, "stream_idle_timeout")),
    ),
    m(
        "idle-timeout",
        "stream idle timeout under the 3900 s silent stream",
        |d| {
            envoy(d, |e| {
                set(e, &format!("{HCM}/stream_idle_timeout"), json!("3600s"))
            })
        },
    ),
    m(
        "idle-timeout",
        "a route's own idle timeout cuts a silent Execute after a minute",
        |d| {
            envoy(d, |e| {
                set_new(e, &format!("{ROUTES}/0/route/idle_timeout"), json!("60s"))
            })
        },
    ),
    m(
        "route-timeout",
        "route timeout back to Envoy's default 15 s: every Execute is cut",
        |d| envoy(d, |e| remove(e, &format!("{ROUTES}/0/route"), "timeout")),
    ),
    m(
        "route-timeout",
        "route timeout 4000 s, over the silent stream: it still ends every Execute 4000 s into its response",
        |d| {
            envoy(d, |e| {
                set(e, &format!("{ROUTES}/0/route/timeout"), json!("4000s"))
            })
        },
    ),
    m(
        "max-stream-duration",
        "the connection manager ends every stream at 300 s",
        |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{HCM}/common_http_protocol_options"),
                    json!({"max_stream_duration": "300s"}),
                )
            })
        },
    ),
    m(
        "max-stream-duration",
        "a route ends its streams at 300 s",
        |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{ROUTES}/0/route/max_stream_duration"),
                    json!({"max_stream_duration": "300s"}),
                )
            })
        },
    ),
    m(
        "max-stream-duration",
        "the upstream connections end every stream at 300 s",
        |d| {
            envoy(d, |e| {
                let opts = format!(
                    "{CLUSTER}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/common_http_protocol_options"
                );
                set_new(e, &opts, json!({"max_stream_duration": "300.5s"}));
            })
        },
    ),
    m(
        "request-timeout",
        "a 300 s request timeout cuts a ByteStream Write still uploading",
        |d| {
            envoy(d, |e| {
                set_new(e, &format!("{HCM}/request_timeout"), json!("300s"))
            })
        },
    ),
    m(
        "route-match",
        "a route also needs a header no client sends: its calls get 404",
        |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{ROUTES}/0/match/headers"),
                    json!([{"name": "x-kbf-never", "present_match": true}]),
                )
            })
        },
    ),
    m(
        "route-match",
        "a route matches on both a path and a prefix",
        |d| {
            envoy(d, |e| {
                set_new(e, &format!("{ROUTES}/0/match/prefix"), json!("/build."))
            })
        },
    ),
    m(
        "route-match",
        "a route's grpc match carries options the model does not read",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{ROUTES}/0/match/grpc"),
                    json!({"presented": true}),
                )
            })
        },
    ),
    m(
        "route-action",
        "a route rewrites the path to /: the server sees no REAPI method",
        |d| {
            envoy(d, |e| {
                set_new(e, &format!("{ROUTES}/0/route/prefix_rewrite"), json!("/"))
            })
        },
    ),
    m(
        "health-check",
        "no health check: Envoy sends to a server that is down or not the leader",
        |d| {
            envoy(d, |e| {
                set(e, &format!("{CLUSTER}/health_checks"), json!([]))
            })
        },
    ),
    m(
        "health-check-grpc",
        "a health check that is not grpc.health.v1",
        |d| {
            envoy(d, |e| {
                remove(e, HEALTH, "grpc_health_check");
                set_new(
                    e,
                    &format!("{HEALTH}/http_health_check"),
                    json!({"path": "/readyz"}),
                );
            })
        },
    ),
    m(
        "health-check-service",
        "the health check asks for service \"kbf\": kbf-server answers NOT_FOUND, so no server is ever healthy",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{HEALTH}/grpc_health_check/service_name"),
                    json!("kbf"),
                )
            })
        },
    ),
    m(
        "health-check-service",
        "the health check's service name is not a string",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{HEALTH}/grpc_health_check/service_name"),
                    json!(7),
                )
            })
        },
    ),
    m(
        "health-check-tls",
        "the health check picks its own transport socket",
        |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{HEALTH}/transport_socket_match_criteria"),
                    json!({"server": "x"}),
                )
            })
        },
    ),
    m(
        "panic-threshold",
        "panic threshold back to Envoy's default 50 %: an unhealthy server still gets traffic",
        |d| envoy(d, |e| remove(e, CLUSTER, "common_lb_config")),
    ),
    m("panic-threshold", "panic threshold set to 50 %", |d| {
        envoy(d, |e| {
            set(
                e,
                &format!("{CLUSTER}/common_lb_config/healthy_panic_threshold/value"),
                json!(50.0),
            )
        })
    }),
    m(
        "upstream-ca",
        "the hop trusts the system CAs, not the internal CA",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!(
                        "{DEFAULT_SOCKET}/common_tls_context/validation_context/trusted_ca/filename"
                    ),
                    json!("/etc/ssl/certs/ca-certificates.crt"),
                )
            })
        },
    ),
    m(
        "upstream-ca",
        "the Secret's ca.pem lands at ca.crt: trusted_ca names a file the pod lacks and Envoy does not start",
        |d| {
            set(
                d,
                &format!("{POD}/spec/volumes/1/secret/items/0/path"),
                json!("ca.crt"),
            )
        },
    ),
    m(
        "upstream-ca",
        "the Secret projects no ca.pem item: the pod has no CA file at all",
        |d| {
            set(
                d,
                &format!("{POD}/spec/volumes/1/secret/items/0/key"),
                json!("ca.crt"),
            )
        },
    ),
    m(
        "upstream-san",
        "any certificate from the CA is accepted for any server",
        |d| {
            envoy(d, |e| {
                remove(
                    e,
                    &format!("{SOCKET}/common_tls_context/validation_context"),
                    "match_typed_subject_alt_names",
                )
            })
        },
    ),
    m(
        "endpoint-tls-match",
        "a server's endpoint matches no transport socket, so its name is never checked",
        |d| envoy(d, |e| set(e, &format!("{ENDPOINT}/metadata"), json!({}))),
    ),
    m(
        "endpoint-tls-match",
        "a server's endpoint names a transport socket match that does not exist",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!(
                        "{ENDPOINT}/metadata/filter_metadata/envoy.transport_socket_match/server"
                    ),
                    json!("other"),
                )
            })
        },
    ),
    m(
        "upstream-tls",
        "an endpoint without a match falls back to plain text",
        |d| envoy(d, |e| remove(e, CLUSTER, "transport_socket")),
    ),
    m(
        "no-catch-all",
        "a catch-all route forwards any path to the servers",
        |d| {
            envoy(d, |e| {
                let mut all = at(e, &format!("{ROUTES}/0")).clone();
                set(&mut all, "/match", json!({"prefix": "/", "grpc": {}}));
                at(e, ROUTES).as_array_mut().expect("routes").insert(0, all);
            })
        },
    ),
    m(
        "filters",
        "a buffering filter in front of the router caps a 64 MiB write",
        |d| {
            envoy(d, |e| {
                let buffer = json!({"name": "envoy.filters.http.buffer", "typed_config": {"@type": "type.googleapis.com/envoy.extensions.filters.http.buffer.v3.Buffer", "max_request_bytes": 1_048_576}});
                at(e, &format!("{HCM}/http_filters"))
                    .as_array_mut()
                    .expect("filters")
                    .insert(0, buffer);
            })
        },
    ),
    m(
        "listener-address",
        "Envoy listens on 127.0.0.1: the Service cannot reach it",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{LISTENER}/address/socket_address/address"),
                    json!("127.0.0.1"),
                )
            })
        },
    ),
    m(
        "netpol-port",
        "the NetworkPolicy admits the admin port, not Envoy's: the proxy is blocked",
        |d| set(d, &format!("{NETPOL}/ingress/0/ports/0/port"), json!(9901)),
    ),
    m(
        "netpol-from",
        "the NetworkPolicy admits the front's own namespace, not the operator's: the proxy is blocked",
        |d| {
            set(
                d,
                &format!(
                    "{NETPOL}/ingress/0/from/0/namespaceSelector/matchLabels/kubernetes.io~1metadata.name"
                ),
                json!("kbf-front"),
            )
        },
    ),
    m(
        "netpol-pods",
        "the NetworkPolicy selects pods that are not Envoy's: Envoy is open to every pod",
        |d| {
            set(
                d,
                &format!("{NETPOL}/podSelector/matchLabels"),
                json!({"app.kubernetes.io/name": "kbf-server"}),
            )
        },
    ),
    m(
        "netpol-pods",
        "the NetworkPolicy is not an ingress policy, so it admits nothing it names",
        |d| set(d, &format!("{NETPOL}/policyTypes"), json!(["Egress"])),
    ),
    m(
        "netpol-rule",
        "an empty ingress rule beside the operator's admits every source",
        |d| {
            at(d, &format!("{NETPOL}/ingress"))
                .as_array_mut()
                .expect("rules")
                .insert(0, json!({}))
        },
    ),
    m(
        "ingress-cleartext",
        "the Service on 443: the operator dials Envoy with TLS (https+insecure)",
        |d| {
            set(d, &format!("{SERVICE}/spec/ports/0/port"), json!(443));
            set(
                d,
                &format!("{INGRESS}/defaultBackend/service/port/number"),
                json!(443),
            );
        },
    ),
    m(
        "ingress-cleartext",
        "the Service port named https: the operator dials Envoy with TLS",
        |d| set(d, &format!("{SERVICE}/spec/ports/0/name"), json!("https")),
    ),
    m(
        "config-hash",
        "the pod template's hash does not follow the config: an edit does not roll Envoy",
        |d| {
            set(
                d,
                &format!(
                    "{POD}/metadata/annotations/{}",
                    CONFIG_HASH_ANNOTATION.replace('/', "~1")
                ),
                json!(sha256_hex("")),
            )
        },
    ),
    m("image-pinned", "the image is not pinned by digest", |d| {
        set(
            d,
            &format!("{CONTAINER}/image"),
            json!("envoyproxy/envoy:distroless-v1.39.3"),
        )
    }),
    m(
        "image-pinned",
        "the digest is upper-case hex, which no registry resolves",
        |d| {
            let image = format!("envoyproxy/envoy@sha256:{}", "A".repeat(64));
            set(d, &format!("{CONTAINER}/image"), json!(image))
        },
    ),
    // Rows for the checks the first checker's mutants did not plant.
    m(
        "kinds",
        "an object twice: kubectl applies the second over the first",
        |d| {
            let cm = at(d, "/items/0").clone();
            push(d, "/items", cm);
        },
    ),
    m(
        "namespace",
        "the Ingress in another namespace than the Service it sends to",
        |d| set(d, "/items/4/metadata/namespace", json!("other")),
    ),
    m(
        "envoy-config",
        "the ConfigMap's envoy.json is not JSON: Envoy does not start",
        |d| set(d, "/items/0/data/envoy.json", json!("{")),
    ),
    m("envoy-config", "the ConfigMap carries no envoy.json", |d| {
        set(d, "/items/0/data", json!({}))
    }),
    m("envoy-shape", "a duration without its unit", |d| {
        envoy(d, |e| {
            set(e, &format!("{HCM}/stream_idle_timeout"), json!("7200"))
        })
    }),
    m("envoy-shape", "a route prefix that is not a string", |d| {
        envoy(d, |e| set(e, &format!("{ROUTES}/2/match/prefix"), json!(7)))
    }),
    m(
        "ingress-class",
        "not the tailscale IngressClass: the operator ignores the Ingress",
        |d| set(d, &format!("{INGRESS}/ingressClassName"), json!("nginx")),
    ),
    m(
        "ingress-host",
        "a dotted tls host: the operator wants one MagicDNS label",
        |d| {
            set(
                d,
                &format!("{INGRESS}/tls/0/hosts"),
                json!(["kbf.tail0000.ts.net"]),
            )
        },
    ),
    m(
        "ingress-host",
        "two tls hosts: the operator takes the first",
        |d| set(d, &format!("{INGRESS}/tls/0/hosts"), json!(["kbf", "kbf2"])),
    ),
    m(
        "ingress-backend",
        "the Ingress sends to another Service than Envoy's",
        |d| {
            set(
                d,
                &format!("{INGRESS}/defaultBackend/service/name"),
                json!("other"),
            )
        },
    ),
    m(
        "ingress-port",
        "the Ingress sends to a port the Service does not have",
        |d| {
            set(
                d,
                &format!("{INGRESS}/defaultBackend/service/port/number"),
                json!(9999),
            )
        },
    ),
    m("container", "a second container in the Envoy pod", |d| {
        let c = at(d, CONTAINER).clone();
        push(d, &format!("{POD}/spec/containers"), c);
    }),
    m(
        "service-selector",
        "the Service selects pods that are not Envoy's",
        |d| {
            set(
                d,
                &format!("{SERVICE}/spec/selector"),
                json!({"app.kubernetes.io/name": "kbf-server"}),
            )
        },
    ),
    m("service-selector", "the Service selects nothing", |d| {
        set(d, &format!("{SERVICE}/spec/selector"), json!({}))
    }),
    m(
        "config-mount",
        "Envoy reads a config path the ConfigMap is not mounted at",
        |d| {
            set(
                d,
                &format!("{CONTAINER}/args/1"),
                json!("/etc/other/envoy.json"),
            )
        },
    ),
    m("config-mount", "no volume holds the ConfigMap", |d| {
        set(
            d,
            &format!("{POD}/spec/volumes/0/configMap/name"),
            json!("other"),
        )
    }),
    m(
        "ca-mount",
        "no Secret volume: the internal CA is not in the pod",
        |d| {
            at(d, &format!("{POD}/spec/volumes"))
                .as_array_mut()
                .expect("volumes")
                .remove(1);
        },
    ),
    m(
        "ca-mount",
        "the CA volume is not mounted into the container",
        |d| {
            at(d, &format!("{CONTAINER}/volumeMounts"))
                .as_array_mut()
                .expect("mounts")
                .remove(1);
        },
    ),
    m("listener", "a second listener", |d| {
        envoy(d, |e| {
            let l = at(e, LISTENER).clone();
            push(e, "/static_resources/listeners", l);
        })
    }),
    m(
        "listener",
        "no listener: nothing takes the Service's traffic",
        |d| envoy(d, |e| set(e, "/static_resources/listeners", json!([]))),
    ),
    m(
        "listener-port",
        "Envoy listens on another port than the Service's targetPort",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{LISTENER}/address/socket_address/port_value"),
                    json!(9090),
                )
            })
        },
    ),
    m(
        "hcm",
        "a second filter chain: which one serves a call is not what the check models",
        |d| {
            envoy(d, |e| {
                let ch = at(e, &format!("{LISTENER}/filter_chains/0")).clone();
                push(e, &format!("{LISTENER}/filter_chains"), ch);
            })
        },
    ),
    m(
        "listener-plaintext",
        "the listener expects TLS from a proxy that forwards cleartext",
        |d| {
            envoy(d, |e| {
                set_new(
                    e,
                    &format!("{LISTENER}/filter_chains/0/transport_socket"),
                    json!({"name": "envoy.transport_sockets.tls"}),
                )
            })
        },
    ),
    m("codec", "an HTTP/1 codec cannot take gRPC", |d| {
        envoy(d, |e| set(e, &format!("{HCM}/codec_type"), json!("HTTP1")))
    }),
    m(
        "vhost",
        "the virtual host pins an authority the proxy does not send",
        |d| {
            envoy(d, |e| {
                set(
                    e,
                    &format!("{HCM}/route_config/virtual_hosts/0/domains"),
                    json!(["kbf"]),
                )
            })
        },
    ),
    m("vhost", "a second virtual host", |d| {
        envoy(d, |e| {
            let v = at(e, &format!("{HCM}/route_config/virtual_hosts/0")).clone();
            push(e, &format!("{HCM}/route_config/virtual_hosts"), v);
        })
    }),
    m(
        "route-cluster",
        "a route sends to a cluster that is not defined",
        |d| {
            envoy(d, |e| {
                set(e, &format!("{ROUTES}/0/route/cluster"), json!("other"))
            })
        },
    ),
    m("upstream-h2", "the hop to the servers is HTTP/1", |d| {
        envoy(d, |e| {
            let explicit = format!(
                "{CLUSTER}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config"
            );
            set(e, &explicit, json!({"http_protocol_options": {}}));
        })
    }),
    m("upstream-alpn", "the server's TLS offers no h2", |d| {
        envoy(d, |e| {
            set(
                e,
                &format!("{SOCKET}/common_tls_context/alpn_protocols"),
                json!(["http/1.1"]),
            )
        })
    }),
    m(
        "upstream-sni",
        "the server is dialled with another name than its own",
        |d| {
            envoy(d, |e| {
                set(e, &format!("{SOCKET}/sni"), json!("other.example"))
            })
        },
    ),
    m("endpoints", "a cluster with no server", |d| {
        envoy(d, |e| {
            set(
                e,
                &format!("{CLUSTER}/load_assignment/endpoints/0/lb_endpoints"),
                json!([]),
            )
        })
    }),
    m("image-pinned", "a digest one hex digit short", |d| {
        let image = format!("envoyproxy/envoy@sha256:{}", "a".repeat(63));
        set(d, &format!("{CONTAINER}/image"), json!(image))
    }),
    m(
        "filters",
        "the router replaced by another filter: no call reaches a cluster",
        |d| {
            let router = format!("{HCM}/http_filters/0/typed_config/@type");
            let buffer = "type.googleapis.com/envoy.extensions.filters.http.buffer.v3.Buffer";
            envoy(d, |e| set(e, &router, json!(buffer)))
        },
    ),
    m(
        "no-catch-all",
        "a catch-all route on the empty prefix",
        |d| {
            envoy(d, |e| {
                let mut all = at(e, &format!("{ROUTES}/0")).clone();
                set(&mut all, "/match", json!({"prefix": "", "grpc": {}}));
                at(e, ROUTES).as_array_mut().expect("routes").insert(0, all);
            })
        },
    ),
    m(
        "envoy-shape",
        "a cluster's max_stream_duration without its unit",
        |d| {
            envoy(d, |e| {
                let opts = format!(
                    "{CLUSTER}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/common_http_protocol_options"
                );
                set_new(e, &opts, json!({"max_stream_duration": "300"}));
            })
        },
    ),
];

/// Set a field the render does not have.
fn set_new(v: &mut Value, pointer: &str, value: Value) {
    let (parent, key) = pointer.rsplit_once('/').expect("a field");
    let fields = at(v, parent).as_object_mut().expect("an object");
    assert!(
        fields.insert(key.to_owned(), value).is_none(),
        "{pointer} already set"
    );
}

/// Catches a check that fails a correct render (it would then catch every mutant
/// below for nothing): the example and the two-server render pass every check.
#[test]
fn the_example_and_two_server_renders_pass_every_check() {
    assert_eq!(check(&manifests(EXAMPLE), OPERATOR_NS), vec![]);
    assert_eq!(check(&manifests(TWO), OPERATOR_NS), vec![]);
}

/// The value mutants: each planted defect makes exactly its check fail. Catches a
/// check that cannot fail (its row passes), a check that fires on a neighbour's
/// defect (the row names two checks, so the line would name the wrong cause), and a
/// check with no row at all (every name of `CheckFailure::NAMES` must be caught by
/// some row).
#[test]
fn each_value_mutant_is_caught_by_exactly_its_check() {
    let mut wrong = Vec::new();
    let mut caught = BTreeSet::new();
    for mutant in MUTANTS {
        let mut doc = manifests(EXAMPLE);
        (mutant.plant)(&mut doc);
        let failures = check(&doc, OPERATOR_NS);
        for f in &failures {
            let line = f.line();
            let why = line.strip_prefix(&format!("FAIL {}: ", f.name()));
            assert!(why.is_some_and(|w| !w.is_empty()), "{line}");
        }
        let got: BTreeSet<&str> = failures.iter().map(CheckFailure::name).collect();
        if got != BTreeSet::from([mutant.check]) {
            wrong.push(format!(
                "{} ({}): failed {got:?}",
                mutant.check, mutant.defect
            ));
        }
        caught.extend(got);
    }
    assert_eq!(wrong, Vec::<String>::new());
    let names: BTreeSet<&str> = CheckFailure::NAMES.into_iter().collect();
    assert_eq!(names.len(), CheckFailure::NAMES.len(), "a name twice");
    assert_eq!(caught, names, "a check no mutant reaches");
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
