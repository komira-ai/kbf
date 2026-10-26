//! The rules of the `[proxy]` table about the children: `[[frontend]]` tables, the removed
//! `upstream` table and `admin_listen`.

use super::*;

const HEAD: &str = r#"
operator_token_file = "/etc/kbf/operator.token"
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file = "/etc/kbf/tls/server.key"
client_ca_file = "/etc/kbf/tls/ca.pem"
"#;

/// The table of the removed two-slot supervision.
const UPSTREAM: &str = "[upstream]\nbase_port = 18000\n";

/// One `[[frontend]]` table.
fn frontend(name: &str, base: u16) -> String {
    format!("[[frontend]]\nname = \"{name}\"\nbase_port = {base}\n")
}

/// A pool table of frontends `a` and `b`, with `top` (keys of `[proxy]` itself) before it.
fn pool(top: &str) -> String {
    let (a, b) = (frontend("a", 18_000), frontend("b", 18_010));
    format!("{top}{HEAD}{a}{b}")
}

/// The reason `text` is refused.
fn refused(text: &str) -> String {
    let error = ProxyConfig::from_toml_str(text).expect_err("refused");
    error.to_string()
}

/// Asserts that a table of `[[frontend]]` tables is read: the frontends are in order with their
/// names and base ports, and without `admin_listen` there is no admin API.
#[test]
fn a_pool_table_is_read_with_its_frontends() {
    let config = ProxyConfig::from_toml_str(&pool("")).expect("valid");
    assert_eq!(config.admin_listen, None);
    let names: Vec<&str> = config.frontend.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    let bases: Vec<u16> = config.frontend.iter().map(|f| f.base_port).collect();
    assert_eq!(bases, [18_000, 18_010]);
}

/// Asserts that the `upstream` table of the removed two-slot supervision is refused with the
/// words that say what replaced it, alone, beside frontends and beside `admin_listen`, and that
/// a table that gives no frontend and no `upstream` table is refused for the frontends it lacks.
#[test]
fn the_upstream_table_is_refused_and_the_frontends_are_needed() {
    let alone = refused(&format!("{HEAD}{UPSTREAM}"));
    assert_eq!(alone, SLOT_MODE_REMOVED);
    assert!(alone.contains("[[proxy.frontend]]"), "{alone}");
    let both = refused(&format!("{}{UPSTREAM}", pool("")));
    assert_eq!(both, SLOT_MODE_REMOVED);
    let admin = format!("admin_listen = \"127.0.0.1:18090\"\n{HEAD}{UPSTREAM}");
    assert_eq!(refused(&admin), SLOT_MODE_REMOVED);
    let neither = refused(HEAD);
    assert!(neither.contains("are needed"), "{neither}");
}

/// Asserts that a table with the `upstream` table, alone or beside frontends, is accepted by
/// `from_value` and `check` (which `setup` and the release fetcher read an older file with),
/// written back whole, and refused only by `check_pool` and `from_toml_str`, which are the
/// proxy's start.
#[test]
fn the_upstream_table_is_read_and_written_back_and_refused_only_for_serving() {
    for text in [
        format!("{HEAD}{UPSTREAM}"),
        format!("{}{UPSTREAM}", pool("")),
    ] {
        let table: toml::Value = toml::from_str(&text).expect("toml");
        let config = ProxyConfig::from_value(table).expect("read");
        assert_eq!(config.upstream, Some(UpstreamTable { base_port: 18_000 }));
        assert_eq!(config.check(), Ok(()));
        let written = toml::to_string(&config).expect("serializes");
        assert!(
            written.contains("[upstream]\nbase_port = 18000\n"),
            "{written}"
        );
        let again: ProxyConfig = toml::from_str(&written).expect("parses");
        assert_eq!(again, config);
        assert_eq!(
            config.check_pool(),
            Err(ConfigError::Invalid(SLOT_MODE_REMOVED))
        );
        assert_eq!(refused(&text), SLOT_MODE_REMOVED);
    }
}

/// Asserts that a frontend name is 1 to 64 letters, digits, `-` and `_`, and that two
/// frontends do not share one.
#[test]
fn frontend_names_are_plain_and_unique() {
    let long = "a".repeat(MAX_NAME_BYTES + 1);
    for bad in ["", "a b", "a:b", "a/b", "a.b", long.as_str()] {
        let text = format!("{HEAD}{}", frontend(bad, 18_000));
        let why = refused(&text);
        assert!(why.contains("frontend.name"), "{bad:?}: {why}");
    }
    let longest = "a".repeat(MAX_NAME_BYTES);
    for good in ["a", "A-b_9", longest.as_str()] {
        let text = format!("{HEAD}{}", frontend(good, 18_000));
        assert!(ProxyConfig::from_toml_str(&text).is_ok(), "{good}");
    }
    let (first, second) = (frontend("a", 18_000), frontend("a", 18_010));
    let why = refused(&format!("{HEAD}{first}{second}"));
    assert!(why.contains("unique"), "{why}");
}

