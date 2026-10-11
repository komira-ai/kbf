use super::config::{Shape, fits};
use super::*;

const EXAMPLE: &str = include_str!("../../templates/envoy/config.example");
const TWO: &str = include_str!("../../tests/golden/envoy/two-servers/config");

/// The goldens are what the kit's first renderer (a Python script) wrote for the same
/// config files, byte for byte.
const ONE_MANIFESTS: &str = include_str!("../../tests/golden/envoy/one-server/manifests.json");
const ONE_ENVOY: &str = include_str!("../../tests/golden/envoy/one-server/envoy.json");
const TWO_MANIFESTS: &str = include_str!("../../tests/golden/envoy/two-servers/manifests.json");
const TWO_ENVOY: &str = include_str!("../../tests/golden/envoy/two-servers/envoy.json");

/// `want == got`, failing with the first line that differs rather than two 15 kB
/// strings.
fn same_bytes(name: &str, got: &str, want: &str) {
    if got == want {
        return;
    }
    let first = got
        .lines()
        .zip(want.lines())
        .enumerate()
        .find(|(_, (g, w))| g != w);
    match first {
        Some((i, (g, w))) => panic!("{name} line {}: got {g:?}, want {w:?}", i + 1),
        None => panic!(
            "{name}: one is a prefix of the other ({} vs {} bytes)",
            got.len(),
            want.len()
        ),
    }
}

fn rendered(text: &str) -> Rendered {
    render::render(&EnvoyConfig::parse(text).expect("valid config"))
}

/// The golden test. Catches any byte drift from the first renderer on the example
/// config: a key out of order, a changed spacing or escape, a number printed as a
/// string, a route or object moved, a changed value. Drift matters because the sha256
/// of envoy.json is a pod template annotation: a byte changed in it restarts every
/// Envoy pod on the next apply.
#[test]
fn the_example_config_renders_the_golden_bytes() {
    let r = rendered(EXAMPLE);
    same_bytes("envoy.json", &r.envoy, ONE_ENVOY);
    same_bytes("manifests.json", &r.manifests, ONE_MANIFESTS);
}

/// Catches a renderer that lists only the first server, gives every server the first
/// one's SNI or name check, or orders the servers other than as configured.
#[test]
fn two_servers_render_the_golden_bytes() {
    let r = rendered(TWO);
    same_bytes("envoy.json", &r.envoy, TWO_ENVOY);
    same_bytes("manifests.json", &r.manifests, TWO_MANIFESTS);
}

/// Catches an annotation hashed over other bytes than the ConfigMap's envoy.json (the
/// text without its final newline, say), which would leave the pods running an old
/// config after a change, or restart them when nothing changed.
#[test]
fn the_pod_annotation_is_the_sha256_of_the_configmaps_envoy_json() {
    let r = rendered(EXAMPLE);
    let hash = render::sha256_hex(&r.envoy);
    let line = format!("\"{}\": \"{hash}\"", render::CONFIG_HASH_ANNOTATION);
    assert!(r.manifests.contains(&line), "{line}");
    let embedded = serde_json::to_string(&r.envoy).expect("a JSON string");
    assert!(
        r.manifests.contains(&embedded),
        "the ConfigMap carries envoy.json"
    );
}

