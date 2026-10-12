use super::*;
use crate::mdm::tests::{EXAMPLE, config, config_text, lan};

fn refusal(text: &str) -> String {
    MdmConfig::parse(text)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

/// `config_text()` with the line of `key` replaced by `KEY=value`.
fn with(key: &str, value: &str) -> String {
    config_text()
        .lines()
        .map(|l| match l.split_once('=') {
            Some((k, _)) if k == key => format!("{key}={value}\n"),
            _ => format!("{l}\n"),
        })
        .collect()
}

/// Catches an example that leaves a key undocumented (the operator never learns it
/// exists) or documents one the parser refuses, and settings read into the wrong field.
#[test]
fn the_example_documents_every_key_and_parses_into_its_fields() {
    let set: Vec<&str> = EXAMPLE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    assert_eq!(set, KEYS);
    let c = config();
    assert_eq!(c.dns_name, "mdm.example.com");
    assert_eq!((c.bind, c.lan.addr, c.lan.bits), (lan(10), lan(0), 24));
    assert_eq!(c.user, "kbf-mdm");
    let paths = (c.state.as_str(), c.etc.as_str(), c.bin.as_str());
    assert_eq!(
        paths,
        ("/var/lib/kbf-mdm", "/etc/kbf-mdm", "/opt/kbf-mdm/bin")
    );
    assert_eq!(
        (c.nanohub_port, c.scep_port, c.dep_port),
        (9004, 9005, 9001)
    );
    let caps = (
        c.memory_max.as_str(),
        c.cpu_quota.as_str(),
        c.tasks_max.as_str(),
    );
    assert_eq!(caps, ("1G", "100%", "256"));
    assert_eq!(
        (c.dep_name.as_str(), c.org.as_str()),
        ("example", "Example")
    );
    assert_eq!(c.lan.to_string(), format!("{}/24", lan(0)));
}

/// Each row is one planted bad setting the old kit's checks refused; the parser must
/// refuse it naming the key, before any verb reads the host.
#[test]
fn a_bad_setting_is_refused_naming_its_key() {
    let public = Ipv4Addr::new(8, 8, 4, 4).to_string();
    let other_lan = Ipv4Addr::new(10, 2, 0, 1).to_string();
    let cidr = |bits: u8| format!("{}/{bits}", lan(0));
    let cases = [
        with("MDM_DNS_NAME", "MDM.example.com"),
        with("MDM_DNS_NAME", "localhost"),
        with("MDM_DNS_NAME", "-a.example.com"),
        with("MDM_DNS_NAME", "a-.example.com"),
        with("MDM_DNS_NAME", "a..com"),
        with("MDM_DNS_NAME", "a_b.example.com"),
        with("MDM_BIND", "0.0.0.0"),
        with("MDM_BIND", "127.0.0.1"),
        with("MDM_BIND", &public),
        with("MDM_BIND", "not-an-address"),
        with("MDM_BIND", &other_lan),
        with("MDM_LAN_CIDR", &cidr(7)),
        with("MDM_LAN_CIDR", &cidr(31)),
        with("MDM_LAN_CIDR", &cidr(99)),
        with("MDM_LAN_CIDR", &lan(0).to_string()),
        with("MDM_LAN_CIDR", &format!("{public}/24")),
        with("MDM_USER", "Kbf"),
        with("MDM_USER", "kbf mdm"),
        with("MDM_USER", &"a".repeat(32)),
        with("MDM_STATE", "/var/lib/kbf mdm"),
        with("MDM_ETC", "etc/kbf-mdm"),
        with("MDM_BIN", "/opt/../bin"),
        with("MDM_NANOHUB_PORT", "0"),
        with("MDM_SCEP_PORT", "65536"),
        with("MDM_DEP_PORT", "x"),
        with("MDM_MEMORY_MAX", "1T"),
        with("MDM_MEMORY_MAX", "G"),
        with("MDM_MEMORY_MAX", "01G"),
        with("MDM_CPU_QUOTA", "100"),
        with("MDM_CPU_QUOTA", "1x%"),
        with("MDM_TASKS_MAX", "0"),
        with("MDM_TASKS_MAX", "25%"),
        with("MDM_DEP_NAME", "a b"),
        with("MDM_DEP_NAME", "a%n"),
        with("MDM_ORG", "a\u{7}b"),
        with("MDM_ORG", ""),
        with("MDM_ORGS", "x"),
    ];
    for text in cases {
        let changed: Vec<&str> = text
            .lines()
            .filter(|l| !config_text().lines().any(|g| g == *l))
            .collect();
        let line = changed.first().copied().unwrap_or_default();
        let key = line.split('=').next().unwrap_or_default();
        let got = refusal(&text);
        assert!(got.contains(key), "{line:?}: {got:?}");
    }
}

/// Catches a network check that compares the wrong bits: an address just outside a
/// /24 must be outside, and the network's own edges inside.
#[test]
fn an_address_is_inside_a_network_by_its_prefix_bits() {
    let net = Cidr {
        addr: lan(0),
        bits: 24,
    };
    assert!(net.contains(lan(255)));
    assert!(net.contains(lan(1)));
    assert!(!net.contains(Ipv4Addr::new(10, 1, 1, 0)));
    let wide = Cidr {
        addr: Ipv4Addr::new(10, 0, 0, 0),
        bits: 8,
    };
    assert!(wide.contains(Ipv4Addr::new(10, 255, 0, 1)));
    for (a, private) in [
        (Ipv4Addr::new(172, 16, 0, 1), true),
        (Ipv4Addr::new(172, 31, 255, 1), true),
        (Ipv4Addr::new(172, 32, 0, 1), false),
        (Ipv4Addr::new(192, 168, 0, 1), true),
        (Ipv4Addr::new(100, 64, 0, 1), false),
    ] {
        assert_eq!(lan_address(&a.to_string()).is_some(), private, "{a}");
    }
}

/// Catches a name check that refuses what systemd and DNS take: a hyphen or a digit
/// inside a DNS label, a user name with `_`, `-` and digits.
#[test]
fn names_with_hyphens_digits_and_underscores_are_taken() {
    let text = config_text()
        .replace(
            "MDM_DNS_NAME=mdm.example.com",
            "MDM_DNS_NAME=mdm-1.example2.com",
        )
        .replace("MDM_USER=kbf-mdm", "MDM_USER=_kbf_mdm-2");
    let c = MdmConfig::parse(&text).expect("valid");
    assert_eq!(
        (c.dns_name.as_str(), c.user.as_str()),
        ("mdm-1.example2.com", "_kbf_mdm-2")
    );
}
