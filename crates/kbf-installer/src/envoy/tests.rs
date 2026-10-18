//! The box's settings, its golden render, the library's files and errors, and the box's
//! own checks on the example render and on one planted defect at a time. The checks the
//! box shares with any Envoy in front of kbf are `check_tests`'.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::check::check_box;
use super::check_tests::{
    HCM, PROXY, Plant, RC, REAPI, ROUTES, WC, WORKER, at, example, parsed, push, remove, rendered,
    run, set, set_new,
};
use super::config::{BoxSettings, SettingsError};
use super::failure::CheckFailure;
use super::render::Rendered;
use super::{ENVOY, ENVOY_ARGS, EnvoyError, check, check_dir, render, write_files};

const GOLDEN_ENVOY: &str = include_str!("../../tests/golden/envoy/box/envoy.json");
const GOLDEN_ARGS: &str = include_str!("../../tests/golden/envoy/box/envoy.args");

/// The example with one field changed.
fn with(change: impl FnOnce(&mut BoxSettings)) -> BoxSettings {
    let mut s = example();
    change(&mut s);
    s
}

fn refusal(settings: &BoxSettings) -> String {
    settings.validate().expect_err("refused").to_string()
}

/// A fresh directory for one test.
fn scratch(name: &str) -> std::path::PathBuf {
    crate::fixtures::scratch_dir(&format!("envoy-{name}"))
}

/// Catches drift in the render: a key, a value or an order changed. The golden is the
/// bytes `kbf-deploy envoy render --topology box` wrote for these numbers (copied from
/// that tool's tests; this renderer is a port of it), validated by Envoy itself
/// (`tests/envoy_validate.rs`), so it pins what was reviewed rather than proves it right.
#[test]
fn the_example_renders_the_golden_bytes() {
    let r = rendered(&example());
    assert_eq!(r.envoy, GOLDEN_ENVOY);
    assert_eq!(r.args, GOLDEN_ARGS);
}

/// Catches a render that hard-codes a number the settings give: every port, the SNI name,
/// the files, the breaker limits and the parent shutdown follow the settings.
#[test]
fn the_settings_reach_the_render() {
    let s = BoxSettings {
        host: "farm.box.test".to_owned(),
        reapi_port: 7000,
        worker_port: 7001,
        admin_port: 7002,
        backend_port_base: 30000,
        cert_file: "/srv/pki/reapi.pem".to_owned(),
        key_file: "/srv/pki/reapi.key".to_owned(),
        ca_file: "/srv/pki/ca.pem".to_owned(),
        reapi_max_streams: 500,
        stream_ceiling: 4000,
        longest_action_secs: 100,
    };
    let r = rendered(&s);
    for want in [
        "\"farm.box.test\"",
        "\"port_value\": 7000",
        "\"port_value\": 7001",
        "\"port_value\": 7002",
        "\"port_value\": 30000",
        "\"port_value\": 30001",
        "\"port_value\": 30002",
        "\"port_value\": 30010",
        "\"port_value\": 30012",
        "/srv/pki/reapi.pem",
        "/srv/pki/reapi.key",
        "/srv/pki/ca.pem",
        "\"max_requests\": 4000",
        "\"max_connections\": 16",
    ] {
        assert!(r.envoy.contains(want), "{want}");
    }
    assert!(r.args.starts_with("--parent-shutdown-time-s 1000\n"));
    assert_eq!(check_box(&parsed(&r), &r.args, &s), vec![]);
}

/// Catches a check that fails a correct render (it would then catch every mutant for
/// nothing).
#[test]
fn the_example_passes_every_box_check() {
    let r = rendered(&example());
    assert_eq!(run(&parsed(&r), &r.args), vec![]);
}

