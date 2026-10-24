//! The configuration of the frontend container: what is read, what is refused.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use kbf_testkit::dir::TestDir;

use super::config::{CONFIG, FrontendConfig, STOP_SECS};
use crate::trust::Trust;

const MINIMAL: &str = r#"
[frontend]
name = "kbf-frontend"
restart = "no"
api = "127.0.0.1:8982"
drain_token_file = "/token"
"#;

fn with(extra: &str) -> String {
    MINIMAL.replace("restart = \"no\"", &format!("restart = \"no\"\n{extra}"))
}

fn refusal(text: &str) -> String {
    FrontendConfig::parse(text).expect_err(text)
}

/// Everything the file can say is read, and the run part makes docker's arguments in a fixed
/// order, the image after the options and the command after the image.
#[test]
fn a_full_file_is_read_and_makes_the_docker_arguments() {
    let text = r#"
[frontend]
name = "kbf-frontend"
restart = "on-failure:5"
network = "kbf-net"
user = "65532:65532"
ports = ["127.0.0.1:8982:8982", "8980:8980"]
mounts = ["/etc/kbf:/etc/kbf:ro"]
command = ["--config", "/etc/kbf/kbf.toml"]
api = "127.0.0.1:8982"
drain_token_file = "/etc/kbf/operator.token"
peers = ["203.0.113.11:8982"]
drain_secs = 60
ready_secs = 30
"#;
    let config = FrontendConfig::parse(text).unwrap();
    assert_eq!(config.api, "127.0.0.1:8982".parse().unwrap());
    assert_eq!(config.peers, ["203.0.113.11:8982".parse().unwrap()]);
    assert_eq!((config.drain_secs, config.ready_secs), (60, 30));
    assert_eq!(config.switch_secs(), 60 + STOP_SECS + 30);
    assert_eq!(
        config.run.run_args("sha256:abc"),
        [
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "on-failure:5",
            "--network",
            "kbf-net",
            "--user",
            "65532:65532",
            "--publish",
            "127.0.0.1:8982:8982",
            "--publish",
            "8980:8980",
            "--volume",
            "/etc/kbf:/etc/kbf:ro",
            "sha256:abc",
            "--config",
            "/etc/kbf/kbf.toml",
        ]
    );
}

/// What is left out takes its default, and a spec with no options makes the shortest command.
#[test]
fn what_is_left_out_takes_its_default() {
    let config = FrontendConfig::parse(MINIMAL).unwrap();
    assert_eq!((config.drain_secs, config.ready_secs), (180, 90));
    assert!(config.peers.is_empty());
    assert_eq!(
        config.run.run_args("sha256:abc"),
        [
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "no",
            "sha256:abc"
        ]
    );
}

/// Every value that docker would take as something else, or that cannot be one, is refused
/// with the line that says which.
#[test]
fn values_that_are_not_what_they_name_are_refused() {
    let cases: Vec<(String, &str)> = vec![
        (MINIMAL.replace("kbf-frontend", "kbf frontend"), "name"),
        (MINIMAL.replace("kbf-frontend", "-x"), "name"),
        (MINIMAL.replace("kbf-frontend", &"x".repeat(65)), "name"),
        (MINIMAL.replace("\"no\"", "\"sometimes\""), "restart"),
        (MINIMAL.replace("\"no\"", "\"on-failure:x\""), "restart"),
        (MINIMAL.replace("\"no\"", "\"always:3\""), "restart"),
        (with("network = \"container:other\""), "network"),
        (with("user = \"a:b:c\""), "user"),
        (with("user = \":b\""), "user"),
        (with("user = \"a:\""), "user"),
        (with("ports = [\"\"]"), "port"),
        (with("ports = [\"80:80\\u0001\"]"), "port"),
        (with("mounts = [\"etc:/etc\"]"), "mount"),
        (with("mounts = [\"\"]"), "mount"),
        (with("command = [\"a\\u0000b\"]"), "command"),
        (
            MINIMAL.replace("\"/token\"", "\"token\""),
            "drain_token_file",
        ),
        (
            MINIMAL.replace("\"/token\"", "\"/to\\u0001ken\""),
            "drain_token_file",
        ),
        (with("drain_secs = 1801"), "drain_secs"),
        (with("ready_secs = 9"), "ready_secs"),
        (with("ready_secs = 601"), "ready_secs"),
        (MINIMAL.replace("127.0.0.1:8982", "localhost:8982"), "api"),
        (with("peers = [\"nowhere\"]"), "peer"),
    ];
    for (text, word) in cases {
        let why = refusal(&text);
        assert!(why.contains(word), "{why:?} for {word}");
    }
    // A key nobody knows is a typo, not an extension.
    assert!(!refusal(&with("restarts = \"no\"")).is_empty());
    assert!(!refusal("[other]\nname = \"x\"\n").is_empty());
    assert!(!refusal("not toml [").is_empty());
}

/// The boundaries that are allowed are allowed.
#[test]
fn the_edges_of_the_ranges_are_accepted() {
    for extra in [
        "drain_secs = 0",
        "drain_secs = 1800",
        "ready_secs = 10",
        "ready_secs = 600",
        "user = \"nobody\"",
        "user = \"1000:1000\"",
    ] {
        FrontendConfig::parse(&with(extra)).expect(extra);
    }
    for restart in [
        "no",
        "always",
        "unless-stopped",
        "on-failure",
        "on-failure:0",
    ] {
        FrontendConfig::parse(&MINIMAL.replace("\"no\"", &format!("\"{restart}\"")))
            .expect(restart);
    }
}

/// The file is read as root reads configuration: through the trust, so a file anyone else may
/// write is not believed, and a missing one says which it is.
#[test]
fn the_file_is_read_through_the_trust() {
    assert_eq!(CONFIG, "/etc/kbf/host-install-frontend.toml");
    let dir = TestDir::under(std::env::temp_dir(), "hi-fe-config-load");
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let uid = fs::metadata(&*dir).unwrap().uid();
    let trust = Trust::owned_by(uid, &dir);
    let path = dir.join("frontend.toml");

    let why = FrontendConfig::load(&trust, &path).unwrap_err();
    assert!(why.contains("frontend.toml"), "{why}");

    fs::write(&path, MINIMAL).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(FrontendConfig::load(&trust, &path).is_err());

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        FrontendConfig::load(&trust, &path).unwrap(),
        FrontendConfig::parse(MINIMAL).unwrap()
    );

    fs::write(&path, "[frontend]\n").unwrap();
    let why = FrontendConfig::load(&trust, &path).unwrap_err();
    assert!(why.contains("frontend.toml"), "{why}");
}
