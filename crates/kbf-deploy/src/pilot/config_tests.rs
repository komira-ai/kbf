use super::*;

const EXAMPLE: &str = include_str!("../../templates/pilot/config.example");

fn config(text: &str) -> Config {
    Config::parse(text, KEYS).expect("valid lines")
}

fn server(text: &str) -> Result<ServerSettings, String> {
    ServerSettings::from_config(&config(text), None).map_err(|e| e.to_string())
}

/// Catches an example that no longer parses (the documented settings would be
/// refused), a default lost or changed, and the bind address not falling back to
/// the server host when KBF_SERVER_BIND is empty.
#[test]
fn the_example_parses_with_the_documented_defaults() {
    let s = server(EXAMPLE).expect("the example is valid");
    assert_eq!(
        s,
        ServerSettings {
            bind: Ipv4Addr::new(192, 0, 2, 10),
            iface: "tailscale0".into(),
            reapi_port: 8980,
            worker_port: 8981,
            tls_server_name: "kbf-pilot-server".into(),
            state: None,
            memory_max: "4G".into(),
            cpu_quota: "200%".into(),
            tasks_max: "512".into(),
            minio_endpoint: "http://192.0.2.20:30900".into(),
            s3_bucket: "kbf-pilot-cas".into(),
            s3_region: "us-east-1".into(),
            s3_prefix: "kbf/".into(),
            conditional_put: true,
        }
    );
    let minimal = "KBF_SERVER_HOST=192.0.2.10\nKBF_TAILNET_IFACE=eth0\n\
                   MINIO_ENDPOINT=http://192.0.2.20:9000\nKBF_S3_BUCKET=kbf-b\n";
    let m = server(minimal).expect("defaults fill the rest");
    assert_eq!(
        (m.reapi_port, m.worker_port, m.tls_server_name.as_str()),
        (8980, 8981, "kbf-pilot-server")
    );
    assert_eq!(
        (
            m.memory_max.as_str(),
            m.cpu_quota.as_str(),
            m.tasks_max.as_str()
        ),
        ("4G", "200%", "512")
    );
    assert_eq!(
        (m.s3_region.as_str(), m.s3_prefix.as_str()),
        ("us-east-1", "kbf/")
    );
    assert!(m.conditional_put);
    let o = OverlaySettings::from_config(&config(EXAMPLE)).expect("valid");
    assert_eq!(
        o,
        OverlaySettings {
            server_host: "192.0.2.10".into(),
            reapi_port: 8980,
            instance_name: "main".into(),
            concurrency: "8".into(),
            linux_properties: "OSFamily=Linux,label.pool=linux".into(),
            darwin_properties: "OSFamily=Darwin,label.pool=mac".into(),
            darwin_labels: "pool=mac".into(),
            darwin_macos_hosts: "26.0-0123456789abcdef".into(),
        }
    );
}

/// Catches the bind address ignoring KBF_SERVER_BIND or the flag (the flag wins over
/// the file, the file's bind over its host), and a host name taken as an address.
#[test]
fn the_flag_wins_over_the_bind_key_and_the_bind_key_over_the_host() {
    let c = config(
        &format!("{EXAMPLE}\n").replace("KBF_SERVER_BIND=\n", "KBF_SERVER_BIND=192.0.2.11\n"),
    );
    let from_file = ServerSettings::from_config(&c, None).expect("valid");
    assert_eq!(from_file.bind, Ipv4Addr::new(192, 0, 2, 11));
    let flag = ServerSettings::from_config(&c, Some("192.0.2.12")).expect("valid");
    assert_eq!(flag.bind, Ipv4Addr::new(192, 0, 2, 12));
    let name = ServerSettings::from_config(&c, Some("kbf.example"))
        .err()
        .map(|e| e.to_string());
    assert_eq!(
        name.as_deref(),
        Some("bind address \"kbf.example\" is not an IPv4 address")
    );
}

