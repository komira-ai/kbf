//! The settings: the documented example, the defaults and every refusal.

mod common;

use std::net::Ipv4Addr;

use common::{config, example, example_keys, ip, lan, minimal, with, without};
use mdm_core::config::{Cidr, ConfigError, DEFAULT_SKIP_ITEMS, KEYS, MdmConfig, lan_address};
use mdm_core::settings::SettingsError;

fn refusal(text: &str) -> String {
    MdmConfig::parse(text).expect_err("refused").to_string()
}

/// Catches an example that leaves a key undocumented (the operator never learns it
/// exists) or documents one the parser refuses, a key set in a different order than the
/// closed list, and settings read into the wrong field.
#[test]
fn the_example_documents_every_key_and_parses_into_its_fields() {
    assert_eq!(example_keys(), KEYS);
    let c = config();
    assert_eq!(c.dns_name, "mdm.example.com");
    assert_eq!(
        (c.bind, c.lan.addr, c.lan.bits),
        (Ipv4Addr::new(10, 0, 0, 10), Ipv4Addr::new(10, 0, 0, 0), 24)
    );
    assert_eq!(c.lan.to_string(), lan());
    assert_eq!(c.user, "js-mdm");
    let paths = (c.state.as_str(), c.etc.as_str(), c.bin.as_str());
    assert_eq!(paths, ("/var/lib/js-mdm", "/etc/js-mdm", "/opt/js-mdm/bin"));
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
        (c.unit_prefix.as_str(), c.slice.as_str()),
        ("js-mdm", "js-mdm.slice")
    );
    assert_eq!(
        (c.dep_name.as_str(), c.org.as_str(), c.profile_id.as_str()),
        ("example", "Example", "com.example.mdm")
    );
    assert_eq!(c.profile_display_name, "Example MDM");
    assert_eq!(c.ade.profile_name, "Example MDM");
    assert_eq!(c.ade.org_magic, "js-mdm");
    assert_eq!(
        (c.ade.language.as_str(), c.ade.region.as_str()),
        ("en", "US")
    );
    assert_eq!(c.ade.skip_items, DEFAULT_SKIP_ITEMS);
    assert_eq!(c.scep_ca.common_name, "Example MDM SCEP CA");
    assert_eq!(c.scep_ca.organizational_unit, "js-mdm");
    assert_eq!(c.scep_ca.years, 10);
    assert_eq!(c.dep_cert.common_name, "js-mdm");
    assert_eq!(c.dep_cert.days, 7);
    assert_eq!(c.signature_namespace, "js-mdm-erase");
}

/// Catches a default that drifted from what `config.example` says it is: a file with
/// only the keys that have no default must read as the fully written example.
#[test]
fn the_defaults_are_the_values_the_example_documents() {
    let defaulted = MdmConfig::parse(&minimal()).expect("the required keys are enough");
    assert_eq!(defaulted, config());
}

/// Catches a default that ignores the unit prefix or the organisation (a site that sets
/// only those would get another site's names), and a key that is read but not applied.
#[test]
fn the_derived_defaults_follow_the_prefix_and_the_organisation() {
    let text = format!(
        "{}MDM_UNIT_PREFIX=acme-mdm\nMDM_ORG=Acme Labs\n",
        minimal().replace("MDM_ORG=Example\n", "")
    );
    let c = MdmConfig::parse(&text).expect("valid");
    assert_eq!(
        (c.unit_prefix.as_str(), c.slice.as_str()),
        ("acme-mdm", "acme-mdm.slice")
    );
    assert_eq!(c.profile_display_name, "Acme Labs MDM");
    assert_eq!(c.ade.profile_name, "Acme Labs MDM");
    assert_eq!(c.ade.org_magic, "acme-mdm");
    assert_eq!(c.scep_ca.common_name, "Acme Labs MDM SCEP CA");
    assert_eq!(c.scep_ca.organizational_unit, "acme-mdm");
    assert_eq!(c.dep_cert.common_name, "acme-mdm");
    assert_eq!(c.signature_namespace, "acme-mdm-erase");
}

