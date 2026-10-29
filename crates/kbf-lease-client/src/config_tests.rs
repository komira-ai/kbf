use std::os::unix::fs::PermissionsExt as _;

use kbf_testkit::dir::TestDir;

use super::*;

fn origin(text: &str) -> Result<Origin, ConfigError> {
    text.parse()
}

fn scratch(name: &str) -> TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
    TestDir::under(temp, &format!("kbf-lease-client-{name}"))
}

fn token_file(dir: &TestDir, name: &str, mode: u32, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// Asserts what an origin is made of: scheme, host (without brackets) and port, with the
/// scheme's default port when none is written and a trailing slash allowed. It catches a
/// default port that is wrong for a scheme and an IPv6 host kept with its brackets (which
/// TLS would take for a name).
#[test]
fn an_origin_is_a_scheme_a_host_and_a_port() {
    let tls = origin("https://leases.farm.example").unwrap();
    assert!(tls.is_tls());
    assert_eq!((tls.host(), tls.port()), ("leases.farm.example", 443));
    assert_eq!(tls.authority(), "leases.farm.example");
    let local = origin("http://127.0.0.1:8443/").unwrap();
    assert!(!local.is_tls());
    assert_eq!((local.host(), local.port()), ("127.0.0.1", 8443));
    assert_eq!(local.authority(), "127.0.0.1:8443");
    let v6 = origin("http://[::1]:9").unwrap();
    assert_eq!((v6.host(), v6.port()), ("::1", 9));
    assert_eq!(v6.authority(), "[::1]:9");
    let plain = origin("http://localhost").unwrap();
    assert_eq!((plain.host(), plain.port()), ("localhost", 80));
}

/// Asserts that an origin that could send the token somewhere else is refused: another
/// scheme, a user name, a path, a query, no host. It catches each check being dropped.
#[test]
fn an_origin_with_anything_but_a_scheme_host_and_port_is_refused() {
    for text in [
        "ftp://leases.farm.example",
        "leases.farm.example",
        "https://u:p@leases.farm.example",
        "https://leases.farm.example/v1",
        "https://leases.farm.example/?x=1",
        "https://leases.farm.example:99999",
        "",
    ] {
        assert!(
            matches!(origin(text), Err(ConfigError::Origin { .. })),
            "{text:?}"
        );
    }
}

/// Asserts that plain `http` is allowed to loopback names and addresses only. It catches a
/// check that lets a farm address through, which would put the token on the wire in clear.
#[test]
fn plain_http_is_for_loopback_only() {
    for text in ["http://localhost:1", "http://127.0.0.1:1", "http://[::1]:1"] {
        assert!(origin(text).is_ok(), "{text}");
    }
    for text in [
        "http://0.0.0.0:1",
        "http://leases.farm.example",
        "http://[fd00::1]:1",
    ] {
        assert!(
            matches!(origin(text), Err(ConfigError::PlainHttp(_))),
            "{text}"
        );
    }
}

/// Asserts that a token must be printable ASCII without spaces and that it never shows in
/// Debug output or in the header's own Debug (the header is marked sensitive).
#[test]
fn a_token_is_checked_and_never_printed() {
    let token = Token::new("s3cret-Token_value~!").unwrap();
    assert_eq!(format!("{token:?}"), "Token(..)");
    assert!(token.header().is_sensitive());
    assert_eq!(token.header().as_bytes(), b"Bearer s3cret-Token_value~!");
    assert!(!format!("{:?}", token.header()).contains("s3cret"));
    for bad in ["", "has space", "tab\t", "new\nline", "caf\u{e9}"] {
        assert!(
            matches!(Token::new(bad), Err(ConfigError::BadToken(_))),
            "{bad:?}"
        );
    }
}

/// Asserts that the token file is read the way the server reads its own: mode 0600 or
/// 0400, regular file, one trailing newline dropped. It catches a file readable by others
/// being accepted, which is how a token leaks to another user of the box, and an owner-execute
/// mode (0700, 0500), which the server's own check refuses.
#[test]
fn a_token_file_must_be_private() {
    let dir = scratch("token-file");
    let ok = token_file(&dir, "ok", 0o600, b"abcdefghijklmnopqrstuvwxyz012345\n");
    let read = Token::from_file(&ok).unwrap();
    assert_eq!(
        read.header().as_bytes(),
        b"Bearer abcdefghijklmnopqrstuvwxyz012345"
    );
    let read_only = token_file(
        &dir,
        "read-only",
        0o400,
        b"abcdefghijklmnopqrstuvwxyz012345",
    );
    assert!(Token::from_file(&read_only).is_ok());
    for mode in [0o640, 0o604, 0o660, 0o666, 0o701, 0o700, 0o500] {
        let shared = token_file(
            &dir,
            &format!("shared-{mode:o}"),
            mode,
            b"abcdefghijklmnopqrstuvwxyz012345",
        );
        assert!(
            matches!(
                Token::from_file(&shared),
                Err(ConfigError::TokenFileMode { .. })
            ),
            "{mode:o}"
        );
    }
}

/// Asserts the other ways a token file is wrong: missing, a directory, too big, and holding
/// no usable token.
#[test]
fn a_token_file_that_is_missing_big_or_empty_is_refused() {
    let dir = scratch("token-file-bad");
    let missing = dir.path().join("nothing");
    assert!(matches!(
        Token::from_file(&missing),
        Err(ConfigError::TokenFile { .. })
    ));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        Token::from_file(dir.path()),
        Err(ConfigError::TokenFileMode { .. })
    ));
    let big = token_file(&dir, "big", 0o600, &vec![b'a'; 5000]);
    assert!(matches!(
        Token::from_file(&big),
        Err(ConfigError::TokenFileMode { .. })
    ));
    let empty = token_file(&dir, "empty", 0o600, b"\n");
    assert!(matches!(
        Token::from_file(&empty),
        Err(ConfigError::BadToken(_))
    ));
    let binary = token_file(&dir, "binary", 0o600, &[0xff, 0xfe]);
    assert!(matches!(
        Token::from_file(&binary),
        Err(ConfigError::TokenFile { .. })
    ));
}

/// Asserts the builder keeps what it is given.
#[test]
fn a_config_keeps_its_parts() {
    let token = Token::new("x").unwrap();
    let config = Config::new(origin("http://localhost:1").unwrap(), token);
    assert_eq!(config.connect_timeout, Config::CONNECT_TIMEOUT);
    assert_eq!(config.request_timeout, Config::REQUEST_TIMEOUT);
    assert!(config.ca_pem.is_none());
    let config = config
        .with_ca_pem(b"pem".to_vec())
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(2));
    assert_eq!(config.ca_pem.as_deref(), Some(&b"pem"[..]));
    assert_eq!(config.connect_timeout, Duration::from_secs(1));
    assert_eq!(config.request_timeout, Duration::from_secs(2));
}