/// Catches settings that accept what the render cannot use: each row is one refusal, and
/// names the field at fault.
#[test]
fn the_settings_refuse_what_the_render_cannot_use() {
    assert_eq!(example().instances()[1].api, 18992);
    let cases: Vec<(BoxSettings, &str)> = vec![
        (with(|s| s.reapi_port = 0), "reapi_port=\"0\""),
        (with(|s| s.worker_port = 0), "worker_port=\"0\""),
        (with(|s| s.admin_port = 0), "admin_port=\"0\""),
        (with(|s| s.backend_port_base = 0), "backend_port_base=\"0\""),
        (with(|s| s.reapi_max_streams = 0), "reapi_max_streams=\"0\""),
        (
            with(|s| s.reapi_max_streams = 32768),
            "reapi_max_streams=\"32768\"",
        ),
        (with(|s| s.stream_ceiling = 0), "stream_ceiling=\"0\""),
        (
            with(|s| s.stream_ceiling = 100_000_001),
            "stream_ceiling=\"100000001\"",
        ),
        (
            with(|s| s.longest_action_secs = 0),
            "longest_action_secs=\"0\"",
        ),
        (
            with(|s| s.longest_action_secs = 31_536_001),
            "longest_action_secs=\"31536001\"",
        ),
        (with(|s| s.host = "Not_A_Name".to_owned()), "host="),
        (with(|s| s.host = String::new()), "host=\"\""),
        (with(|s| s.host = "a..b".to_owned()), "host=\"a..b\""),
        (with(|s| s.host = "-a.b".to_owned()), "host=\"-a.b\""),
        (with(|s| s.host = format!("{}.b", "a".repeat(64))), "host="),
        (
            with(|s| s.cert_file = "relative/pem".to_owned()),
            "cert_file=\"relative/pem\"",
        ),
        (
            with(|s| s.key_file = "/a\"b".to_owned()),
            "key_file=\"/a\\\"b\"",
        ),
        (
            with(|s| s.ca_file = "/has space/ca.pem".to_owned()),
            "ca_file=\"/has space/ca.pem\"",
        ),
        (with(|s| s.ca_file = "/back\\slash".to_owned()), "ca_file="),
        (
            with(|s| s.admin_port = 8980),
            "reapi_port and admin_port are both port 8980",
        ),
        (
            with(|s| s.backend_port_base = 8978),
            "reapi_port and instance a API are both port 8980",
        ),
        (with(|s| s.backend_port_base = 65530), "run past 65535"),
    ];
    for (settings, want) in cases {
        let got = refusal(&settings);
        assert!(got.contains(want), "{got} lacks {want}");
    }
    assert_eq!(
        with(|s| s.backend_port_base = 65530).validate(),
        Err(SettingsError::BasePastRange(65530))
    );
    // One past the highest base that fits: the second instance's API port would be 65536.
    assert_eq!(
        with(|s| s.backend_port_base = 65524).validate(),
        Err(SettingsError::BasePastRange(65524))
    );
    // The highest base that fits: the second instance's API port is 65535.
    let top = with(|s| s.backend_port_base = 65523);
    assert_eq!(top.validate(), Ok(()));
    assert_eq!(top.instances()[1].api, 65535);
    // A base past the range saturates instead of overflowing.
    assert_eq!(
        with(|s| s.backend_port_base = 65535).instances()[1].reapi,
        65535
    );
}

/// Catches a render or a check that proceeds on settings the render cannot use: both
/// say why instead.
#[test]
fn render_and_check_refuse_unusable_settings() {
    let bad = with(|s| s.reapi_port = 0);
    assert!(matches!(
        render(&bad),
        Err(SettingsError::Field {
            field: "reapi_port",
            ..
        })
    ));
    let good = rendered(&example());
    let err = check(&bad, &good).expect_err("refused");
    assert!(
        matches!(err, EnvoyError::Settings(SettingsError::Field { .. })),
        "{err}"
    );
    assert!(err.to_string().contains("reapi_port"), "{err}");
}

/// Catches an example that is not the one-host default the docs show: both ends of the
/// parent shutdown arithmetic.
#[test]
fn the_parent_shutdown_is_the_longest_action_plus_a_quarter_hour() {
    assert_eq!(example().parent_shutdown_secs(), 21600 + 900);
    // A year-long action does not overflow the sum.
    let long = with(|s| s.longest_action_secs = u64::MAX);
    assert_eq!(long.parent_shutdown_secs(), u64::MAX);
}

/// Catches a library that checks what it did not write, or writes what it cannot read
/// back: `write_files` then `check_dir` passes the example, `check` on the texts agrees,
/// and the files are the golden bytes.
#[test]
fn written_files_are_the_golden_bytes_and_pass_the_check() {
    let dir = scratch("write").join("slot/new");
    let r = rendered(&example());
    write_files(&dir, &r).expect("written");
    assert_eq!(
        std::fs::read_to_string(dir.join(ENVOY)).expect("envoy.json"),
        GOLDEN_ENVOY
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(ENVOY_ARGS)).expect("envoy.args"),
        GOLDEN_ARGS
    );
    assert_eq!(check_dir(&example(), &dir).expect("checked"), vec![]);
    assert_eq!(check(&example(), &r).expect("checked"), vec![]);
}

