//! The flags of the agent mode.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::flags::{
    DEFAULT_HELPER_SOCKET, NodeApi, ServiceConfig, ServiceDrain, ServiceHealth, Timing, parse_flags,
};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

const REQUIRED: [&str; 10] = [
    "--coordinator",
    "coordinator.farm.example:8443",
    "--client-cert",
    "/etc/kbf/updater/cert.pem",
    "--client-key",
    "/etc/kbf/updater/key.pem",
    "--ca",
    "/etc/kbf/updater/ca.pem",
    "--channel",
    "stable",
];

const NODE_API: [&str; 4] = [
    "--operator-api",
    "http://127.0.0.1:18002",
    "--token-file",
    "/etc/kbf/updater/token",
];

fn with(extra: &[&str]) -> Vec<String> {
    let mut all = args(&REQUIRED);
    all.extend(args(extra));
    all
}

/// The required flags make a config with the defaults of the times, the helper's usual socket and
/// no proxy secret; `--flag=value` is `--flag value`.
#[test]
fn the_required_flags_make_a_config_with_the_defaults() {
    let config = parse_flags(&with(&NODE_API)).unwrap();
    assert_eq!(config.coordinator.host, "coordinator.farm.example");
    assert_eq!(config.coordinator.port, 8443);
    assert_eq!(
        config.files.cert,
        PathBuf::from("/etc/kbf/updater/cert.pem")
    );
    assert_eq!(config.files.key, PathBuf::from("/etc/kbf/updater/key.pem"));
    assert_eq!(config.files.ca, PathBuf::from("/etc/kbf/updater/ca.pem"));
    assert_eq!(config.channel, "stable");
    assert_eq!(
        config.node_api,
        Some(NodeApi {
            addr: "127.0.0.1:18002".parse::<SocketAddr>().unwrap(),
            token_file: PathBuf::from("/etc/kbf/updater/token"),
        })
    );
    assert_eq!(config.helper_socket, PathBuf::from(DEFAULT_HELPER_SOCKET));
    assert_eq!(config.timing, Timing::DEFAULT);
    assert_eq!(Timing::DEFAULT.heartbeat, Duration::from_secs(15));

    let inline: Vec<String> = [REQUIRED.as_slice(), NODE_API.as_slice()]
        .concat()
        .chunks(2)
        .map(|pair| format!("{}={}", pair[0], pair[1]))
        .collect();
    assert_eq!(parse_flags(&inline).unwrap(), config);
}

/// `--cordon-dir` is read as a path and is absent unless given.
#[test]
fn the_cordon_directory_is_read() {
    assert_eq!(parse_flags(&with(&NODE_API)).unwrap().cordon_dir, None);
    let mut words = with(&NODE_API);
    words.extend(args(&["--cordon-dir", "/var/lib/kbf-updater/cordons"]));
    assert_eq!(
        parse_flags(&words).unwrap().cordon_dir,
        Some(PathBuf::from("/var/lib/kbf-updater/cordons"))
    );
}

/// Every optional flag is read: the helper's socket and each of the times.
#[test]
fn the_optional_flags_are_read() {
    let config = parse_flags(&with(&[
        "--operator-api",
        "http://127.0.0.1:18002",
        "--token-file",
        "/etc/kbf/updater/token",
        "--helper-socket",
        "/run/other.sock",
        "--daemon-node",
        "d1",
        "--heartbeat-secs",
        "20",
        "--drain-deadline-secs",
        "30",
        "--health-timeout-secs",
        "40",
        "--health-poll-secs",
        "2",
        "--retry-secs",
        "60",
    ]))
    .unwrap();
    assert_eq!(config.helper_socket, PathBuf::from("/run/other.sock"));
    assert_eq!(config.daemon_node.as_deref(), Some("d1"));
    let secs = Duration::from_secs;
    assert_eq!(config.timing.heartbeat, secs(20));
    assert_eq!(config.timing.drain_deadline, Some(secs(30)));
    assert_eq!(config.timing.health_timeout, secs(40));
    assert_eq!(config.timing.health_poll, secs(2));
    assert_eq!(config.timing.retry, secs(60));
    assert_eq!(config.timing.drain_poll, Timing::DEFAULT.drain_poll);
    assert_eq!(config.timing.margin, Timing::DEFAULT.margin);
}

