//! The configuration of the frontend container: what is read, what is refused.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use kbf_testkit::dir::TestDir;

use super::config::{CONFIG, FrontendConfig};
use crate::trust::Trust;

const MINIMAL: &str = r#"
[frontend]
name = "kbf-frontend"
restart = "unless-stopped"
api = "127.0.0.1:8982"
drain_token_file = "/token"
"#;

fn with(extra: &str) -> String {
    MINIMAL.replace(
        "restart = \"unless-stopped\"",
        &format!("restart = \"unless-stopped\"\n{extra}"),
    )
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
    assert_eq!((config.drain_secs, config.ready_secs), (Some(60), 30));
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
    assert_eq!((config.drain_secs, config.ready_secs), (None, 90));
    assert!(config.peers.is_empty());
    assert_eq!(
        config.run.run_args("sha256:abc"),
        [
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "unless-stopped",
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
        (
            MINIMAL.replace("\"unless-stopped\"", "\"sometimes\""),
            "restart",
        ),
        (
            MINIMAL.replace("\"unless-stopped\"", "\"on-failure:x\""),
            "restart",
        ),
        (
            MINIMAL.replace("\"unless-stopped\"", "\"always:3\""),
            "restart",
        ),
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
        (with("drain_secs = 0"), "drain_secs"),
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
        "drain_secs = 1",
        "drain_secs = 1800",
        "drain_secs = 7200",
        "drain_secs = 31536000",
        "ready_secs = 10",
        "ready_secs = 600",
        "user = \"nobody\"",
        "user = \"1000:1000\"",
    ] {
        FrontendConfig::parse(&with(extra)).expect(extra);
    }
    for restart in ["always", "unless-stopped", "on-failure", "on-failure:0"] {
        FrontendConfig::parse(&MINIMAL.replace("\"unless-stopped\"", &format!("\"{restart}\"")))
            .expect(restart);
    }
}

/// A frontend that is told not to restart stays down after a reboot of the host or of docker,
/// and nothing else starts it, so the policy `no` is refused and says why.
#[test]
fn a_restart_policy_of_no_is_refused() {
    let why = refusal(&MINIMAL.replace("\"unless-stopped\"", "\"no\""));
    assert!(why.contains("restart"), "{why}");
    assert!(why.contains("reboot"), "{why}");
}

/// The three policies that bring the container back after a reboot are read as given.
#[test]
fn the_policies_that_restart_after_a_reboot_are_accepted() {
    for restart in ["unless-stopped", "always", "on-failure"] {
        let config = FrontendConfig::parse(
            &MINIMAL.replace("\"unless-stopped\"", &format!("\"{restart}\"")),
        )
        .expect(restart);
        assert_eq!(config.run.restart, restart);
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

/// `group_add` gids come out as `--group-add <gid>` words, in the order given, after `--user`
/// and before the first `--publish`.
#[test]
fn group_add_gids_are_exact_argv_words_in_order() {
    let text = MINIMAL.replace(
        "restart = \"unless-stopped\"",
        "restart = \"unless-stopped\"\nuser = \"65532:65532\"\ngroup_add = [\"990\", \"4294967295\", \"007\"]\nports = [\"8980:8980\"]",
    );
    let config = FrontendConfig::parse(&text).unwrap();
    assert_eq!(config.run.group_add, [990, u32::MAX, 7]);
    assert_eq!(
        config.run.run_args("sha256:abc"),
        [
            "run",
            "--detach",
            "--name",
            "kbf-frontend",
            "--restart",
            "unless-stopped",
            "--user",
            "65532:65532",
            "--group-add",
            "990",
            "--group-add",
            "4294967295",
            "--group-add",
            "7",
            "--publish",
            "8980:8980",
            "sha256:abc"
        ]
    );
    // Left out, there is none.
    assert!(
        FrontendConfig::parse(MINIMAL)
            .unwrap()
            .run
            .group_add
            .is_empty()
    );
}

/// A gid that is not a decimal number from 1 to 4294967295 is refused, naming `group_add`.
#[test]
fn a_gid_that_is_not_a_plain_nonzero_number_is_refused() {
    for bad in [
        "0",
        "admin",
        "-1",
        "+5",
        "1 2",
        " 7",
        "7 ",
        "",
        "0x10",
        "4294967296",
        "1e3",
        "٣",
    ] {
        let text = with(&format!("group_add = [{bad:?}]"));
        assert!(refusal(&text).contains("group_add"), "{bad:?}");
    }
    // One bad gid among good ones refuses the file.
    assert!(refusal(&with("group_add = [\"990\", \"0\"]")).contains("group_add"));
    // A number that is not quoted is not a string: refused too.
    FrontendConfig::parse(&with("group_add = [990]")).unwrap_err();
}
