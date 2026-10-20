//! The check of a box's Envoy: the properties of the render ([`super::render`]) that keep a
//! long silent `Execute` alive and every instance reachable, read from `envoy.json` and
//! `envoy.args`.
//!
//! The connection manager, its routes and the REAPI cluster are held to the rules of
//! [`super::rules`]; this adds the box's own: the worker L4 path, the limits one open
//! stream per action needs, a health failure that leaves open streams alone, both
//! instances in both clusters, and the hot restart's timings. Each rule has a
//! [`CheckFailure`] of its own, listed in [`CheckFailure::NAMES`].

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::config::BoxSettings;
use super::render::{DOWNSTREAM_TLS, HCM, HTTP_OPTIONS, REAPI_CLUSTER, TCP_PROXY, WORKER_CLUSTER};
use super::rules::{
    Checker, Malformed, by_name, check_cluster, check_hcm, duration, endpoints, get, list, show,
    text, truthy,
};

use super::failure::CheckFailure;

/// Envoy's default `--drain-time-s`, when the flag is not given.
const DEFAULT_DRAIN_SECS: u64 = 600;
/// Envoy's default `--parent-shutdown-time-s`, when the flag is not given.
const DEFAULT_PARENT_SHUTDOWN_SECS: u64 = 900;

/// What the render is checked against: the settings' numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spec {
    /// The CA file the upstream TLS must trust.
    ca_file: String,
    /// The longest action the farm runs.
    longest_action: Duration,
    /// The concurrent streams the REAPI cluster must hold open.
    stream_ceiling: u64,
    /// The streams one upstream connection may carry.
    max_streams: u32,
}

impl From<&BoxSettings> for Spec {
    fn from(c: &BoxSettings) -> Spec {
        Spec {
            ca_file: c.ca_file.clone(),
            longest_action: Duration::from_secs(c.longest_action_secs),
            stream_ceiling: u64::from(c.stream_ceiling),
            max_streams: c.reapi_max_streams,
        }
    }
}

/// Every failure of the render of `settings`: `envoy` is `envoy.json` parsed, `args` the
/// text of `envoy.args`. The settings are not validated here (the render's caller has);
/// a number outside its range only makes the failures it names.
pub(super) fn check_box(envoy: &Value, args: &str, settings: &BoxSettings) -> Vec<CheckFailure> {
    let spec = Spec::from(settings);
    let mut c = Checker::default();
    if let Err(Malformed(why)) = check_envoy(&mut c, envoy, &spec) {
        c.failures.push(CheckFailure::EnvoyShape { why });
    }
    check_args(&mut c, args, &spec);
    c.failures
}

fn check_envoy(c: &mut Checker, envoy: &Value, spec: &Spec) -> Result<(), Malformed> {
    let address = &envoy["admin"]["address"]["socket_address"]["address"];
    c.require(*address == "127.0.0.1", || CheckFailure::AdminLoopback {
        address: text(address),
    });
    let resources = &envoy["static_resources"];
    let clusters = by_name(
        list(&resources["clusters"])
            .iter()
            .map(|cl| (&cl["name"], cl)),
    );
    let listeners = list(&resources["listeners"]);
    let named = |n: &str| listeners.iter().find(|l| l["name"] == n);
    let names: Vec<&Value> = listeners.iter().map(|l| &l["name"]).collect();
    let (reapi, worker) = (named("reapi"), named("worker"));
    let ok = listeners.len() == 2 && reapi.is_some() && worker.is_some();
    c.require(ok, || CheckFailure::Listeners {
        got: json!(names).to_string(),
    });
    for (name, l) in [("REAPI", reapi), ("worker", worker)] {
        if let Some(l) = l {
            // Explicit even though Linux's default is on: it is what lets a new Envoy
            // bind the same ports while the old one drains (the installer's swap).
            c.require(l["enable_reuse_port"] == true, || CheckFailure::ReusePort {
                listener: name.to_owned(),
            });
        }
    }
    if let Some(l) = reapi {
        check_reapi_listener(c, l, &clusters)?;
    }
    if let Some(l) = worker {
        check_worker_listener(c, l);
    }
    let ca = spec.ca_file.as_str();
    let defined = |name: &str| get(&clusters, &json!(name));
    match defined(REAPI_CLUSTER) {
        Some(cl) => {
            check_cluster(c, cl, REAPI_CLUSTER, ca)?;
            check_limits(c, cl, REAPI_CLUSTER, spec, true);
            let opts = &cl["typed_extension_protocol_options"][HTTP_OPTIONS];
            let ka =
                &opts["explicit_http_config"]["http2_protocol_options"]["connection_keepalive"];
            let on = match (ka.get("interval"), ka.get("timeout")) {
                (Some(i), Some(t)) => !duration(i)?.is_zero() && !duration(t)?.is_zero(),
                _ => false,
            };
            c.require(on, || CheckFailure::UpstreamKeepalive {
                cluster: REAPI_CLUSTER.to_owned(),
            });
            let streams =
                &opts["explicit_http_config"]["http2_protocol_options"]["max_concurrent_streams"];
            c.require(
                streams
                    .as_u64()
                    .is_some_and(|n| (1..=u64::from(spec.max_streams)).contains(&n)),
                || CheckFailure::UpstreamStreams {
                    cluster: REAPI_CLUSTER.to_owned(),
                    got: if streams.is_null() {
                        "absent".to_owned()
                    } else {
                        text(streams)
                    },
                    want: spec.max_streams.to_string(),
                },
            );
        }
        None => c.failures.push(CheckFailure::RouteCluster {
            path: "the REAPI listener".to_owned(),
            cluster: REAPI_CLUSTER.to_owned(),
        }),
    }
    match defined(WORKER_CLUSTER) {
        Some(cl) => check_worker_cluster(c, cl, spec),
        None => c.failures.push(CheckFailure::WorkerListener {
            why: format!("no cluster {WORKER_CLUSTER}"),
        }),
    }
    check_ports(c, envoy);
    Ok(())
}