/// Catches a new key that is accepted and then ignored: each is set to a value that is
/// not its default and must arrive in its field.
#[test]
fn every_new_key_can_be_set_away_from_its_default() {
    let mut text = minimal();
    for line in [
        "MDM_UNIT_PREFIX=site-a",
        "MDM_SLICE=site-a-b.slice",
        "MDM_PROFILE_DISPLAY_NAME=Site A Macs",
        "MDM_ADE_PROFILE_NAME=Site A enrollment",
        "MDM_ADE_ORG_MAGIC=site.a_1",
        "MDM_ADE_SKIP_ITEMS=\"Siri  TOS\"",
        "MDM_LANGUAGE=de",
        "MDM_REGION=DE",
        "MDM_SCEP_CA_CN=Site A CA",
        "MDM_SCEP_CA_OU=Site A OU",
        "MDM_SCEP_CA_YEARS=30",
        "MDM_DEP_CERT_CN=site-a.cert",
        "MDM_DEP_CERT_DAYS=365",
        "MDM_SIGNATURE_NAMESPACE=site.a-erase",
    ] {
        text.push_str(line);
        text.push('\n');
    }
    let c = MdmConfig::parse(&text).expect("valid");
    assert_eq!(
        (c.unit_prefix.as_str(), c.slice.as_str()),
        ("site-a", "site-a-b.slice")
    );
    assert_eq!(c.profile_display_name, "Site A Macs");
    assert_eq!(c.ade.profile_name, "Site A enrollment");
    assert_eq!(c.ade.org_magic, "site.a_1");
    assert_eq!(c.ade.skip_items, ["Siri", "TOS"]);
    assert_eq!(
        (c.ade.language.as_str(), c.ade.region.as_str()),
        ("de", "DE")
    );
    assert_eq!(c.scep_ca.common_name, "Site A CA");
    assert_eq!(c.scep_ca.organizational_unit, "Site A OU");
    assert_eq!(c.scep_ca.years, 30);
    assert_eq!(c.dep_cert.common_name, "site-a.cert");
    assert_eq!(c.dep_cert.days, 365);
    assert_eq!(c.signature_namespace, "site.a-erase");
}

