//! The flags of the agent mode.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::flags::{DEFAULT_HELPER_SOCKET, NodeApi, Timing, parse_flags};

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
            proxy_auth_file: None,
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

/// Every optional flag is read: the proxy's secret file, the helper's socket and each of the
/// times.
#[test]
fn the_optional_flags_are_read() {
    let config = parse_flags(&with(&[
        "--operator-api",
        "http://127.0.0.1:18002",
        "--token-file",
        "/etc/kbf/updater/token",
        "--operator-proxy-auth-file",
        "/etc/kbf/updater/proxy",
        "--helper-socket",
        "/run/other.sock",
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
    assert_eq!(
        config.node_api.unwrap().proxy_auth_file,
        Some(PathBuf::from("/etc/kbf/updater/proxy"))
    );
    assert_eq!(config.helper_socket, PathBuf::from("/run/other.sock"));
    let secs = Duration::from_secs;
    assert_eq!(config.timing.heartbeat, secs(20));
    assert_eq!(config.timing.drain_deadline, secs(30));
    assert_eq!(config.timing.health_timeout, secs(40));
    assert_eq!(config.timing.health_poll, secs(2));
    assert_eq!(config.timing.retry, secs(60));
    assert_eq!(config.timing.drain_poll, Timing::DEFAULT.drain_poll);
    assert_eq!(config.timing.margin, Timing::DEFAULT.margin);
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
        assert_eq!(err, format!("{flag} is required"));
    }
    let cases: [(&[&str], &str); 10] = [
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
/// given no credential for the frontend. Giving one without the other, or the proxy secret
/// alone, is refused.
#[test]
fn the_operator_credential_is_given_whole_or_not_at_all() {
    assert_eq!(parse_flags(&args(&REQUIRED)).unwrap().node_api, None);
    for half in [
        &NODE_API[..2],
        &NODE_API[2..],
        &["--operator-proxy-auth-file", "/p"],
    ] {
        let err = parse_flags(&with(half)).unwrap_err();
        assert!(err.contains("given together"), "{half:?}: {err}");
    }
}