/// The drain deadline has no default (absent waits however long the leases take), takes any
/// number of seconds from 1 up with no upper bound (the other times stop at a day), and refuses
/// 0 and non-numbers with the flag named.
#[test]
fn the_drain_deadline_is_absent_by_default_has_no_cap_and_refuses_zero() {
    let base = with(&NODE_API);
    assert_eq!(parse_flags(&base).unwrap().timing.drain_deadline, None);
    for (text, want) in [("1", 1), ("86401", 86_401), ("31536000", 31_536_000)] {
        let mut args = base.clone();
        args.extend(["--drain-deadline-secs".to_owned(), text.to_owned()]);
        let got = parse_flags(&args).unwrap().timing.drain_deadline;
        assert_eq!(got, Some(Duration::from_secs(want)), "{text}");
    }
    for bad in ["0", "-1", "soon", "1.5", ""] {
        let mut args = base.clone();
        args.extend(["--drain-deadline-secs".to_owned(), bad.to_owned()]);
        let err = parse_flags(&args).unwrap_err();
        assert!(
            err.contains("--drain-deadline-secs must be"),
            "{bad}: {err}"
        );
    }
}

/// Whatever is missing, repeated, unknown or malformed is refused with the flag in the message:
/// each required flag, a flag twice, a flag with no value, an unknown flag, a coordinator that is
/// an address, a channel that is not a name, an operator API that is not loopback, and times that
/// are not seconds in a day.
#[test]
fn flags_that_are_missing_or_wrong_are_refused_by_name() {
    for (position, flag) in REQUIRED.iter().enumerate().step_by(2) {
        let mut words = args(&REQUIRED);
        words.drain(position..position + 2);
        let err = parse_flags(&words).unwrap_err();
        if ["--client-cert", "--client-key", "--ca"].contains(flag) {
            assert!(
                err.starts_with("give --state-dir, or all of"),
                "{flag}: {err}"
            );
        } else {
            assert_eq!(err, format!("{flag} is required"));
        }
    }
    let cases: [(&[&str], &str); 14] = [
        (&["--frontend", "203.0.113.5:443"], "--frontend:"),
        (&["--frontend", "frontend"], "--frontend:"),
        (
            &["--enroll-frontend", "203.0.113.5:443"],
            "--enroll-frontend:",
        ),
        (
            &["--state-dir", "/var/lib/kbf-updater"],
            "give --state-dir, or all of",
        ),
        (&["--retry-secs"], "--retry-secs needs a value"),
        (&["--ca", "/other"], "--ca is given twice"),
        (&["--nope", "1"], "unknown argument --nope"),
        (&["--heartbeat-secs", "0"], "--heartbeat-secs must be"),
        (&["--heartbeat-secs", "86401"], "--heartbeat-secs must be"),
        (
            &["--health-poll-secs", "soon"],
            "--health-poll-secs must be",
        ),
        (&["--retry-secs", "-1"], "--retry-secs must be"),
        (
            &["--drain-deadline-secs", ""],
            "--drain-deadline-secs must be",
        ),
        (
            &["--health-timeout-secs", "1.5"],
            "--health-timeout-secs must be",
        ),
        (&["--helper-socket"], "--helper-socket needs a value"),
    ];
    for (extra, why) in cases {
        let err = parse_flags(&with(extra)).unwrap_err();
        assert!(err.contains(why), "{extra:?}: {err}");
    }
    let replace = |flag: &str, value: &str| {
        let mut words = with(&NODE_API);
        let at = words.iter().position(|w| w == flag).unwrap();
        words[at + 1] = value.to_owned();
        parse_flags(&words).unwrap_err()
    };
    assert!(replace("--coordinator", "203.0.113.5:8443").starts_with("--coordinator:"));
    assert!(replace("--coordinator", "coordinator").starts_with("--coordinator:"));
    assert!(replace("--channel", "Stable").contains("--channel must be a channel name"));
    assert!(replace("--channel", "").contains("--channel must be a channel name"));
    assert!(replace("--operator-api", "http://203.0.113.5:18002").contains("loopback"));
    assert!(replace("--operator-api", "https://127.0.0.1:1").contains("http://"));
}

