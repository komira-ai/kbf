//! The `linux` kit's settings: each bad setting is refused before anything is
//! rendered, and an unset key takes the default `config.example` shows.

use super::config::LinuxConfig;
use super::shared;

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");
const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn img() -> String {
    format!("gcr.io/distroless/base-debian12@{D1}")
}

/// `config.example` with each `KEY=value` line replaced (the key must be there).
fn example_with(settings: &[(&str, &str)]) -> String {
    let mut text = EXAMPLE.to_owned();
    for (key, value) in settings {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap_or_else(|| panic!("config.example has no {key}= line"))
            .to_owned();
        text = text.replace(&line, &format!("{key}={value}"));
    }
    text
}

fn config(settings: &[(&str, &str)]) -> LinuxConfig {
    LinuxConfig::parse(&example_with(settings)).expect("valid settings")
}

fn refusal(text: &str) -> String {
    LinuxConfig::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn bad_settings_are_refused_before_anything_renders() {
    // Catches, one row each: a host with nothing left for builds (or exactly 0), too
    // little left to the OS, a listener on every interface, a host value left unset,
    // a typo'd or repeated key, a character systemd would expand or a unit cannot
    // carry, an image by tag or in the daemon's docker:// form, a number or port that
    // is not one, two listeners on one port (the first pair, the last pair, the first
    // and the last), a relative path, and a MinIO unit that is not a service.
    let tag = "gcr.io/distroless/base-debian12:latest";
    let tag_digest = format!("gcr.io/distroless/base-debian12:nonroot@{D1}");
    let docker = format!("docker://{}", img());
    let cases: Vec<(String, &str)> = vec![
        (
            example_with(&[("KBF_RAM_GIB", "56")]),
            "KBF_RAM_GIB=56 leaves 0 GiB for builds after MinIO 32, kbf-server 16 and the OS 8",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "8")]),
            "KBF_RAM_GIB=8 leaves -48 GiB for builds",
        ),
        (
            example_with(&[("KBF_OS_HEADROOM_GIB", "3")]),
            "KBF_OS_HEADROOM_GIB=3: keep at least 4 GiB for the OS",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "0.0.0.0")]),
            "KBF_LISTEN_ADDRESS=0.0.0.0: bind the host's pinned address, not every interface",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "::")]),
            "KBF_LISTEN_ADDRESS=::: bind the host's pinned address",
        ),
        (
            example_with(&[("KBF_LISTEN_ADDRESS", "[::]")]),
            "KBF_LISTEN_ADDRESS holds a character the units cannot carry: [::]",
        ),
        (
            example_with(&[("KBF_HOST_NAME", "")]),
            "KBF_HOST_NAME is not set (config file)",
        ),
        (
            example_with(&[("KBF_NODE_ID", "")]),
            "KBF_NODE_ID is not set (config file)",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "")]),
            "KBF_RAM_GIB is not set (config file)",
        ),
        (
            format!("{EXAMPLE}KBF_RAM=1\n"),
            "line 111: unknown key KBF_RAM",
        ),
        (
            format!("{EXAMPLE}KBF_RAM_GIB=1\n"),
            "line 111: KBF_RAM_GIB is already set on line 24",
        ),
        (
            example_with(&[("KBF_NODE_ID", "a;b")]),
            "KBF_NODE_ID holds a character the units cannot carry: a;b",
        ),
        (
            example_with(&[("KBF_ETC", "/etc/%h")]),
            "KBF_ETC holds a character the units cannot carry: /etc/%h",
        ),
        (
            example_with(&[("KBF_USER", "$USER")]),
            "KBF_USER holds a character the units cannot carry: $USER",
        ),
        (example_with(&[("KBF_IMAGES", tag)]), "names no digest"),
        (
            example_with(&[("KBF_IMAGES", &tag_digest)]),
            "names a tag; name an image by digest only",
        ),
        (
            example_with(&[("KBF_IMAGES", &docker)]),
            "write the bare <repo>@sha256:<digest>",
        ),
        (
            example_with(&[("KBF_RAM_GIB", "+256")]),
            "KBF_RAM_GIB must be a whole number, not '+256'",
        ),
        (
            example_with(&[("MINIO_RESERVATION_GIB", "32G")]),
            "MINIO_RESERVATION_GIB must be a whole number, not '32G'",
        ),
        (
            example_with(&[("KBF_REAPI_PORT", "0")]),
            "KBF_REAPI_PORT must be a port number from 1 to 65535, not '0'",
        ),
        (
            example_with(&[("KBF_API_PORT", "65536")]),
            "KBF_API_PORT must be a port number from 1 to 65535, not '65536'",
        ),
        (
            example_with(&[("KBF_WORKER_PORT", "+81")]),
            "KBF_WORKER_PORT must be a port number from 1 to 65535, not '+81'",
        ),
        (
            example_with(&[("KBF_ETC", "etc/kbf")]),
            "KBF_ETC: \"etc/kbf\" is not absolute",
        ),
        (
            example_with(&[("KBF_META_DIR", "/var/lib/../raft")]),
            "KBF_META_DIR: \"/var/lib/../raft\" has an empty",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "0")]),
            "KBF_STORAGE_PORT must be a port number from 1 to 65535, not '0'",
        ),
        (
            example_with(&[("KBF_HOT_DIR", "hot")]),
            "KBF_HOT_DIR: \"hot\" is not absolute",
        ),
        (
            example_with(&[("KBF_REAPI_PORT", "8981")]),
            "KBF_REAPI_PORT and KBF_WORKER_PORT are both 8981: each listener of a host needs its own port",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "8982")]),
            "KBF_API_PORT and KBF_STORAGE_PORT are both 8982",
        ),
        (
            example_with(&[("KBF_STORAGE_PORT", "8980")]),
            "KBF_REAPI_PORT and KBF_STORAGE_PORT are both 8980",
        ),
        (
            example_with(&[("MINIO_UNIT", "minio")]),
            "MINIO_UNIT=minio is not a .service unit name",
        ),
        (
            example_with(&[("MINIO_UNIT", ".service")]),
            "MINIO_UNIT=.service is not a .service unit name",
        ),
    ];
    let mut wrong = Vec::new();
    for (text, want) in &cases {
        let got = refusal(text);
        if !got.contains(want) {
            wrong.push(format!("want {want:?}, got {got:?}"));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
    // The least that renders: one GiB for builds.
    assert_eq!(config(&[("KBF_RAM_GIB", "57")]).actions_gib, 1);
    assert_eq!(config(&[("KBF_OS_HEADROOM_GIB", "4")]).actions_gib, 204);
}

#[test]
fn a_settings_file_for_the_old_units_is_refused() {
    // Catches: the removed KBF_META_LOG still accepted (a settings file written for the
    // old units would run with a log it never asked for), and a defaulted key not taking
    // its default.
    let old = format!("{EXAMPLE}KBF_META_LOG=raft\n");
    assert!(refusal(&old).contains("KBF_META_LOG"), "{}", refusal(&old));

    let defaults = config(&[
        ("KBF_REAPI_PORT", ""),
        ("KBF_ETC", "\"\""),
        ("MINIO_UNIT", "minio-kbf.service"),
    ]);
    assert_eq!(defaults.reapi_port, 8980);
    assert_eq!(defaults.etc.as_str(), "/etc/kbf");
}

#[test]
fn an_unset_key_takes_the_default_config_example_shows() {
    // Catches: a default that differs from what config.example documents (a host
    // configured by leaving keys out would get other units than the example says), and
    // an unset key read as empty rather than defaulted.
    let host = "KBF_HOST_NAME=storage-node.example.internal\nKBF_LISTEN_ADDRESS=192.0.2.10\n\
                KBF_NODE_ID=storage-node-1\nKBF_RAM_GIB=256\n";
    let minimal = LinuxConfig::parse(host).expect("the four host values are enough");
    // MINIO_S3_ENDPOINT alone has no default in the settings: the units of the modes that
    // carry a cold store take the host's own MinIO, and `--mode storage` refuses (modes_tests).
    assert_eq!(minimal.minio_s3_endpoint, None);
    let example = config(&[]);
    assert_eq!(
        example.minio_s3_endpoint.as_deref(),
        Some(super::config::LOOPBACK_MINIO)
    );
    assert_eq!(
        LinuxConfig {
            minio_s3_endpoint: example.minio_s3_endpoint.clone(),
            // The example names another frontend and a storage node to call; left out,
            // the frontend is this host and there is no storage URL.
            frontend_name: example.frontend_name.clone(),
            storage_url: example.storage_url.clone(),
            ..minimal.clone()
        },
        example
    );
    assert_eq!(
        shared::render_slice(&minimal, Some(1500)).expect("renders"),
        shared::render_slice(&example, Some(1500)).expect("renders")
    );
}

/// `KBF_HOT_DIR` unset takes `<KBF_DATA_DIR>/hot`, in the units of both modes that carry a
/// hot store: the old default under `/var/lib/kbf-server` is the frontend account's
/// 0700 directory, which `storage-and-daemon`'s `KBF_USER` cannot enter. A set value
/// wins. Catches the default left at the server account's directory, and one that
/// ignores a moved `KBF_DATA_DIR`.
#[test]
fn the_hot_dir_defaults_beside_the_data_dir_in_each_mode() {
    use super::modes::{self, Mode};
    let hot = |c: &LinuxConfig, mode| {
        let text = modes::render(c, Some(1500), mode).expect("renders");
        text.lines()
            .find_map(|l| l.trim().strip_prefix("--hot-store=").map(str::to_owned))
            .unwrap_or_default()
            .trim_end_matches(" \\")
            .to_owned()
    };
    let plain = config(&[("KBF_HOT_DIR", "")]);
    assert_eq!(plain.hot_dir.as_str(), "/var/lib/kbf/hot");
    let moved = config(&[("KBF_HOT_DIR", ""), ("KBF_DATA_DIR", "/srv/kbf")]);
    for mode in [Mode::Storage, Mode::StorageAndDaemon] {
        assert_eq!(hot(&plain, mode), "file:///var/lib/kbf/hot", "{mode}");
        assert_eq!(hot(&moved, mode), "file:///srv/kbf/hot", "{mode}");
    }
    let set = config(&[("KBF_HOT_DIR", "/fast/hot"), ("KBF_DATA_DIR", "/srv/kbf")]);
    assert_eq!(set.hot_dir.as_str(), "/fast/hot");
    assert_eq!(hot(&set, Mode::StorageAndDaemon), "file:///fast/hot");
}