/// Catches a check of a directory that passes a render with a defect in the file on disk
/// (it must read the files, not re-render), and one that reports a defective `args` file.
#[test]
fn the_check_of_a_directory_reads_its_files() {
    let dir = scratch("defect");
    let r = rendered(&example());
    let mut envoy = parsed(&r);
    remove(&mut envoy, &format!("{ROUTES}/0/route"), "timeout");
    let planted = Rendered {
        envoy: serde_json::to_string_pretty(&envoy).expect("print"),
        args: r.args.replace("22500", "900"),
    };
    write_files(&dir, &planted).expect("written");
    let names: Vec<&str> = check_dir(&example(), &dir)
        .expect("checked")
        .iter()
        .map(CheckFailure::name)
        .collect();
    assert_eq!(names, ["route-timeout", "parent-shutdown"]);
}

/// Catches an I/O or parse failure reported as a pass, or without the path at fault.
#[test]
fn files_that_cannot_be_read_written_or_parsed_are_errors_with_their_path() {
    let dir = scratch("errors");
    let missing = dir.join("none");
    let err = check_dir(&example(), &missing).expect_err("no files");
    assert!(matches!(err, EnvoyError::Read { .. }));
    assert!(err.to_string().contains("none/envoy.json"), "{err}");
    // Only the args file is missing.
    std::fs::create_dir_all(&missing).expect("dir");
    std::fs::write(missing.join(ENVOY), "{}").expect("envoy.json");
    let err = check_dir(&example(), &missing).expect_err("no args");
    assert!(err.to_string().contains("envoy.args"), "{err}");
    // Not JSON, from a directory and from texts.
    std::fs::write(missing.join(ENVOY_ARGS), "").expect("envoy.args");
    std::fs::write(missing.join(ENVOY), "{").expect("envoy.json");
    let err = check_dir(&example(), &missing).expect_err("not JSON");
    assert!(matches!(err, EnvoyError::Parse { .. }));
    assert!(
        err.to_string().contains("none/envoy.json is not JSON"),
        "{err}"
    );
    let texts = Rendered {
        envoy: "{".to_owned(),
        args: String::new(),
    };
    let err = check(&example(), &texts).expect_err("not JSON");
    assert!(
        err.to_string().starts_with("envoy.json is not JSON"),
        "{err}"
    );
    // A path that is a file cannot hold the files; a file cannot be replaced by a
    // directory.
    let file = dir.join("a-file");
    std::fs::write(&file, "").expect("file");
    let r = rendered(&example());
    let err = write_files(&file, &r).expect_err("a file is not a directory");
    assert!(matches!(err, EnvoyError::Write { .. }));
    assert!(err.to_string().contains("a-file"), "{err}");
    let blocked = dir.join("blocked");
    std::fs::create_dir_all(blocked.join(ENVOY)).expect("a directory named envoy.json");
    let err = write_files(&blocked, &r).expect_err("envoy.json is a directory");
    assert!(err.to_string().contains("envoy.json"), "{err}");
}

/// The box's names the mutant table below reaches, for `check_tests`' count of checks
/// with a row.
pub(super) fn box_mutant_checks() -> BTreeSet<&'static str> {
    MUTANTS.iter().map(|m| m.check).collect()
}

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