/// With neither `--operator-api` nor `--token-file` the config has no node API: the agent is
/// given no credential for the frontend. Giving one without the other is refused, and the flag
/// of the proxy secret that once went with them is unknown.
#[test]
fn the_operator_credential_is_given_whole_or_not_at_all() {
    assert_eq!(parse_flags(&args(&REQUIRED)).unwrap().node_api, None);
    for half in [&NODE_API[..2], &NODE_API[2..]] {
        let err = parse_flags(&with(half)).unwrap_err();
        assert!(err.contains("given together"), "{half:?}: {err}");
    }
    let err = parse_flags(&with(&["--operator-proxy-auth-file", "/p"])).unwrap_err();
    assert_eq!(err, "unknown argument --operator-proxy-auth-file");
}

const STATE: [&str; 6] = [
    "--coordinator",
    "coordinator.farm.example:8443",
    "--channel",
    "stable",
    "--state-dir",
    "/var/lib/kbf-updater",
];

/// With `--state-dir` the certificate, the key and the CA are the files the enrollment wrote
/// there, and the agent renews them at the enroll listener it was told; nothing is placed by
/// hand. The node calls go to the worker listener, which is the other address.
#[test]
fn a_state_directory_names_the_files_and_the_enroll_listener_renews_them() {
    let mut words = args(&STATE);
    words.extend(args(&[
        "--frontend",
        "frontend.farm.example:8981",
        "--enroll-frontend",
        "frontend.farm.example:8983",
    ]));
    let config = parse_flags(&words).unwrap();
    assert_eq!(
        config.state_dir,
        Some(PathBuf::from("/var/lib/kbf-updater"))
    );
    assert_eq!(
        config.files.cert,
        PathBuf::from("/var/lib/kbf-updater/pki/updater.pem")
    );
    assert_eq!(
        config.files.key,
        PathBuf::from("/var/lib/kbf-updater/pki/updater.key")
    );
    assert_eq!(
        config.files.ca,
        PathBuf::from("/var/lib/kbf-updater/pki/ca.pem")
    );
    let frontend = config.frontend.unwrap();
    assert_eq!(
        (frontend.host.as_str(), frontend.port),
        ("frontend.farm.example", 8981)
    );
    let enroll = config.enroll_frontend.unwrap();
    assert_eq!(
        (enroll.host.as_str(), enroll.port),
        ("frontend.farm.example", 8983)
    );
    assert_eq!(config.node_api, None, "no token, as shipped");
}