/// Asserts that a frontend's five ports are real ports from 1024 up, and that the base ports
/// of two frontends are 5 apart at the least, in whatever order they are written.
#[test]
fn frontend_ports_leave_room_for_five_listeners_each() {
    let ok = |top: u16, low: u16| {
        let (a, b) = (frontend("a", top), frontend("b", low));
        ProxyConfig::from_toml_str(&format!("{HEAD}{a}{b}")).is_ok()
    };
    assert!(ok(18_005, 18_000), "exactly 5 apart, high first");
    assert!(!ok(18_004, 18_000), "4 apart");
    assert!(!ok(18_000, 18_000), "the same base");
    assert!(ok(MAX_FRONTEND_PORT, 1_024), "both ends");
    assert_eq!(MAX_FRONTEND_PORT, 65_531);
    assert!(!ok(MAX_FRONTEND_PORT + 1, 18_000), "too high");
    assert!(!ok(18_000, 1_023), "below 1024");
    let why = refused(&format!("{HEAD}{}", frontend("a", 80)));
    assert!(why.contains("frontend.base_port"), "{why}");
    let (a, b) = (frontend("a", 18_000), frontend("b", 18_002));
    let why = refused(&format!("{HEAD}{a}{b}"));
    assert!(why.contains("5 apart"), "{why}");
}

/// Asserts that `admin_listen` is read as a socket address with a port, in the text of
/// either family, and that a refusal names the key and never the value.
#[test]
fn admin_listen_is_a_loopback_socket_address() {
    let with = |value: &str| {
        let text = pool(&format!("admin_listen = {value}\n"));
        ProxyConfig::from_toml_str(&text)
    };
    let v4 = with("\"127.0.0.1:18090\"").expect("v4");
    assert_eq!(v4.admin_listen, Some("127.0.0.1:18090".parse().unwrap()));
    let v6 = with("\"[::1]:18090\"").expect("v6");
    assert_eq!(v6.admin_listen, Some("[::1]:18090".parse().unwrap()));
    for bad in [
        "\"localhost:18090\"",
        "\"127.0.0.1\"",
        "\"127.0.0.1:0\"",
        "8090",
    ] {
        // Through `from_value`, as `installer.toml` is read: `from_toml_str` adds the source
        // line to a parse error.
        let text = pool(&format!("admin_listen = {bad}\n"));
        let table: toml::Value = toml::from_str(&text).expect("toml");
        let why = ProxyConfig::from_value(table)
            .expect_err("refused")
            .to_string();
        assert!(why.contains("admin_listen"), "{bad}: {why}");
        assert!(!why.contains("127.0.0.1"), "{bad}: {why} echoes it");
    }
}

/// Asserts that `admin_listen` on an address that is not loopback is refused (nothing on that
/// socket asks for a credential), whatever the family.
#[test]
fn admin_listen_must_be_loopback() {
    for open in ["192.0.2.1:18090", "0.0.0.0:18090", "[2001:db8::1]:18090"] {
        let why = refused(&pool(&format!("admin_listen = \"{open}\"\n")));
        assert!(why.contains("loopback"), "{open}: {why}");
    }
}

/// Asserts that `admin_listen` is refused on any of the five ports of a frontend (first and
/// last of each) and accepted on the ports just outside them.
#[test]
fn admin_listen_must_not_use_a_frontend_port() {
    let with = |port: u16| pool(&format!("admin_listen = \"127.0.0.1:{port}\"\n"));
    for taken in [18_000, 18_004, 18_010, 18_014] {
        let why = refused(&with(taken));
        assert!(
            why.contains("not use a port of a frontend"),
            "{taken}: {why}"
        );
    }
    for free in [17_999, 18_005, 18_009, 18_015] {
        assert!(ProxyConfig::from_toml_str(&with(free)).is_ok(), "{free}");
    }
}

/// Asserts that a table written back out (as `setup` rewrites `installer.toml`) reads as the
/// same pool table, frontends and admin address included.
#[test]
fn a_pool_table_serializes_back_to_itself() {
    let text = pool("admin_listen = \"127.0.0.1:18090\"\n");
    let config = ProxyConfig::from_toml_str(&text).expect("valid");
    assert_eq!(config.frontend.len(), 2);
    let again = toml::to_string(&config).expect("serializes");
    assert_eq!(ProxyConfig::from_toml_str(&again).expect("valid"), config);
}

/// Asserts that a key a frontend table does not have is refused, as everywhere in the table.
#[test]
fn a_frontend_with_an_unknown_key_is_refused() {
    let text = format!("{HEAD}[[frontend]]\nname = \"a\"\nbase_port = 18000\nweight = 2\n");
    assert!(matches!(
        ProxyConfig::from_toml_str(&text),
        Err(ConfigError::Parse(_))
    ));
}
