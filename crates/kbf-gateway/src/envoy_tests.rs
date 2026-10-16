use super::*;

fn options(upstream: Option<Upstream>) -> Options {
    Options {
        listen: "127.0.0.1:8980".parse().expect("address"),
        xds: "127.0.0.1:18000".parse().expect("address"),
        admin: "127.0.0.1:9901".parse().expect("address"),
        upstream,
    }
}

fn tls() -> Upstream {
    Upstream {
        ca: PathBuf::from("/etc/kbf/ca.pem"),
        san_suffix: ".farm.example".to_owned(),
    }
}

/// Catches: any change to the bootstrap Envoy is given, byte for byte, that is not made
/// to the golden file in the same change.
#[test]
fn the_plain_text_bootstrap_matches_its_golden() {
    assert_eq!(
        render(&options(None)),
        include_str!("../tests/golden/envoy_plain.json")
    );
}

/// Catches: the same, with upstream TLS; the name check missing from the TLS context.
#[test]
fn the_tls_bootstrap_matches_its_golden() {
    assert_eq!(
        render(&options(Some(tls()))),
        include_str!("../tests/golden/envoy_tls.json")
    );
}

/// Catches: the properties the golden must keep if someone regenerates it without
/// reading it: calls routed with no response timeout and a stream idle timeout above
/// the longest silent stream (3900 s), the leader endpoint from ADS and never from a
/// static list, the health check that guards a stale claim, and no panic routing to
/// unhealthy endpoints.
#[test]
fn the_bootstrap_keeps_what_reapi_streams_and_failover_need() {
    let config: Value = serde_json::from_str(&render(&options(Some(tls())))).expect("JSON");
    let manager = &config["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"];
    let idle = manager["stream_idle_timeout"]
        .as_str()
        .expect("idle timeout");
    let seconds: u64 = idle.trim_end_matches('s').parse().expect("seconds");
    assert!(seconds > 3900, "idle timeout {idle}");
    assert_eq!(
        manager["route_config"]["virtual_hosts"][0]["routes"][0]["route"]["timeout"],
        "0s"
    );
    let clusters = &config["static_resources"]["clusters"];
    let leader = clusters
        .as_array()
        .expect("clusters")
        .iter()
        .find(|c| c["name"] == CLUSTER)
        .expect("the leader cluster");
    assert_eq!(leader["type"], "EDS");
    assert!(leader["eds_cluster_config"]["eds_config"]["ads"].is_object());
    assert!(
        leader.get("load_assignment").is_none(),
        "no static endpoint"
    );
    assert!(leader["health_checks"][0]["grpc_health_check"].is_object());
    assert_eq!(
        leader["common_lb_config"]["healthy_panic_threshold"]["value"],
        0
    );
    assert_eq!(
        config["dynamic_resources"]["ads_config"]["grpc_services"][0]["envoy_grpc"]["cluster_name"],
        "kbf_xds"
    );
}