/// Each row is one planted bad setting; the parser must refuse it naming the key,
/// before any verb reads the host.
#[test]
fn a_bad_setting_is_refused_naming_its_key() {
    let long = "a".repeat(41);
    let long_user = "a".repeat(32);
    // Addresses are built at run time: a public file carries no private or public one.
    let public = ip(8, 8, 4, 4);
    let other_lan = ip(10, 2, 0, 1);
    let ten = |rest: &str| format!("{}{rest}", ip(10, 0, 0, 0));
    let (ten_7, ten_31, ten_99, ten_x) = (ten("/7"), ten("/31"), ten("/99"), ten("/x"));
    let ten_bare = ten("");
    let public_net = format!("{}/24", ip(8, 8, 4, 0));
    let wide_192 = format!("{}/8", ip(192, 168, 0, 0));
    let wide_172 = format!("{}/8", ip(172, 16, 0, 0));
    let wide_172_11 = format!("{}/11", ip(172, 16, 0, 0));
    let cases = [
        ("MDM_DNS_NAME", "MDM.example.com"),
        ("MDM_DNS_NAME", "localhost"),
        ("MDM_DNS_NAME", "-a.example.com"),
        ("MDM_DNS_NAME", "a-.example.com"),
        ("MDM_DNS_NAME", "a..com"),
        ("MDM_DNS_NAME", "a_b.example.com"),
        ("MDM_BIND", "0.0.0.0"),
        ("MDM_BIND", "127.0.0.1"),
        ("MDM_BIND", public.as_str()),
        ("MDM_BIND", "not-an-address"),
        ("MDM_BIND", other_lan.as_str()),
        ("MDM_LAN_CIDR", ten_7.as_str()),
        ("MDM_LAN_CIDR", ten_31.as_str()),
        ("MDM_LAN_CIDR", ten_99.as_str()),
        ("MDM_LAN_CIDR", ten_x.as_str()),
        ("MDM_LAN_CIDR", ten_bare.as_str()),
        ("MDM_LAN_CIDR", public_net.as_str()),
        ("MDM_USER", "Js"),
        ("MDM_USER", "1js"),
        ("MDM_USER", "js mdm"),
        ("MDM_USER", long_user.as_str()),
        ("MDM_STATE", "/var/lib/js mdm"),
        ("MDM_ETC", "etc/js-mdm"),
        ("MDM_BIN", "/opt/../bin"),
        ("MDM_NANOHUB_PORT", "0"),
        ("MDM_SCEP_PORT", "65536"),
        ("MDM_DEP_PORT", "x"),
        ("MDM_MEMORY_MAX", "1T"),
        ("MDM_MEMORY_MAX", "G"),
        ("MDM_MEMORY_MAX", "01G"),
        ("MDM_CPU_QUOTA", "100"),
        ("MDM_CPU_QUOTA", "1x%"),
        ("MDM_TASKS_MAX", "0"),
        ("MDM_TASKS_MAX", "25%"),
        ("MDM_UNIT_PREFIX", "Js-mdm"),
        ("MDM_UNIT_PREFIX", "1js"),
        ("MDM_UNIT_PREFIX", "js_mdm"),
        ("MDM_UNIT_PREFIX", "js-"),
        ("MDM_UNIT_PREFIX", "js--mdm"),
        ("MDM_SLICE", "a--b.slice"),
        ("MDM_SLICE", "system.slice"),
        ("MDM_SLICE", "user.slice"),
        ("MDM_SLICE", "machine.slice"),
        ("MDM_SLICE", "init.slice"),
        ("MDM_SLICE", "system-js.slice"),
        ("MDM_SLICE", "-.slice"),
        ("MDM_SLICE", "other.slice"),
        ("MDM_SLICE", "js-mdmx.slice"),
        ("MDM_USER", "root"),
        ("MDM_USER", "toor"),
        ("MDM_LAN_CIDR", wide_192.as_str()),
        ("MDM_LAN_CIDR", wide_172.as_str()),
        ("MDM_LAN_CIDR", wide_172_11.as_str()),
        ("MDM_LAN_CIDR", ten_7.as_str()),
        ("MDM_DEP_NAME", "-h"),
        ("MDM_DEP_NAME", "-"),
        ("MDM_DEP_NAME", "."),
        ("MDM_DEP_NAME", ".."),
        ("MDM_DEP_CERT_CN", "-x"),
        ("MDM_DEP_CERT_CN", ".x"),
        ("MDM_ADE_ORG_MAGIC", "-x"),
        ("MDM_ADE_ORG_MAGIC", ".x"),
        ("MDM_SIGNATURE_NAMESPACE", "-x"),
        ("MDM_SIGNATURE_NAMESPACE", ".x"),
        ("MDM_SLICE", "a-.slice"),
        ("MDM_UNIT_PREFIX", long.as_str()),
        ("MDM_SLICE", "js-mdm"),
        ("MDM_SLICE", ".slice"),
        ("MDM_SLICE", "-js.slice"),
        ("MDM_SLICE", "Js.slice"),
        ("MDM_SLICE", "js mdm.slice"),
        ("MDM_DEP_NAME", "a b"),
        ("MDM_DEP_NAME", "a%n"),
        ("MDM_ORG", "a\u{7}b"),
        ("MDM_ORG", ""),
        ("MDM_PROFILE_ID", "Com.Example"),
        ("MDM_PROFILE_ID", "js-mdm"),
        ("MDM_PROFILE_DISPLAY_NAME", "a\u{7}b"),
        ("MDM_PROFILE_DISPLAY_NAME", ""),
        ("MDM_ADE_PROFILE_NAME", "a\u{7}b"),
        ("MDM_ADE_ORG_MAGIC", "a b"),
        ("MDM_ADE_ORG_MAGIC", "a/b"),
        ("MDM_ADE_SKIP_ITEMS", "   "),
        ("MDM_ADE_SKIP_ITEMS", "Siri Sk-ip"),
        ("MDM_LANGUAGE", "EN"),
        ("MDM_LANGUAGE", "e"),
        ("MDM_LANGUAGE", "eng"),
        ("MDM_LANGUAGE", "e1"),
        ("MDM_REGION", "us"),
        ("MDM_REGION", "U"),
        ("MDM_REGION", "USA"),
        ("MDM_SCEP_CA_CN", "a\u{7}b"),
        ("MDM_SCEP_CA_OU", "a\u{7}b"),
        ("MDM_SCEP_CA_YEARS", "0"),
        ("MDM_SCEP_CA_YEARS", "31"),
        ("MDM_SCEP_CA_YEARS", "x"),
        ("MDM_SCEP_CA_YEARS", "-1"),
        ("MDM_DEP_CERT_CN", "a&b"),
        ("MDM_DEP_CERT_CN", "a b"),
        ("MDM_DEP_CERT_DAYS", "0"),
        ("MDM_DEP_CERT_DAYS", "366"),
        ("MDM_SIGNATURE_NAMESPACE", "a b"),
    ];
    for (key, value) in cases {
        let got = refusal(&with(key, value));
        assert!(got.starts_with(key), "{key}={value:?}: {got:?}");
        let short = key.strip_prefix("MDM_").expect("prefix");
        let line = format!("MDM_{short}={value}");
        assert!(
            got.starts_with(&line) || value.is_empty(),
            "{line}: {got:?}"
        );
    }
}