/// Catches a hex encoding that drops leading zeros or prints upper case.
#[test]
fn sha256_hex_is_64_lower_case_digits() {
    assert_eq!(
        render::sha256_hex("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        render::sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// The example with the line for `key` replaced by `KEY=value` (or dropped, for
/// `None`).
fn with(key: &str, value: Option<&str>) -> String {
    let prefix = format!("{key}=");
    let mut out = String::new();
    for line in EXAMPLE.lines() {
        if line.starts_with(&prefix) {
            if let Some(v) = value {
                out.push_str(&format!("{key}={v}\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn refusal(text: &str) -> String {
    EnvoyConfig::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

/// Each row is a defect the first renderer refused, and the reason this one gives.
/// Catches a shape check that is missing or looser than it was: an unpinned image (the
/// pods would run whatever the tag points at), a name the cluster or Envoy would refuse
/// at apply time, a port Envoy cannot dial, a server named twice (two endpoints with
/// one transport socket match).
#[test]
fn a_value_of_the_wrong_shape_is_refused() {
    let label = Shape::DnsLabel.to_string();
    let name = Shape::DnsName.to_string();
    let image = Shape::PinnedImage.to_string();
    let servers = Shape::Servers.to_string();
    let long = "a".repeat(64);
    let digest = "7369b033550a14997e5d698677b7b51f0210a80754f5d084c5caa1a78268267b";
    let cases: Vec<(&str, String, String)> = vec![
        (
            "KBF_FRONT_KUBE_CONTEXT",
            "a b".into(),
            format!("KBF_FRONT_KUBE_CONTEXT='a b' is not {}", Shape::KubeContext),
        ),
        (
            "KBF_FRONT_NAMESPACE",
            "Kbf".into(),
            format!("KBF_FRONT_NAMESPACE='Kbf' is not {label}"),
        ),
        (
            "KBF_FRONT_NAMESPACE",
            "-kbf".into(),
            format!("KBF_FRONT_NAMESPACE='-kbf' is not {label}"),
        ),
        (
            "KBF_FRONT_HOSTNAME",
            "kbf-".into(),
            format!("KBF_FRONT_HOSTNAME='kbf-' is not {label}"),
        ),
        (
            "KBF_FRONT_HOSTNAME",
            long.clone(),
            format!("KBF_FRONT_HOSTNAME='{long}' is not {label}"),
        ),
        (
            "KBF_FRONT_HOSTNAME",
            "kbf.a".into(),
            format!("KBF_FRONT_HOSTNAME='kbf.a' is not {label}"),
        ),
        (
            "KBF_FRONT_TAILNET_DNS",
            "tail0000.example".into(),
            format!(
                "KBF_FRONT_TAILNET_DNS='tail0000.example' is not {}",
                Shape::Tailnet
            ),
        ),
        (
            "KBF_FRONT_TAILNET_DNS",
            "a.b.ts.net".into(),
            format!(
                "KBF_FRONT_TAILNET_DNS='a.b.ts.net' is not {}",
                Shape::Tailnet
            ),
        ),
        (
            "KBF_TS_OPERATOR_NAMESPACE",
            "tail_scale".into(),
            format!("KBF_TS_OPERATOR_NAMESPACE='tail_scale' is not {label}"),
        ),
        (
            "KBF_TS_OPERATOR_SECRET",
            "a..b".into(),
            format!("KBF_TS_OPERATOR_SECRET='a..b' is not {name}"),
        ),
        (
            "KBF_FRONT_CA_SECRET",
            "ca.".into(),
            format!("KBF_FRONT_CA_SECRET='ca.' is not {name}"),
        ),
        (
            "KBF_SERVERS",
            "a.example:1  b.example:2".into(),
            format!("KBF_SERVERS='a.example:1  b.example:2' is not {servers}"),
        ),
        (
            "KBF_SERVERS",
            "a.example".into(),
            format!("KBF_SERVERS='a.example' is not {servers}"),
        ),
        (
            "KBF_SERVERS",
            "a.example:123456".into(),
            format!("KBF_SERVERS='a.example:123456' is not {servers}"),
        ),
        (
            "KBF_SERVERS",
            "a.example:".into(),
            format!("KBF_SERVERS='a.example:' is not {servers}"),
        ),
        (
            "KBF_SERVERS",
            "A.example:1".into(),
            format!("KBF_SERVERS='A.example:1' is not {servers}"),
        ),
        (
            "KBF_SERVERS",
            "a.example:0".into(),
            "KBF_SERVERS: port 0 of a.example is not from 1 to 65535".into(),
        ),
        (
            "KBF_SERVERS",
            "a.example:65536".into(),
            "KBF_SERVERS: port 65536 of a.example is not from 1 to 65535".into(),
        ),
        (
            "KBF_SERVERS",
            "a.example:1 a.example:2".into(),
            "KBF_SERVERS names a.example twice".into(),
        ),
        (
            "KBF_ENVOY_IMAGE",
            "envoyproxy/envoy:distroless-v1.39.3".into(),
            format!("KBF_ENVOY_IMAGE='envoyproxy/envoy:distroless-v1.39.3' is not {image}"),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("envoyproxy/envoy@sha256:{}", digest.to_uppercase()),
            format!(
                "KBF_ENVOY_IMAGE='envoyproxy/envoy@sha256:{}' is not {image}",
                digest.to_uppercase()
            ),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("envoyproxy/envoy@sha256:{}", &digest[1..]),
            format!(
                "KBF_ENVOY_IMAGE='envoyproxy/envoy@sha256:{}' is not {image}",
                &digest[1..]
            ),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("envoyproxy/envoy:a:b@sha256:{digest}"),
            format!("KBF_ENVOY_IMAGE='envoyproxy/envoy:a:b@sha256:{digest}' is not {image}"),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("envoyproxy/envoy:@sha256:{digest}"),
            format!("KBF_ENVOY_IMAGE='envoyproxy/envoy:@sha256:{digest}' is not {image}"),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("Envoyproxy/envoy@sha256:{digest}"),
            format!("KBF_ENVOY_IMAGE='Envoyproxy/envoy@sha256:{digest}' is not {image}"),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("@sha256:{digest}"),
            format!("KBF_ENVOY_IMAGE='@sha256:{digest}' is not {image}"),
        ),
    ];
    for (key, value, want) in cases {
        assert_eq!(refusal(&with(key, Some(&value))), want, "{key}={value}");
    }
}

/// Catches a renderer that runs with a key unset or empty (it would write an object
/// with an empty name), or that takes a typo'd or repeated key.
#[test]
fn a_missing_empty_unknown_or_repeated_key_is_refused() {
    assert_eq!(
        refusal(&with("KBF_FRONT_CA_SECRET", None)),
        "KBF_FRONT_CA_SECRET is not set (config file)"
    );
    assert_eq!(
        refusal(&with("KBF_ENVOY_IMAGE", Some(""))),
        "KBF_ENVOY_IMAGE is not set (config file)"
    );
    let typo = format!("{EXAMPLE}KBF_FRONT_HOST_NAME=kbf\n");
    assert!(
        refusal(&typo).ends_with("unknown key KBF_FRONT_HOST_NAME"),
        "{}",
        refusal(&typo)
    );
    let twice = format!("{EXAMPLE}KBF_FRONT_HOSTNAME=kbf2\n");
    assert!(
        refusal(&twice).contains("KBF_FRONT_HOSTNAME is already set on line"),
        "{}",
        refusal(&twice)
    );
}

/// Catches checks tighter than the first renderer's: a tag-less pinned image, a
/// server port with leading zeros, digits in a label, a one-label secret name and a
/// kube context with every allowed punctuation mark are all valid there.
#[test]
fn the_edges_of_each_shape_are_accepted() {
    let digest = "0123456789abcdef".repeat(4);
    let text = [
        ("KBF_FRONT_KUBE_CONTEXT", "Admin@a.b:c/d_e-F9".to_owned()),
        ("KBF_FRONT_HOSTNAME", "a".repeat(63)),
        ("KBF_FRONT_TAILNET_DNS", "t-0.ts.net".to_owned()),
        ("KBF_FRONT_CA_SECRET", "ca".to_owned()),
        (
            "KBF_SERVERS",
            "a.example:08980 b-2.example:65535".to_owned(),
        ),
        (
            "KBF_ENVOY_IMAGE",
            format!("r.example/x_y.z-w@sha256:{digest}"),
        ),
    ]
    .into_iter()
    .fold(EXAMPLE.to_owned(), |text, (key, value)| {
        let prefix = format!("{key}=");
        text.lines()
            .map(|l| {
                if l.starts_with(&prefix) {
                    format!("{key}={value}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect()
    });
    let c = EnvoyConfig::parse(&text).expect("valid");
    assert_eq!(
        c.servers,
        [
            Server {
                host: "a.example".into(),
                port: 8980
            },
            Server {
                host: "b-2.example".into(),
                port: 65535
            },
        ]
    );
    assert_eq!(c.image, format!("r.example/x_y.z-w@sha256:{digest}"));
    assert_eq!(c.kube_context, "Admin@a.b:c/d_e-F9");
    assert!(fits(
        Shape::PinnedImage,
        &format!("envoy:Tag_1.x-Y@sha256:{digest}")
    ));
    assert!(!fits(Shape::KubeContext, ""));
}

/// Catches `render` writing other files or names than the two the kit documents, or
/// reporting success when a file could not be written or the config not read.
#[test]
fn render_files_writes_the_two_files_and_refuses_what_it_cannot_do() {
    let dir = crate::testkit::scratch("envoy_render_files");
    let config = dir.join("config.local");
    std::fs::write(&config, EXAMPLE).expect("config");
    let out = dir.join("rendered");
    let line = render_files(&config, &out).expect("rendered");
    assert_eq!(
        line,
        format!("rendered manifests.json envoy.json into {}", out.display())
    );
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .expect("out")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["envoy.json", "manifests.json"]);
    let read = |n: &str| std::fs::read_to_string(out.join(n)).expect("read");
    same_bytes("envoy.json", &read("envoy.json"), ONE_ENVOY);
    same_bytes("manifests.json", &read("manifests.json"), ONE_MANIFESTS);

    let gone = render_files(&dir.join("missing"), &out)
        .err()
        .map(|e| e.to_string());
    assert!(gone.is_some_and(|e| e.starts_with("cannot read ")));

    let blocker = dir.join("a-file");
    std::fs::write(&blocker, "").expect("file");
    let not_dir = render_files(&config, &blocker).err().map(|e| e.to_string());
    let want = format!("cannot write {}: ", blocker.display());
    assert!(
        not_dir.as_deref().is_some_and(|e| e.starts_with(&want)),
        "{not_dir:?}"
    );

    let sub = dir.join("sub");
    std::fs::create_dir_all(sub.join("manifests.json")).expect("dir in the way");
    let in_the_way = render_files(&config, &sub).err().map(|e| e.to_string());
    let want = format!("cannot write {}: ", sub.join("manifests.json").display());
    assert!(
        in_the_way.as_deref().is_some_and(|e| e.starts_with(&want)),
        "{in_the_way:?}"
    );
}
