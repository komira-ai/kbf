//! The example unit that runs the coordinator image under Docker
//! (`packaging/container/kbf-coordinator/kbf-coordinator-container.service`) against the image's
//! Dockerfile and the program's flags: the unit mounts the files and the state directory the
//! image's default command names, publishes both listener ports, and the Dockerfile's command
//! parses with the flags the program has.

use std::path::{Path, PathBuf};

use clap::Parser;
use kbf_coordinator::config::Cli;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/container/kbf-coordinator")
}

fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The unit's `ExecStart=` as words, its `\` continuations joined.
fn exec_start(unit: &str) -> Vec<String> {
    let joined = unit.replace("\\\n", " ");
    let found: Vec<&str> = joined
        .lines()
        .filter_map(|l| l.strip_prefix("ExecStart="))
        .collect();
    assert_eq!(found.len(), 1, "ExecStart appears once: {found:?}");
    found[0].split_whitespace().map(str::to_owned).collect()
}

/// The words of the Dockerfile's `CMD [...]`.
fn dockerfile_cmd(text: &str) -> Vec<String> {
    let joined = text.replace("\\\n", " ");
    let line = joined
        .lines()
        .find_map(|l| l.strip_prefix("CMD ["))
        .expect("a CMD");
    line.trim_end_matches(']')
        .split(',')
        .map(|w| w.trim().trim_matches('"').to_owned())
        .collect()
}

fn values<'a>(words: &'a [String], flag: &str) -> Vec<&'a str> {
    words
        .windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].as_str())
        .collect()
}

/// Catches: an image whose default command no longer parses with the program's flags, which
/// would make the unit (that relies on it) start a container that exits 2.
#[test]
fn the_images_default_command_parses_with_the_programs_flags() {
    let mut argv = vec!["kbf-coordinator".to_owned()];
    argv.extend(dockerfile_cmd(&read("Dockerfile")));
    let cli = Cli::try_parse_from(&argv).expect("the CMD parses");
    assert_eq!(cli.heartbeat_listen.port(), 8443);
    assert_eq!(cli.control_listen.port(), 8444);
}

/// Catches: the unit mounting the files read-write or not at all, a state directory the files
/// named by the image's command are not under, a listener port left unpublished, or the image
/// run as root.
#[test]
fn the_unit_mounts_what_the_image_reads_and_publishes_both_listeners() {
    let unit = read("kbf-coordinator-container.service");
    let words = exec_start(&unit);
    assert_eq!(&words[..3], ["/usr/bin/docker", "run", "--rm"]);
    assert_eq!(values(&words, "--name"), ["kbf-coordinator"]);
    assert_eq!(values(&words, "--user"), ["65532:65532"]);

    let mut argv = vec!["kbf-coordinator".to_owned()];
    argv.extend(dockerfile_cmd(&read("Dockerfile")));
    let cli = Cli::try_parse_from(&argv).expect("the CMD parses");

    let volumes = values(&words, "--volume");
    let mounted = |inside: &Path, mode: &str| {
        volumes.iter().any(|v| {
            let parts: Vec<&str> = v.split(':').collect();
            let target = Path::new(parts[1]);
            inside.starts_with(target) && parts.get(2).copied().unwrap_or("rw") == mode
        })
    };
    for read_only in [
        &cli.tls_cert,
        &cli.tls_key,
        &cli.client_ca,
        &cli.oidc_config,
        &cli.operator_token_file,
    ] {
        assert!(mounted(read_only, "ro"), "{read_only:?} mounted read-only");
    }
    assert!(mounted(&cli.state_file, "rw"), "the state file is writable");

    let ports = values(&words, "--publish");
    for container_port in ["8443", "8444"] {
        assert!(
            ports
                .iter()
                .any(|p| p.ends_with(&format!(":{container_port}"))),
            "{container_port} published: {ports:?}"
        );
    }
    assert_eq!(
        words.last().map(String::as_str),
        Some("kbf-coordinator:<commit>")
    );
}