/// Catches a refusal that names the wrong key or says nothing of the cause: a few
/// complete messages, one for each way a value can be wrong.
#[test]
fn refusals_say_which_key_and_why() {
    let other_lan = ip(10, 2, 0, 1);
    assert_eq!(
        refusal(&with("MDM_BIND", &other_lan)),
        format!(
            "MDM_BIND={other_lan}: not inside MDM_LAN_CIDR (a container bridge's or another network's address?)"
        )
    );
    assert_eq!(
        refusal(&with("MDM_SCEP_CA_YEARS", "31")),
        "MDM_SCEP_CA_YEARS=31: not a whole number in the allowed range"
    );
    assert_eq!(
        refusal(&with("MDM_SLICE", "js-mdm")),
        "MDM_SLICE=js-mdm: not a slice name: lower-case letters, digits and - from a letter, then .slice"
    );
    assert_eq!(
        refusal(&without("MDM_DNS_NAME")),
        "MDM_DNS_NAME is not set (settings file)"
    );
}

/// Catches a required key that may be left out or empty, and a defaulted key that an
/// empty value would turn into the default.
#[test]
fn a_missing_required_key_or_an_empty_value_is_refused() {
    for key in [
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
    ] {
        assert_eq!(
            refusal(&without(key)),
            format!("{key} is not set (settings file)")
        );
        assert_eq!(
            refusal(&with(key, "")),
            format!("{key} is not set (settings file)")
        );
    }
    for key in [
        "MDM_UNIT_PREFIX",
        "MDM_SLICE",
        "MDM_ADE_ORG_MAGIC",
        "MDM_SCEP_CA_YEARS",
        "MDM_LANGUAGE",
    ] {
        assert_eq!(
            refusal(&with(key, "")),
            format!("{key} is not set (settings file)")
        );
    }
}

/// Catches a file with a typo'd key or a repeated key being read: the settings are a
/// closed set, so the error is the settings layer's.
#[test]
fn unknown_and_repeated_keys_are_refused() {
    let unknown = format!("{}MDM_ORGS=x\n", example());
    assert!(matches!(
        MdmConfig::parse(&unknown),
        Err(ConfigError::Settings(SettingsError::Unknown { .. }))
    ));
    let repeated = format!("{}MDM_ORG=x\n", example());
    assert!(matches!(
        MdmConfig::parse(&repeated),
        Err(ConfigError::Settings(SettingsError::Repeated { .. }))
    ));
}