/// A state directory without an enroll listener would be a certificate nothing renews, and
/// files placed by hand beside a state directory would be two answers to where the certificate
/// is: both are refused. A frontend alone, with placed files, is the drain without a renewal,
/// and a state directory with the enroll listener alone is a renewal without the drain.
#[test]
fn the_state_directory_goes_with_the_enroll_listener_and_not_with_placed_files() {
    let err = parse_flags(&args(&STATE)).unwrap_err();
    assert_eq!(
        err,
        "--state-dir needs --enroll-frontend: the certificate is renewed there"
    );
    let mut both = args(&STATE);
    both.extend(args(&["--enroll-frontend", "f.example:8983"]));
    let renew_only = parse_flags(&both).unwrap();
    assert_eq!(
        (
            renew_only.frontend.is_some(),
            renew_only.enroll_frontend.is_some()
        ),
        (false, true)
    );
    both.extend(args(&["--frontend", "f.example:8981"]));
    both.extend(args(&["--ca", "/etc/kbf/updater/ca.pem"]));
    let err = parse_flags(&both).unwrap_err();
    assert!(err.starts_with("give --state-dir, or all of"), "{err}");
    let placed = parse_flags(&with(&["--frontend", "f.example:8981"])).unwrap();
    assert_eq!((placed.state_dir, placed.frontend.is_some()), (None, true));
}

/// `--daemon-node` is a node id, and is left out unless the host runs a daemon beside its
/// frontend.
#[test]
fn the_daemon_node_is_a_node_id_or_not_given() {
    assert_eq!(parse_flags(&with(&NODE_API)).unwrap().daemon_node, None);
    for bad in ["D1", "-d", "d_1", "", "a.b"] {
        let err = parse_flags(&with(&["--daemon-node", bad])).unwrap_err();
        assert!(err.contains("--daemon-node"), "{bad}: {err}");
    }
}

/// No `--service` is no services; each is read, in the order given, as `--service V` or
/// `--service=V`: the channel is the service's name unless one is given, the drain is the node's
/// unless `none`, the health is the node's unless there is a readiness URL (a loopback address,
/// with a path or without), and the helper is the socket given.
#[test]
fn services_are_read_in_order_with_their_settings() {
    assert!(parse_flags(&with(&NODE_API)).unwrap().services.is_empty());
    let mut words = with(&NODE_API);
    words.extend(args(&[
        "--service",
        "db,helper-socket=/run/db.sock",
        "--service=cache,channel=cache-stable,helper-socket=/run/cache.sock,drain=none,ready-url=http://127.0.0.1:8080/readyz",
        "--service=plain,helper-socket=/run/plain.sock,drain=node,ready-url=http://[::1]:9",
    ]));
    let config = parse_flags(&words).unwrap();
    assert_eq!(
        config.services,
        [
            ServiceConfig {
                name: "db".to_owned(),
                channel: "db".to_owned(),
                helper_socket: PathBuf::from("/run/db.sock"),
                drain: ServiceDrain::Node,
                health: ServiceHealth::Node,
            },
            ServiceConfig {
                name: "cache".to_owned(),
                channel: "cache-stable".to_owned(),
                helper_socket: PathBuf::from("/run/cache.sock"),
                drain: ServiceDrain::None,
                health: ServiceHealth::Ready {
                    addr: "127.0.0.1:8080".parse().unwrap(),
                    path: "/readyz".to_owned(),
                },
            },
            ServiceConfig {
                name: "plain".to_owned(),
                channel: "plain".to_owned(),
                helper_socket: PathBuf::from("/run/plain.sock"),
                drain: ServiceDrain::Node,
                health: ServiceHealth::Ready {
                    addr: "[::1]:9".parse().unwrap(),
                    path: "/".to_owned(),
                },
            },
        ]
    );
    // The other flags are read as before around them.
    assert_eq!(config.channel, "stable");
}

