//! The value mutants of `envoy check`: each plants one defect in the example's
//! rendered JSON and must be caught by exactly the check that names that defect:
//! caught by another check, the failure line would name the wrong cause; caught by
//! none, the check cannot fail. The helpers and the render are `check_tests`'.

use std::collections::BTreeSet;

use serde_json::json;

use super::check::{CheckFailure, check};
use super::check_tests::*;
use super::render::{CONFIG_HASH_ANNOTATION, sha256_hex};

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
        "idle-timeout",
        "an idle timeout of exactly the silent stream's 3900 s cuts it at its last second",
        |d| {
            envoy(d, |e| {
                set_new(e, &format!("{ROUTES}/0/route/idle_timeout"), json!("3900s"))
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