/// Catches a network check that compares the wrong bits (an address just outside a /24
/// must be outside, the network's own edges inside) and a private-range check that
/// takes a neighbouring range.
#[test]
fn an_address_is_inside_a_network_by_its_prefix_bits() {
    let net = Cidr {
        addr: Ipv4Addr::new(10, 1, 0, 0),
        bits: 24,
    };
    assert!(net.contains(Ipv4Addr::new(10, 1, 0, 255)));
    assert!(net.contains(Ipv4Addr::new(10, 1, 0, 1)));
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
/// inside a DNS label, a user name with `_`, `-` and digits, a unit prefix with digits.
#[test]
fn names_with_hyphens_digits_and_underscores_are_taken() {
    let text = format!(
        "{}MDM_UNIT_PREFIX=js-mdm-2\n",
        with("MDM_DNS_NAME", "mdm-1.example2.com")
            .replace("MDM_USER=js-mdm", "MDM_USER=_js_mdm-2")
            .replace("MDM_UNIT_PREFIX=js-mdm\n", "")
            .replace("MDM_SLICE=js-mdm.slice\n", "")
    );
    let c = MdmConfig::parse(&text).expect("valid");
    assert_eq!(
        (c.dns_name.as_str(), c.user.as_str(), c.slice.as_str()),
        ("mdm-1.example2.com", "_js_mdm-2", "js-mdm-2.slice")
    );
}

/// Catches directories that overlap: the state is the service user's to write, so a
/// binary directory inside it (or equal to it, or around it) would let the user replace
/// pinned binaries, and the same for the unit inputs. Each row sets one path; the
/// refusal names the key that clashes. A name that only shares a prefix is not nested.
#[test]
fn directories_that_equal_nest_or_surround_each_other_are_refused() {
    let cases = [
        ("MDM_ETC", "/var/lib/js-mdm", "MDM_ETC"),
        ("MDM_ETC", "/var/lib/js-mdm/etc", "MDM_ETC"),
        ("MDM_ETC", "/var", "MDM_ETC"),
        ("MDM_BIN", "/var/lib/js-mdm", "MDM_BIN"),
        ("MDM_BIN", "/var/lib/js-mdm/bin", "MDM_BIN"),
        ("MDM_BIN", "/etc/js-mdm", "MDM_BIN"),
        ("MDM_BIN", "/etc/js-mdm/bin", "MDM_BIN"),
        ("MDM_STATE", "/opt", "MDM_BIN"),
        ("MDM_STATE", "/etc/js-mdm/state", "MDM_ETC"),
    ];
    for (key, value, clash) in cases {
        let got = refusal(&with(key, value));
        assert!(
            got.starts_with(&format!("{clash}=")),
            "{key}={value}: {got}"
        );
        assert!(got.contains("equal to, inside or around MDM_"), "{got}");
    }
    let text = with("MDM_ETC", "/var/lib/js-mdm-etc");
    let c = MdmConfig::parse(&text).expect("a shared prefix is not nesting");
    assert_eq!(c.etc.as_str(), "/var/lib/js-mdm-etc");
}

/// Catches a network check that overflows or answers wrongly at the ends of the prefix
/// range: a /0 holds every address (a shift by 32 bits panics), a /32 only itself.
#[test]
fn a_network_of_zero_or_thirty_two_bits_does_not_overflow() {
    let any = Cidr {
        addr: Ipv4Addr::new(10, 0, 0, 0),
        bits: 0,
    };
    assert!(any.contains(Ipv4Addr::new(8, 8, 8, 8)));
    let one = Cidr {
        addr: Ipv4Addr::new(10, 0, 0, 7),
        bits: 32,
    };
    assert!(one.contains(Ipv4Addr::new(10, 0, 0, 7)));
    assert!(!one.contains(Ipv4Addr::new(10, 0, 0, 8)));
}

/// Catches a LAN network that widens past private space, and one that is refused though
/// it is exactly a private block: systemd reads the /8 of an address in 192.168/16 as the /8
/// of 192, so the
/// proxy would be allowed to talk to public addresses. The exact /8, /12 and /16 are
/// accepted, and so is a narrower network inside each.
#[test]
fn a_lan_network_stays_inside_its_private_block() {
    let net = |a: u8, b: u8, c: u8, d: u8, bits: u8| format!("{}/{bits}", ip(a, b, c, d));
    let ok = [
        (ip(10, 0, 0, 10), net(10, 0, 0, 0, 8)),
        (ip(10, 0, 0, 10), net(10, 0, 0, 8, 30)),
        (ip(172, 16, 0, 5), net(172, 16, 0, 0, 12)),
        (ip(172, 31, 255, 5), net(172, 16, 0, 0, 12)),
        (ip(172, 20, 1, 5), net(172, 20, 0, 0, 16)),
        (ip(192, 168, 1, 5), net(192, 168, 0, 0, 16)),
        (ip(192, 168, 1, 5), net(192, 168, 1, 0, 24)),
    ];
    for (bind, net) in ok {
        let text = with("MDM_BIND", &bind).replace(
            &format!("MDM_LAN_CIDR={}", lan()),
            &format!("MDM_LAN_CIDR={net}"),
        );
        let c = MdmConfig::parse(&text).unwrap_or_else(|e| panic!("{bind} {net}: {e}"));
        assert_eq!(c.lan.to_string(), net);
    }
}
