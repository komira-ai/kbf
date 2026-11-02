//! Shared fixtures. `config.example` is the neutral site every golden is rendered from.

#![allow(dead_code)]

use std::net::Ipv4Addr;

use mdm_core::acme::Acme;
use mdm_core::config::MdmConfig;

/// The documented example settings as committed: every key, with `MDM_BIND` and
/// `MDM_LAN_CIDR` left empty (a public file carries no private address).
pub const EXAMPLE_FILE: &str = include_str!("../../config.example");

/// An IPv4 address as text, built at run time so that no source file spells a private one.
pub fn ip(a: u8, b: u8, c: u8, d: u8) -> String {
    Ipv4Addr::new(a, b, c, d).to_string()
}

/// The example site's bind address: host 10 of the first /24 of the private 10/8 block.
pub fn bind() -> String {
    ip(10, 0, 0, 10)
}

/// The example site's LAN: the /24 of [`bind`].
pub fn lan() -> String {
    format!("{}/24", ip(10, 0, 0, 0))
}

/// `text` with the `@TEST_BIND@` and `@TEST_LAN@` of a committed fixture replaced by
/// [`bind`] and [`lan`].
pub fn site_addresses(text: &str) -> String {
    text.replace("@TEST_BIND@", &bind())
        .replace("@TEST_LAN@", &lan())
}

/// The documented example settings, every key set: [`EXAMPLE_FILE`] with the example
/// site's [`bind`] and [`lan`].
pub fn example() -> String {
    let text = EXAMPLE_FILE
        .replace("\nMDM_BIND=\n", &format!("\nMDM_BIND={}\n", bind()))
        .replace("\nMDM_LAN_CIDR=\n", &format!("\nMDM_LAN_CIDR={}\n", lan()));
    assert_ne!(text, EXAMPLE_FILE, "the example leaves the addresses empty");
    text
}

/// The example, parsed.
pub fn config() -> MdmConfig {
    MdmConfig::parse(&example()).expect("the example is valid")
}

/// The example's ACME settings.
pub fn acme() -> Acme {
    Acme::parse(&example()).expect("the example is valid")
}

/// The keys the example sets, in file order.
pub fn example_keys() -> Vec<&'static str> {
    EXAMPLE_FILE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect()
}

/// The example with the line of `key` replaced by `KEY=value`.
pub fn with(key: &str, value: &str) -> String {
    let mut seen = false;
    let text: String = example()
        .lines()
        .map(|l| match l.split_once('=') {
            Some((k, _)) if k == key => {
                seen = true;
                format!("{key}={value}\n")
            }
            _ => format!("{l}\n"),
        })
        .collect();
    assert!(seen, "the example sets {key}");
    text
}

/// The example without the line of `key`.
pub fn without(key: &str) -> String {
    let example = example();
    let text: String = example
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k != key))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_ne!(text.len(), example.len(), "the example sets {key}");
    text
}

/// The example with only the keys that have no default: what a site must set.
pub fn minimal() -> String {
    const REQUIRED: [&str; 16] = [
        "MDM_DNS_NAME",
        "MDM_BIND",
        "MDM_LAN_CIDR",
        "MDM_USER",
        "MDM_STATE",
        "MDM_ETC",
        "MDM_BIN",
        "MDM_NANOHUB_PORT",
        "MDM_SCEP_PORT",
        "MDM_DEP_PORT",
        "MDM_MEMORY_MAX",
        "MDM_CPU_QUOTA",
        "MDM_TASKS_MAX",
        "MDM_DEP_NAME",
        "MDM_ORG",
        "MDM_PROFILE_ID",
    ];
    example()
        .lines()
        .filter(|l| {
            l.split_once('=')
                .is_some_and(|(k, _)| REQUIRED.contains(&k))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

/// A file under `tests/`.
pub fn read(path: &str) -> String {
    let full = format!("{}/tests/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

/// A file under `tests/`, as bytes.
pub fn read_bytes(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}