fn check_reapi_listener(
    c: &mut Checker,
    l: &Value,
    clusters: &[(&Value, &Value)],
) -> Result<(), Malformed> {
    let chains = list(&l["filter_chains"]);
    let hcms: Vec<&Value> = chains
        .iter()
        .flat_map(|ch| list(&ch["filters"]))
        .map(|f| &f["typed_config"])
        .filter(|t| t["@type"] == HCM)
        .collect();
    let ([chain], [hcm]) = (chains, hcms.as_slice()) else {
        c.failures.push(CheckFailure::Hcm);
        return Ok(());
    };
    let tls = &chain["transport_socket"]["typed_config"];
    let ctx = &tls["common_tls_context"];
    let h2 = list(&ctx["alpn_protocols"]).iter().any(|p| *p == "h2");
    let ok = tls["@type"] == DOWNSTREAM_TLS && !list(&ctx["tls_certificates"]).is_empty() && h2;
    c.require(ok, || CheckFailure::ListenerTls);
    check_hcm(c, hcm, clusters)?;
    Ok(())
}

/// The worker listener is a `tcp_proxy` to the worker cluster whose idle timeout is
/// off, and does not end the daemons' mutual TLS.
fn check_worker_listener(c: &mut Checker, l: &Value) {
    let chains = list(&l["filter_chains"]);
    let [chain] = chains else {
        c.failures.push(CheckFailure::WorkerListener {
            why: format!("{} filter chains", chains.len()),
        });
        return;
    };
    c.require(chain.get("transport_socket").is_none(), || {
        CheckFailure::WorkerTls {
            at: "the worker listener".to_owned(),
        }
    });
    let filters = list(&chain["filters"]);
    let proxy = match filters {
        [f] if f["typed_config"]["@type"] == TCP_PROXY => &f["typed_config"],
        _ => {
            c.failures.push(CheckFailure::WorkerListener {
                why: format!("{} filters", filters.len()),
            });
            return;
        }
    };
    c.require(proxy["cluster"] == WORKER_CLUSTER, || {
        CheckFailure::WorkerListener {
            why: format!("it proxies to {}", text(&proxy["cluster"])),
        }
    });
    let idle = proxy.get("idle_timeout");
    let off = idle.is_some_and(|d| duration(d).is_ok_and(|d| d.is_zero()));
    c.require(off, || CheckFailure::WorkerIdle {
        got: idle.map_or_else(|| "absent (Envoy's default 1 h)".to_owned(), text),
    });
}

/// The settings both clusters share: an unready instance leaves open streams alone, the
/// limits are over what the farm holds open, and both instances are in.
fn check_limits(c: &mut Checker, cl: &Value, name: &str, spec: &Spec, reapi: bool) {
    let cluster = || name.to_owned();
    c.require(
        !truthy(&cl["close_connections_on_host_health_failure"]),
        || CheckFailure::HealthKeepsStreams { cluster: cluster() },
    );
    let thresholds = list(&cl["circuit_breakers"]["thresholds"]);
    let default = thresholds
        .iter()
        .find(|t| t.get("priority").is_none_or(|p| *p == "DEFAULT"));
    let value = |key: &str| default.and_then(|t| t[key].as_u64());
    let ceiling = spec.stream_ceiling;
    let connections = ceiling.div_ceil(u64::from(spec.max_streams));
    let mut wants = vec![("max_connections", if reapi { connections } else { ceiling })];
    if reapi {
        wants.extend([
            ("max_requests", ceiling),
            ("max_pending_requests", ceiling),
            ("max_retries", ceiling),
        ]);
    }
    for (key, want) in wants {
        let got = value(key);
        c.require(got.is_some_and(|g| g >= want), || CheckFailure::Breakers {
            cluster: cluster(),
            why: format!(
                "{key} is {} on the DEFAULT priority, under the {want} the farm needs",
                got.map_or_else(|| "absent (Envoy's default)".to_owned(), |g| g.to_string())
            ),
        });
    }
    let endpoints = endpoints(cl);
    let ports: Vec<String> = endpoints
        .iter()
        .map(|e| text(&e["endpoint"]["address"]["socket_address"]["port_value"]))
        .collect();
    let mut distinct = ports.clone();
    distinct.sort();
    distinct.dedup();
    let why = if endpoints.len() < 2 {
        Some(format!("{} instances, not both", endpoints.len()))
    } else if distinct.len() < ports.len() {
        Some("one instance listed twice".to_owned())
    } else {
        None
    };
    if let Some(why) = why {
        c.failures.push(CheckFailure::Instances {
            cluster: cluster(),
            why,
        });
    }
}