/// Catches each bad setting reaching the unit: a missing site value taken as a
/// default, a port that is not one, a cap systemd would refuse or read otherwise (a
/// zero, a leading zero, a unit it does not take), a conditional-put flag that is
/// neither 0 nor 1, and a state directory that is not absolute.
#[test]
fn each_bad_server_setting_is_refused() {
    let set = |key: &str, value: &str| {
        let mut out = String::new();
        let mut found = false;
        for line in EXAMPLE.lines() {
            if line.starts_with(&format!("{key}=")) {
                out.push_str(&format!("{key}={value}\n"));
                found = true;
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        assert!(found, "{key} is in the example");
        out
    };
    let cases = [
        (
            set("KBF_SERVER_HOST", ""),
            "no bind address (--bind, KBF_SERVER_BIND or KBF_SERVER_HOST): the server host's address",
        ),
        (
            set("KBF_TAILNET_IFACE", ""),
            "KBF_TAILNET_IFACE is not set (config file)",
        ),
        (
            set("MINIO_ENDPOINT", ""),
            "MINIO_ENDPOINT is not set (config file)",
        ),
        (
            set("KBF_S3_BUCKET", ""),
            "KBF_S3_BUCKET is not set (config file)",
        ),
        (
            set("KBF_REAPI_PORT", "0"),
            "KBF_REAPI_PORT must be a port number from 1 to 65535, not '0'",
        ),
        (
            set("KBF_WORKER_PORT", "65536"),
            "KBF_WORKER_PORT must be a port number from 1 to 65535, not '65536'",
        ),
        (
            set("KBF_SERVER_MEMORY_MAX", "4"),
            "KBF_SERVER_MEMORY_MAX=4 is not like 4G",
        ),
        (
            set("KBF_SERVER_MEMORY_MAX", "0G"),
            "KBF_SERVER_MEMORY_MAX=0G is not like 4G",
        ),
        (
            set("KBF_SERVER_MEMORY_MAX", "4T"),
            "KBF_SERVER_MEMORY_MAX=4T is not like 4G",
        ),
        (
            set("KBF_SERVER_CPU_QUOTA", "200"),
            "KBF_SERVER_CPU_QUOTA=200 is not like 200%",
        ),
        (
            set("KBF_SERVER_CPU_QUOTA", "02%"),
            "KBF_SERVER_CPU_QUOTA=02% is not like 200%",
        ),
        (
            set("KBF_SERVER_TASKS_MAX", "5x"),
            "KBF_SERVER_TASKS_MAX=5x is not like 512",
        ),
        (
            set("KBF_S3_CONDITIONAL_PUT", "yes"),
            "KBF_S3_CONDITIONAL_PUT=yes: 1 or 0",
        ),
        (
            set("KBF_SERVER_STATE", "state"),
            "KBF_SERVER_STATE: \"state\" is not absolute",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(server(&text).err().as_deref(), Some(want), "{want}");
    }
    let off = server(&set("KBF_S3_CONDITIONAL_PUT", "0")).expect("0 is valid");
    assert!(!off.conditional_put);
    let state = server(&set("KBF_SERVER_STATE", "/srv/kbf-pilot")).expect("valid");
    assert_eq!(
        state.state.map(|s| s.to_string()).as_deref(),
        Some("/srv/kbf-pilot")
    );
    for (key, ok) in [
        ("KBF_SERVER_MEMORY_MAX", "512M"),
        ("KBF_SERVER_MEMORY_MAX", "1K"),
    ] {
        assert!(server(&set(key, ok)).is_ok(), "{key}={ok}");
    }
}

/// Catches buckconfig's settings taking a missing site value or a concurrency buck2
/// would refuse, and the labels key made required (an empty one is a daemon set with
/// no labels).
#[test]
fn each_bad_overlay_setting_is_refused() {
    let overlay =
        |text: &str| OverlaySettings::from_config(&config(text)).map_err(|e| e.to_string());
    let base = "KBF_SERVER_HOST=192.0.2.10\nKBF_INSTANCE_NAME=main\n\
                KBF_LINUX_PROPERTIES=OSFamily=Linux\nKBF_DARWIN_PROPERTIES=OSFamily=Darwin\n\
                KBF_DARWIN_MACOS_HOSTS=26.0-x\n";
    let o = overlay(base).expect("labels and concurrency have defaults");
    assert_eq!(
        (o.darwin_labels.as_str(), o.concurrency.as_str()),
        ("", "8")
    );
    let cases = [
        (
            base.replace("KBF_SERVER_HOST=192.0.2.10\n", ""),
            "KBF_SERVER_HOST is not set (config file)",
        ),
        (
            base.replace("KBF_INSTANCE_NAME=main\n", ""),
            "KBF_INSTANCE_NAME is not set (config file)",
        ),
        (
            base.replace("KBF_LINUX_PROPERTIES=OSFamily=Linux\n", ""),
            "KBF_LINUX_PROPERTIES is not set (config file)",
        ),
        (
            base.replace("KBF_DARWIN_PROPERTIES=OSFamily=Darwin\n", ""),
            "KBF_DARWIN_PROPERTIES is not set (config file)",
        ),
        (
            base.replace("KBF_DARWIN_MACOS_HOSTS=26.0-x\n", ""),
            "KBF_DARWIN_MACOS_HOSTS is not set (config file)",
        ),
        (
            format!("{base}KBF_CONCURRENCY=0\n"),
            "KBF_CONCURRENCY=0 is not like 8",
        ),
        (
            format!("{base}KBF_REAPI_PORT=x\n"),
            "KBF_REAPI_PORT must be a port number from 1 to 65535, not 'x'",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(overlay(&text).err().as_deref(), Some(want), "{want}");
    }
}

/// Catches the kit's other keys refused (one config file serves every step of the
/// kit) and a typo accepted.
#[test]
fn the_kits_other_keys_are_accepted_and_a_typo_is_not() {
    let others = "KBF_DAEMON_NODES=\"mac-1\"\nKBF_DRIVER=native\nKBF_DAEMON_EXTRA_ARGS=\n\
                  KBF_USER=_kbf\nKBF_ROOT=/usr/local/kbf\nKBF_LABEL_PREFIX=org.example\n\
                  KBF_MIN_MACOS=15.0\nKBF_MIN_FREE_GB=200\nMINIO_NAMESPACE=minio\n\
                  MINIO_SERVICE=minio\nMINIO_NODEPORT=30900\nKBF_EXIT_TARGET=root//:hello\n";
    assert!(Config::parse(&format!("{EXAMPLE}{others}"), KEYS).is_ok());
    assert!(Config::parse("KBF_SERVER_HOSTS=x\n", KEYS).is_err());
}