/// A `--service` that is not a service name (or is `kbf`, or is missing its value), is given
/// twice, has a setting that is not `key=value`, unknown, repeated or out of range, or a
/// readiness URL that is not plain http to a loopback address, is refused with the service in
/// the text.
#[test]
fn a_service_flag_that_is_wrong_is_refused() {
    let cases: [(&str, &str); 17] = [
        ("", "service name"),
        ("kbf", "other than kbf"),
        ("Db", "service name"),
        ("db,channel", "not key=value"),
        ("db,channel=a,channel=b", "channel is given twice"),
        ("db,speed=fast", "unknown setting speed"),
        ("db,channel=UP", "the channel must be a channel name"),
        (
            "a-very-long-service-name-that-is-not-a-channel-name-at-all",
            "give one with channel=",
        ),
        (
            "db,helper-socket=run/x.sock",
            "helper-socket must be an absolute path",
        ),
        ("db,drain=sometimes", "drain is node or none"),
        (
            "db,ready-url=https://127.0.0.1:1/",
            "must start with http://",
        ),
        ("db,ready-url=http://example.com:1/", "with an IP address"),
        (
            "db,ready-url=http://203.0.113.5:1/",
            "must be a loopback address",
        ),
        ("db,ready-url=http://127.0.0.1:1/a b", "space or a control"),
        ("db,ready-url=http://127.0.0.1/", "with an IP address"),
        ("db,ready-url=http://127.0.0.1:1", "ok"),
        (
            "db,ready-url=http://127.0.0.1:1/x\u{7}",
            "space or a control",
        ),
    ];
    for (value, expect) in cases {
        // Each case but the socket's own is given a socket, so it reaches the check it is about.
        let value = if value.starts_with("db") && !value.contains("helper-socket") {
            format!("{value},helper-socket=/run/db.sock")
        } else {
            value.to_owned()
        };
        let result = parse_flags(&with(&["--service", &value]));
        if expect == "ok" {
            assert!(result.is_ok(), "{value}");
            continue;
        }
        let err = result.unwrap_err();
        assert!(err.contains(expect), "{value}: {err}");
    }
    let err = parse_flags(&with(&[
        "--service",
        "db,helper-socket=/run/db.sock",
        "--service=db,channel=x,helper-socket=/run/x.sock",
    ]))
    .unwrap_err();
    assert_eq!(err, "--service db is given twice");
    let err = parse_flags(&with(&["--service"])).unwrap_err();
    assert!(err.contains("service name"), "{err}");
}

/// Catches: a service updated through a helper that is not its own: the agent's, which installs
/// `kbf` (an install request names no service, so kbf would be installed at the service's
/// version), or another service's. A service without `helper-socket=` is refused, and so is one
/// whose socket is the agent's `--helper-socket` (the default one or one given), one that
/// shares another service's socket, or one whose path holds a `..` component; a socket of
/// its own is read.
#[test]
fn a_service_needs_a_helper_of_its_own() {
    let err = parse_flags(&with(&["--service", "db"])).unwrap_err();
    assert!(
        err.contains("--service db: helper-socket= is required"),
        "{err}"
    );
    let err = parse_flags(&with(&[
        "--service",
        "db,helper-socket=/run/kbf-host-install/local.sock",
    ]))
    .unwrap_err();
    assert!(
        err.contains("--service db: helper-socket is the agent's"),
        "{err}"
    );
    let err = parse_flags(&with(&[
        "--helper-socket",
        "/run/h.sock",
        "--service",
        "db,helper-socket=/run/h.sock",
    ]))
    .unwrap_err();
    assert!(err.contains("helper-socket is the agent's"), "{err}");
    let err = parse_flags(&with(&[
        "--service",
        "db,helper-socket=/run/one.sock",
        "--service",
        "cache,helper-socket=/run/one.sock",
    ]))
    .unwrap_err();
    assert!(
        err.contains("--service cache: helper-socket is also db's"),
        "{err}"
    );
    let err = parse_flags(&with(&[
        "--service",
        "db,helper-socket=/run/x/../kbf-host-install/local.sock",
    ]))
    .unwrap_err();
    assert!(err.contains("may not hold a .. component"), "{err}");
    let config = parse_flags(&with(&[
        "--helper-socket",
        "/run/h.sock",
        "--service",
        "db,helper-socket=/run/kbf-host-install/local.sock",
    ]))
    .unwrap();
    assert_eq!(
        config.services[0].helper_socket,
        PathBuf::from("/run/kbf-host-install/local.sock")
    );
}
