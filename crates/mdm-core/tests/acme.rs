//! The ACME settings the TLS verb reads.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::{acme, example, with, without};
use mdm_core::acme::{Acme, DnsProvider};
use mdm_core::config::ConfigError;

fn refusal(text: &str) -> String {
    Acme::parse(text).expect_err("refused").to_string()
}

/// Catches ACME settings read into the wrong field, resolvers lost or reordered, a
/// wait that is not in whole seconds, and the arguments lego is given.
#[test]
fn the_example_parses_into_its_fields_and_lego_arguments() {
    let a = acme();
    assert_eq!(a.email, "admin@example.com");
    assert_eq!(
        a.server.to_string(),
        "https://acme-v02.api.letsencrypt.org/directory"
    );
    assert_eq!(a.provider, DnsProvider::Cloudflare);
    let want: Vec<SocketAddr> = ["192.0.2.53:53", "198.51.100.53:53"]
        .iter()
        .map(|s| s.parse().expect("an address"))
        .collect();
    assert_eq!(a.resolvers, want);
    assert_eq!(a.propagation_wait, Duration::from_secs(300));
    assert_eq!(a.resolvers_arg(), "192.0.2.53:53,198.51.100.53:53");
    assert_eq!(a.propagation_wait_arg(), "300s");
}

/// Catches a provider name that maps to the wrong provider or variables, so a unit
/// would run lego against one DNS API with another's credentials.
#[test]
fn providers_are_named_and_know_their_variables() {
    let all = [
        ("godaddy", DnsProvider::GoDaddy, "GODADDY_API_KEY"),
        ("route53", DnsProvider::Route53, "AWS_HOSTED_ZONE_ID"),
        (
            "cloudflare",
            DnsProvider::Cloudflare,
            "CLOUDFLARE_DNS_API_TOKEN",
        ),
    ];
    for (name, provider, var) in all {
        assert_eq!(DnsProvider::named(name), Some(provider));
        assert_eq!(provider.to_string(), name);
        assert!(provider.vars().contains(&var), "{name}");
        let parsed = Acme::parse(&with("MDM_ACME_DNS_PROVIDER", name)).expect("valid");
        assert_eq!(parsed.provider, provider);
    }
    assert_eq!(DnsProvider::named("Godaddy"), None);
    assert_eq!(DnsProvider::GoDaddy.vars().len(), 2);
    assert_eq!(DnsProvider::Route53.vars().len(), 4);
}

/// Each row is one planted bad ACME setting; the parser must refuse it naming the key.
#[test]
fn a_bad_acme_setting_is_refused_naming_its_key() {
    let cases = [
        ("MDM_ACME_EMAIL", "admin"),
        ("MDM_ACME_EMAIL", "-x@example.com"),
        ("MDM_ACME_EMAIL", "@example.com"),
        ("MDM_ACME_EMAIL", "a@b@example.com"),
        ("MDM_ACME_EMAIL", "a@localhost"),
        ("MDM_ACME_EMAIL", "a b@example.com"),
        ("MDM_ACME_EMAIL", "a%n@example.com"),
        ("MDM_ACME_SERVER", "http://acme.example.com/dir"),
        ("MDM_ACME_SERVER", "https://acme.example.com/dir?x=1"),
        ("MDM_ACME_SERVER", "https://user@acme.example.com/dir"),
        ("MDM_ACME_DNS_PROVIDER", "Godaddy"),
        ("MDM_ACME_RESOLVERS", "resolver.example.com"),
        ("MDM_ACME_RESOLVERS", "192.0.2.53:53 192.0.2.54"),
        ("MDM_ACME_RESOLVERS", "192.0.2.53:0"),
        ("MDM_ACME_RESOLVERS", "[fe80::1%2]:53"),
        ("MDM_ACME_RESOLVERS", "192.0.2.53:53 [fe80::1%2]:53"),
        ("MDM_ACME_RESOLVERS", "\"  \""),
        ("MDM_ACME_PROPAGATION_WAIT", "300"),
        ("MDM_ACME_PROPAGATION_WAIT", "s"),
        ("MDM_ACME_PROPAGATION_WAIT", "0s"),
        ("MDM_ACME_PROPAGATION_WAIT", "+5s"),
        ("MDM_ACME_PROPAGATION_WAIT", "1x0s"),
        ("MDM_ACME_PROPAGATION_WAIT", "99999999999999999999s"),
    ];
    for (key, value) in cases {
        let got = refusal(&with(key, value));
        assert!(got.starts_with(&format!("{key}=")), "{key}={value}: {got}");
    }
    let ok = [
        ("MDM_ACME_EMAIL", "first.last+tag@mail.example.com"),
        ("MDM_ACME_SERVER", "https://acme.example.com:14000/dir"),
        ("MDM_ACME_RESOLVERS", "[2001:db8::53]:53"),
        ("MDM_ACME_PROPAGATION_WAIT", "1s"),
    ];
    for (key, value) in ok {
        Acme::parse(&with(key, value)).unwrap_or_else(|e| panic!("{key}={value}: {e}"));
    }
}

/// Catches ACME keys that may be left out (the TLS verb would run lego with a hole in
/// its arguments), and a settings file with a typo'd key.
#[test]
fn a_missing_acme_key_or_an_unknown_key_is_refused() {
    for key in [
        "MDM_ACME_EMAIL",
        "MDM_ACME_SERVER",
        "MDM_ACME_DNS_PROVIDER",
        "MDM_ACME_RESOLVERS",
        "MDM_ACME_PROPAGATION_WAIT",
    ] {
        assert_eq!(
            refusal(&without(key)),
            format!("{key} is not set (settings file)")
        );
    }
    let got = Acme::parse(&format!("{}MDM_ACME_EMAILS=x\n", example()));
    assert!(matches!(got, Err(ConfigError::Settings(_))));
}