const MUTANTS: &[Mutant] = &[
    m(
        "route-timeout",
        "Execute's route timeout back to Envoy's 15 s",
        |e, _| remove(e, &format!("{ROUTES}/0/route"), "timeout"),
    ),
    m(
        "max-stream-duration",
        "a 300 s cap on every stream's life",
        |e, _| {
            set_new(
                e,
                &format!("{HCM}/common_http_protocol_options"),
                json!({"max_stream_duration": "300s"}),
            )
        },
    ),
    m(
        "idle-timeout",
        "the stream idle timeout back to Envoy's 300 s",
        |e, _| remove(e, HCM, "stream_idle_timeout"),
    ),
    m(
        "execute-idle-off",
        "Execute's own idle timeout left to the connection manager's",
        |e, _| remove(e, &format!("{ROUTES}/0/route"), "idle_timeout"),
    ),
    m(
        "execute-idle-off",
        "WaitExecution's idle timeout 2 h",
        |e, _| set(e, &format!("{ROUTES}/1/route/idle_timeout"), json!("7200s")),
    ),
    m(
        "breakers",
        "no circuit breakers on the REAPI cluster: about a thousand actions",
        |e, _| remove(e, RC, "circuit_breakers"),
    ),
    m(
        "breakers",
        "max_connections under ceiling / max_concurrent_streams",
        |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/max_connections"),
                json!(1),
            )
        },
    ),
    m("breakers", "max_requests under the ceiling", |e, _| {
        set(
            e,
            &format!("{RC}/circuit_breakers/thresholds/0/max_requests"),
            json!(1024),
        )
    }),
    m(
        "breakers",
        "the limits on a HIGH priority threshold only, which does not apply",
        |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/priority"),
                json!("HIGH"),
            )
        },
    ),
    m(
        "breakers",
        "no limits on the worker cluster: 1024 connections",
        |e, _| remove(e, WC, "circuit_breakers"),
    ),
    m(
        "health-keeps-streams",
        "a failed check closes the REAPI cluster's connections",
        |e, _| {
            set(
                e,
                &format!("{RC}/close_connections_on_host_health_failure"),
                json!(true),
            )
        },
    ),
    m(
        "health-keeps-streams",
        "a failed check closes a daemon's session",
        |e, _| {
            set(
                e,
                &format!("{WC}/close_connections_on_host_health_failure"),
                json!(true),
            )
        },
    ),
    m(
        "worker-idle",
        "the worker proxy's idle timeout left at Envoy's 1 h",
        |e, _| remove(e, PROXY, "idle_timeout"),
    ),
    m(
        "worker-idle",
        "the worker proxy's idle timeout 1 h written out",
        |e, _| set(e, &format!("{PROXY}/idle_timeout"), json!("3600s")),
    ),
    m("retry-routes", "a retry policy on ByteStream", |e, _| {
        set_new(
            e,
            &format!("{ROUTES}/5/route/retry_policy"),
            json!({"retry_on": "connect-failure"}),
        )
    }),
    m("retry-routes", "a retry on unavailable", |e, _| {
        set(
            e,
            &format!("{ROUTES}/0/route/retry_policy/retry_on"),
            json!("connect-failure,unavailable"),
        )
    }),
    m(
        "retry-routes",
        "a retry policy with status codes the check does not model",
        |e, _| {
            set_new(
                e,
                &format!("{ROUTES}/0/route/retry_policy/retriable_status_codes"),
                json!([503]),
            )
        },
    ),
    m(
        "instances",
        "instance b missing from the REAPI cluster",
        |e, _| {
            at(e, &format!("{RC}/load_assignment/endpoints/0/lb_endpoints"))
                .as_array_mut()
                .expect("endpoints")
                .pop();
        },
    ),
    m(
        "instances",
        "instance b missing from the worker cluster",
        |e, _| {
            at(e, &format!("{WC}/load_assignment/endpoints/0/lb_endpoints"))
                .as_array_mut()
                .expect("endpoints")
                .pop();
        },
    ),
    m(
        "instance-ports",
        "instance b's worker port is instance a's REAPI port",
        |e, _| {
            set(
                e,
                &format!(
                    "{WC}/load_assignment/endpoints/0/lb_endpoints/1/endpoint/address/socket_address/port_value"
                ),
                json!(18980),
            )
        },
    ),
    m(
        "instance-ports",
        "the admin port is the public worker port",
        |e, _| set(e, "/admin/address/socket_address/port_value", json!(8981)),
    ),
    m(
        "parent-shutdown",
        "the default 900 s parent shutdown",
        |_, a| {
            *a = a.replace(
                "--parent-shutdown-time-s 22500\n",
                "--parent-shutdown-time-s 900\n",
            );
        },
    ),
    m("parent-shutdown", "no parent shutdown flag", |_, a| {
        *a = a.replace("--parent-shutdown-time-s 22500\n", "");
    }),
    m(
        "parent-shutdown",
        "a parent shutdown that is not a number",
        |_, a| {
            *a = a.replace("22500", "soon");
        },
    ),
    m(
        "drain-time",
        "a drain time over the parent shutdown",
        |_, a| {
            *a = a.replace("--drain-time-s 60\n", "--drain-time-s 30000\n");
        },
    ),
    m("drain-time", "a drain time that is not a number", |_, a| {
        *a = a.replace("--drain-time-s 60\n", "--drain-time-s soon\n");
    }),
    m(
        "admin-loopback",
        "the admin interface on every address",
        |e, _| set(e, "/admin/address/socket_address/address", json!("0.0.0.0")),
    ),
    m("listeners", "the worker listener missing", |e, _| {
        at(e, "/static_resources/listeners")
            .as_array_mut()
            .expect("listeners")
            .pop();
    }),
    m("listeners", "a third listener", |e, _| {
        let mut extra = at(e, WORKER).clone();
        set(&mut extra, "/name", json!("spare"));
        set(
            &mut extra,
            "/address/socket_address/port_value",
            json!(7777),
        );
        push(e, "/static_resources/listeners", extra);
    }),
    m(
        "listener-tls",
        "the REAPI listener in plain text",
        |e, _| remove(e, &format!("{REAPI}/filter_chains/0"), "transport_socket"),
    ),
    m(
        "listener-tls",
        "the REAPI listener with TLS but no certificate",
        |e, _| {
            set(
                e,
                &format!(
                    "{REAPI}/filter_chains/0/transport_socket/typed_config/common_tls_context/tls_certificates"
                ),
                json!([]),
            )
        },
    ),
    m(
        "worker-listener",
        "the worker listener proxies to the REAPI cluster",
        |e, _| set(e, &format!("{PROXY}/cluster"), json!("kbf_reapi")),
    ),
    m(
        "worker-listener",
        "the worker listener has two filters",
        |e, _| {
            let extra = at(e, &format!("{WORKER}/filter_chains/0/filters/0")).clone();
            push(e, &format!("{WORKER}/filter_chains/0/filters"), extra);
        },
    ),
    m(
        "worker-listener",
        "the worker listener has two chains",
        |e, _| {
            let extra = at(e, &format!("{WORKER}/filter_chains/0")).clone();
            push(e, &format!("{WORKER}/filter_chains"), extra);
        },
    ),
    m(
        "worker-tls",
        "Envoy ends the daemons' mutual TLS",
        |e, _| {
            set_new(
                e,
                &format!("{WORKER}/filter_chains/0/transport_socket"),
                json!({"name": "envoy.transport_sockets.tls"}),
            )
        },
    ),
    m("worker-tls", "the worker cluster dials with TLS", |e, _| {
        set_new(
            e,
            &format!("{WC}/transport_socket"),
            json!({"name": "envoy.transport_sockets.tls"}),
        )
    }),
    m(
        "worker-health",
        "no health check on the worker cluster",
        |e, _| set(e, &format!("{WC}/health_checks"), json!([])),
    ),
    m(
        "worker-health",
        "a TCP connect check of the worker port",
        |e, _| {
            set(
                e,
                &format!("{WC}/health_checks/0"),
                json!({"tcp_health_check": {}}),
            )
        },
    ),
    m(
        "worker-health",
        "an instance with no health port: its worker port would be checked",
        |e, _| {
            remove(
                e,
                &format!("{WC}/load_assignment/endpoints/0/lb_endpoints/0/endpoint"),
                "health_check_config",
            )
        },
    ),
    m(
        "worker-health",
        "the worker cluster sends to unhealthy instances",
        |e, _| {
            remove(
                e,
                &format!("{WC}/common_lb_config"),
                "healthy_panic_threshold",
            )
        },
    ),
    m(
        "upstream-keepalive",
        "no HTTP/2 keepalive upstream",
        |e, _| {
            remove(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options"
                ),
                "connection_keepalive",
            )
        },
    ),
    m(
        "upstream-streams",
        "no max_concurrent_streams upstream: Envoy assumes unlimited until SETTINGS arrive",
        |e, _| {
            remove(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options"
                ),
                "max_concurrent_streams",
            )
        },
    ),
    m(
        "upstream-streams",
        "max_concurrent_streams above kbf's limit",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options/max_concurrent_streams"
                ),
                json!(10001),
            )
        },
    ),
    m(
        "health-check",
        "no health check on the REAPI cluster",
        |e, _| set(e, &format!("{RC}/health_checks"), json!([])),
    ),
    m(
        "endpoint-tls-match",
        "an instance with no socket match: its name is not checked",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/load_assignment/endpoints/0/lb_endpoints/1/metadata/filter_metadata/envoy.transport_socket_match/server"
                ),
                json!("other"),
            )
        },
    ),
    m(
        "upstream-ca",
        "the upstream trusts a CA other than the config's",
        |e, _| {
            set(
                e,
                &format!(
                    "{RC}/transport_socket_matches/0/transport_socket/typed_config/common_tls_context/validation_context/trusted_ca/filename"
                ),
                json!("/etc/ssl/other.pem"),
            )
        },
    ),
    m("route", "ByteStream not routed", |e, _| {
        at(e, ROUTES).as_array_mut().expect("routes").remove(5);
    }),
    m(
        "request-timeout",
        "a request timeout cuts a 64 MiB write",
        |e, _| set_new(e, &format!("{HCM}/request_timeout"), json!("60s")),
    ),
];