/// The worker cluster: raw TCP (no TLS of Envoy's), health by the instance's `/readyz`
/// on its API port.
fn check_worker_cluster(c: &mut Checker, cl: &Value, spec: &Spec) {
    check_limits(c, cl, WORKER_CLUSTER, spec, false);
    let tls = truthy(&cl["transport_socket"]) || truthy(&cl["transport_socket_matches"]);
    c.require(!tls, || CheckFailure::WorkerTls {
        at: format!("cluster {WORKER_CLUSTER}"),
    });
    let panic = &cl["common_lb_config"]["healthy_panic_threshold"]["value"];
    let hcs = list(&cl["health_checks"]);
    let http = |hc: &Value| hc["http_health_check"]["path"] == "/readyz";
    // A port equal to the instance's own worker port is a port used twice, which
    // `check_ports` reports.
    let own_port = |e: &Value| e["endpoint"]["health_check_config"]["port_value"].is_u64();
    let why = if panic.as_f64() != Some(0.0) {
        Some("healthy_panic_threshold is not 0, so unready instances get traffic")
    } else if hcs.is_empty() || !hcs.iter().all(http) {
        Some("the health check is not an HTTP check of /readyz")
    } else if !endpoints(cl).into_iter().all(own_port) {
        Some("an instance has no health_check_config port, so its worker port is checked")
    } else {
        None
    };
    if let Some(why) = why {
        c.failures.push(CheckFailure::WorkerHealth {
            why: why.to_owned(),
        });
    }
}

/// No port is used twice across the listeners, the admin interface and every instance's
/// worker, REAPI and API ports.
fn check_ports(c: &mut Checker, envoy: &Value) {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut note = |c: &mut Checker, port: &Value, who: String| {
        let port = text(port);
        if let Some(first) = seen.insert(port.clone(), who.clone()) {
            c.failures.push(CheckFailure::InstancePorts {
                port,
                first,
                second: who,
            });
        }
    };
    let resources = &envoy["static_resources"];
    note(
        c,
        &envoy["admin"]["address"]["socket_address"]["port_value"],
        "the admin interface".to_owned(),
    );
    for l in list(&resources["listeners"]) {
        let port = &l["address"]["socket_address"]["port_value"];
        note(c, port, format!("listener {}", text(&l["name"])));
    }
    for cl in list(&resources["clusters"]) {
        for (i, e) in endpoints(cl).into_iter().enumerate() {
            let port = &e["endpoint"]["address"]["socket_address"]["port_value"];
            note(c, port, format!("{} endpoint {i}", text(&cl["name"])));
            if let Some(h) = e["endpoint"]["health_check_config"].get("port_value") {
                note(c, h, format!("{} endpoint {i} health", text(&cl["name"])));
            }
        }
    }
}

/// Envoy's hot-restart flags, one `FLAG VALUE` per line.
fn check_args(c: &mut Checker, args: &str, spec: &Spec) {
    let flag = |name: &str| {
        args.lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    };
    let secs = |v: Option<&str>, default: u64| match v {
        None => Some(default),
        Some(v) => v.parse::<u64>().ok(),
    };
    let parent = secs(
        flag("--parent-shutdown-time-s"),
        DEFAULT_PARENT_SHUTDOWN_SECS,
    );
    let want = spec.longest_action;
    let shown = || {
        flag("--parent-shutdown-time-s")
            .unwrap_or("absent (Envoy's default 900)")
            .to_owned()
    };
    c.require(parent.is_some_and(|p| p >= want.as_secs()), || {
        CheckFailure::ParentShutdown {
            got: shown(),
            want: show(want),
        }
    });
    let drain = secs(flag("--drain-time-s"), DEFAULT_DRAIN_SECS);
    // With no readable parent shutdown the other rule has already failed.
    let below = match (drain, parent) {
        (None, _) => false,
        (Some(d), Some(p)) => d < p,
        (Some(_), None) => true,
    };
    c.require(below, || CheckFailure::DrainTime {
        got: flag("--drain-time-s")
            .unwrap_or("absent (Envoy's default 600)")
            .to_owned(),
        parent: shown(),
    });
}
