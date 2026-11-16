use super::*;

fn node(text: &str) -> Result<Node, ConfigError> {
    text.parse()
}

fn key() -> Key {
    Key::new("AKIDTEST", "secret-0123456789").unwrap()
}

/// Asserts what a node address is made of: the host without brackets, the port, the
/// authority as written, and the URIs under `http` and `https`. Catches an IPv6 host kept
/// with its brackets, which TLS would take for a name and the connect could not resolve.
#[test]
fn a_node_is_a_host_and_a_port() {
    let v4 = node("192.0.2.5:9600").unwrap();
    assert_eq!(
        (v4.host(), v4.port(), v4.authority()),
        ("192.0.2.5", 9600, "192.0.2.5:9600")
    );
    assert_eq!(v4.uri(false).to_string(), "http://192.0.2.5:9600/");
    assert_eq!(v4.uri(true).to_string(), "https://192.0.2.5:9600/");
    assert_eq!(format!("{v4:?}"), "Node(\"192.0.2.5:9600\")");
    let v6 = node("[::1]:9600").unwrap();
    assert_eq!(
        (v6.host(), v6.port(), v6.authority()),
        ("::1", 9600, "[::1]:9600")
    );
    let name = node("node-05.farm:1").unwrap();
    assert_eq!((name.host(), name.port()), ("node-05.farm", 1));
}

/// Asserts an address that is not `host:port` alone is refused, naming why: no port, a port
/// of 0 or past 65535 or not a number, a scheme, a path, a user, no host, an IPv6 host
/// without brackets, text a URI cannot hold. Catches a label with a scheme or path being
/// dialled as something else.
#[test]
fn an_address_that_is_not_host_and_port_is_refused() {
    let cases = [
        ("node", "no port"),
        ("[::1]", "no port"),
        ("node:0", "the port is not 1 to 65535"),
        ("node:65536", "the port is not 1 to 65535"),
        ("node:http", "the port is not 1 to 65535"),
        ("http://node:1", "not host:port alone"),
        ("node:1/path", "not host:port alone"),
        ("user@node:1", "not host:port alone"),
        ("[::1]:0", "the port is not 1 to 65535"),
        (":9600", "no host"),
        ("::1:9600", "not host:port"),
        ("no de:1", "not host:port"),
        ("", "not host:port"),
        ("a*b:1", "the host is not a name or an address"),
        ("a$b:1", "the host is not a name or an address"),
    ];
    let got: Vec<(&str, String)> = cases
        .iter()
        .map(|(text, _)| match node(text) {
            Err(ConfigError::Address { address, reason }) => {
                assert_eq!(address, *text);
                (*text, reason.to_owned())
            }
            other => (*text, format!("{other:?}")),
        })
        .collect();
    let want: Vec<(&str, String)> = cases.iter().map(|(t, r)| (*t, (*r).to_owned())).collect();
    assert_eq!(got, want);
}

/// Asserts which hosts are loopback: `localhost` in any case, `127.0.0.1` and `::1`; and
/// not a name, another address, or the unspecified address.
#[test]
fn loopback_is_localhost_and_the_loopback_addresses() {
    for text in ["localhost:1", "LocalHost:1", "127.0.0.1:1", "[::1]:1"] {
        assert!(node(text).unwrap().is_loopback(), "{text}");
    }
    for text in [
        "node:1",
        "192.0.2.5:1",
        "0.0.0.0:1",
        "[::]:1",
        "localhost.farm:1",
    ] {
        assert!(!node(text).unwrap().is_loopback(), "{text}");
    }
}

/// Asserts a key's id and secret are printable ASCII and not empty, the id holds none of the
/// characters that would break the `Authorization` header, and the key prints its id and
/// never its secret.
#[test]
fn a_key_signs_and_never_prints_its_secret() {
    let key = key();
    assert_eq!(key.id(), "AKIDTEST");
    assert_eq!(key.secret(), "secret-0123456789");
    let printed = format!("{key:?}");
    assert_eq!(printed, "Key { id: \"AKIDTEST\", .. }");
    for id in ["", "AK ID", "AK,ID", "AK/ID", "AK=ID", "AKIDé"] {
        assert!(
            matches!(Key::new(id, "s"), Err(ConfigError::BadKey(why)) if why.starts_with("id")),
            "{id:?}"
        );
    }
    for secret in ["", "with space", "tab\t"] {
        assert!(
            matches!(Key::new("AKID", secret), Err(ConfigError::BadKey(why)) if why.starts_with("secret")),
            "{secret:?}"
        );
    }
    let error = Key::new("", "s").unwrap_err();
    assert_eq!(
        error.to_string(),
        "the key's id is empty or not a plain token"
    );
}

/// Asserts a region is printable ASCII without `/`, since it is a part of the signature's
/// scope; and the defaults of a configuration: no CA, plaintext not allowed off loopback,
/// the default timeouts.
#[test]
fn a_config_has_a_region_and_the_defaults() {
    let node = node("node:9600").unwrap();
    for region in ["", "us east", "a/b"] {
        let refused = Config::new(node.clone(), key(), region).unwrap_err();
        assert!(matches!(&refused, ConfigError::BadRegion(r) if r == region));
    }
    let config = Config::new(node.clone(), key(), "farm").unwrap();
    assert_eq!(config.node(), &node);
    assert_eq!(config.region, "farm");
    assert!(config.ca_pem.is_none());
    assert!(!config.allow_plaintext);
    assert_eq!(config.connect_timeout, Config::CONNECT_TIMEOUT);
    assert_eq!(config.call_timeout, Duration::from_secs(40));
    let changed = config
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(2))
        .with_clock(|| SystemTime::UNIX_EPOCH)
        .with_ca(b"pem".to_vec())
        .allow_plaintext();
    assert_eq!(changed.connect_timeout, Duration::from_secs(1));
    assert_eq!(changed.call_timeout, Duration::from_secs(2));
    assert_eq!((changed.clock)(), SystemTime::UNIX_EPOCH);
    assert_eq!(changed.ca_pem.as_deref(), Some(&b"pem"[..]));
    assert!(changed.allow_plaintext);
}