/// The box mutants: each planted defect makes exactly its check fail. Catches a check
/// that cannot fail and a check that fires on a neighbour's defect (the line would name
/// the wrong cause). A check with no row at all is `check_tests`' to catch.
#[test]
fn each_box_mutant_is_caught_by_exactly_its_check() {
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
        if got != BTreeSet::from([mutant.check]) {
            wrong.push(format!(
                "{} ({}): failed {got:?}",
                mutant.check, mutant.defect
            ));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

/// Renders Envoy reads the same way as the example: each must still pass. Catches a
/// check stricter than Envoy, which would refuse a correct box: an absent
/// close_connections flag (Envoy's default is false), an absent priority (DEFAULT), a
/// route idle timeout written with a fraction, a retry list in the other order, a
/// larger limit, a larger drain gap, extra whitespace-free flags.
#[test]
fn renders_envoy_reads_the_same_way_pass() {
    let controls: [(&str, Plant); 7] = [
        ("close_connections flag absent", |e, _| {
            remove(e, RC, "close_connections_on_host_health_failure")
        }),
        ("breaker priority absent", |e, _| {
            remove(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0"),
                "priority",
            )
        }),
        ("a bigger request limit", |e, _| {
            set(
                e,
                &format!("{RC}/circuit_breakers/thresholds/0/max_requests"),
                json!(1_000_000),
            )
        }),
        ("a retry list in the other order", |e, _| {
            set(
                e,
                &format!("{ROUTES}/0/route/retry_policy/retry_on"),
                json!("refused-stream,connect-failure"),
            )
        }),
        ("a route without a retry policy", |e, _| {
            remove(e, &format!("{ROUTES}/2/route"), "retry_policy")
        }),
        ("parent shutdown exactly the longest action", |_, a| {
            *a = a.replace("22500", "21600");
        }),
        ("a longer drain, still under the parent shutdown", |_, a| {
            *a = a.replace("--drain-time-s 60", "--drain-time-s 600");
        }),
    ];
    for (name, plant) in controls {
        let r = rendered(&example());
        let (mut envoy, mut args) = (parsed(&r), r.args.clone());
        plant(&mut envoy, &mut args);
        assert_eq!(run(&envoy, &args), vec![], "{name}");
    }
}

/// Catches a check that reads the REAPI cluster's endpoints by address, as the front
/// does: the box's addresses are loopback for both instances, and the name checked is
/// the hostname.
#[test]
fn a_box_server_name_is_checked_by_hostname() {
    let r = rendered(&example());
    let mut e = parsed(&r);
    set(
        &mut e,
        &format!("{RC}/transport_socket_matches/0/transport_socket/typed_config/sni"),
        json!("127.0.0.1"),
    );
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert_eq!(failures, ["upstream-sni", "upstream-sni"]);
}

/// Catches a check that reads the first of two clusters with one name: Envoy refuses a
/// config that names a cluster twice, and the model reads the name as a Python dict does,
/// by its last entry, so a defect in the last copy is the one reported (and the ports of
/// both are counted).
#[test]
fn a_cluster_named_twice_is_read_as_its_last() {
    let r = rendered(&example());
    let mut e = parsed(&r);
    let mut last = at(&mut e, RC).clone();
    set(
        &mut last,
        "/close_connections_on_host_health_failure",
        json!(true),
    );
    push(&mut e, "/static_resources/clusters", last);
    let names: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert_eq!(
        names,
        ["health-keeps-streams", "instance-ports", "instance-ports"]
    );
}

/// Catches a host name check that refuses a name with a hyphen inside a label, or a
/// dotted name of several labels, which a farm's name commonly is.
#[test]
fn host_names_with_hyphens_and_labels_are_accepted() {
    for host in ["kbf-1.farm-a.example", "localhost", "a-b-c"] {
        assert_eq!(
            with(|s| s.host = host.to_owned()).validate(),
            Ok(()),
            "{host}"
        );
    }
}

/// Catches a duration the box checks cannot read stopping silently instead of being
/// reported as a malformed config.
#[test]
fn an_unreadable_duration_is_a_shape_failure() {
    let r = rendered(&example());
    let mut e = parsed(&r);
    set(&mut e, &format!("{PROXY}/idle_timeout"), json!("forever"));
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert!(failures.contains(&"worker-idle"), "{failures:?}");
    set(
        &mut e,
        &format!(
            "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options/connection_keepalive/interval"
        ),
        json!("often"),
    );
    let failures: Vec<&str> = run(&e, &r.args).iter().map(CheckFailure::name).collect();
    assert!(failures.contains(&"envoy-shape"), "{failures:?}");
}

/// Catches a check that stops at, or skips, a part the render lacks: each plant removes or
/// breaks one part and the failure that names it must be reported (and the check must
/// not panic on the rest). Defects: a listener not named `reapi` or `worker`; the REAPI
/// or worker cluster missing; no HTTP connection manager; a worker filter that is not a
/// `tcp_proxy`; an upstream keepalive with a zero interval, a zero timeout or neither; both
/// instances on one port; one instance only; a retry policy with no `retry_on`.
#[test]
fn a_part_the_render_lacks_is_a_named_failure() {
    let keepalive = format!(
        "{RC}/typed_extension_protocol_options/envoy.extensions.upstreams.http.v3.HttpProtocolOptions/explicit_http_config/http2_protocol_options/connection_keepalive"
    );
    let endpoints = format!("{RC}/load_assignment/endpoints/0/lb_endpoints");
    let cases: Vec<(&str, &str, Plant)> = vec![
        ("listeners", "no worker listener", |e, _| {
            set(e, &format!("{WORKER}/name"), json!("other"))
        }),
        ("listeners", "no reapi listener", |e, _| {
            set(e, &format!("{REAPI}/name"), json!("other"))
        }),
        ("route-cluster", "no REAPI cluster", |e, _| {
            set(e, &format!("{RC}/name"), json!("other"))
        }),
        ("worker-listener", "no worker cluster", |e, _| {
            set(e, &format!("{WC}/name"), json!("other"))
        }),
        ("hcm", "no connection manager", |e, _| {
            set(e, &format!("{REAPI}/filter_chains/0/filters"), json!([]))
        }),
        ("worker-listener", "not a tcp_proxy", |e, _| {
            set(e, &format!("{PROXY}/@type"), json!("type.example/other"))
        }),
    ];
    for (name, defect, plant) in cases {
        let r = rendered(&example());
        let (mut envoy, mut args) = (parsed(&r), r.args.clone());
        plant(&mut envoy, &mut args);
        let got: Vec<&str> = run(&envoy, &args).iter().map(CheckFailure::name).collect();
        assert!(got.contains(&name), "{defect}: {got:?}");
    }
    let r = rendered(&example());
    let planted = |change: &dyn Fn(&mut Value)| {
        let mut e = parsed(&r);
        change(&mut e);
        run(&e, &r.args)
            .iter()
            .map(CheckFailure::name)
            .collect::<Vec<_>>()
    };
    for (key, value) in [("interval", "0s"), ("timeout", "0s")] {
        let got = planted(&|e| set(e, &format!("{keepalive}/{key}"), json!(value)));
        assert_eq!(got, ["upstream-keepalive"], "{key} {value}");
    }
    let got = planted(&|e| {
        let k = at(e, &keepalive).as_object_mut().expect("an object");
        k.remove("interval");
        k.remove("timeout");
    });
    assert_eq!(got, ["upstream-keepalive"], "neither");
    let got = planted(&|e| {
        let port = at(
            e,
            &format!("{endpoints}/0/endpoint/address/socket_address/port_value"),
        )
        .clone();
        set(
            e,
            &format!("{endpoints}/1/endpoint/address/socket_address/port_value"),
            port,
        );
    });
    assert!(got.contains(&"instances"), "one port twice: {got:?}");
    let got = planted(&|e| {
        at(e, &endpoints).as_array_mut().expect("a list").pop();
    });
    assert_eq!(got, ["instances"], "one instance");
    let got = planted(&|e| remove(e, &format!("{ROUTES}/0/route/retry_policy"), "retry_on"));
    assert_eq!(got, ["retry-routes"], "no retry_on");
}
