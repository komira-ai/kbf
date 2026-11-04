use std::os::unix::fs::PermissionsExt as _;

use kbf_testkit::dir::TestDir;

use super::*;
use crate::Client;

fn origin(text: &str) -> Result<Origin, ConfigError> {
    text.parse()
}

fn scratch(name: &str) -> TestDir {
    let temp = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
    TestDir::under(temp, &format!("kbf-job-client-{name}"))
}

fn token_file(dir: &TestDir, name: &str, mode: u32, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn token() -> Token {
    Token::new("abcdefghijklmnopqrstuvwxyz012345").unwrap()
}

const SECRET: &[u8] = b"abcdefghijklmnopqrstuvwxyz012345";

/// Asserts what an origin is made of: scheme, host (without brackets) and port, with the
/// scheme's default port when none is written and a trailing slash allowed, and that the
/// address tonic is given is the same authority under `http`. Catches a default port that
/// is wrong for a scheme, an IPv6 host kept with its brackets (which TLS would take for a
/// name), and an `https` endpoint handed to tonic, which would add TLS of its own.
#[test]
fn an_origin_is_a_scheme_a_host_and_a_port() {
    let tls = origin("https://jobs.example").unwrap();
    assert!(tls.is_tls());
    assert_eq!((tls.host(), tls.port()), ("jobs.example", 443));
    assert_eq!(tls.authority(), "jobs.example");
    assert_eq!(tls.uri().to_string(), "https://jobs.example/");
    assert_eq!(tls.endpoint().to_string(), "http://jobs.example/");
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
/// scheme, a user name, a path, a query, a port past 65535, no host. Catches each check
/// being dropped.
#[test]
fn an_origin_with_anything_but_a_scheme_host_and_port_is_refused() {
    for text in [
        "ftp://jobs.example",
        "jobs.example",
        "https://u:p@jobs.example",
        "https://jobs.example/v1",
        "https://jobs.example/?x=1",
        "https://jobs.example:99999",
        "",
    ] {
        assert!(
            matches!(origin(text), Err(ConfigError::Origin { .. })),
            "{text:?}"
        );
    }
}

/// Asserts that plain `http` builds a client for a loopback name or address
/// (`127.0.0.1`, `[::1]`, `localhost`) and is refused for any other host, and that `https`
/// needs the pinned CA, refuses a CA for `http`, refuses a PEM with no certificate or a
/// broken one, and builds with one that holds a CA; each refusal is its own `ConfigError`,
/// and a client that builds dials nothing until it is called. Catches a loopback check that matches a prefix
/// (`127.0.0.1.example.com` is a name like any other), a client that falls back to no TLS or
/// to system roots without a CA, and a build that connects.
#[tokio::test]
async fn plain_http_is_refused_off_loopback_and_https_needs_the_pinned_ca() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for text in [
        format!("http://127.0.0.1:{port}"),
        format!("http://[::1]:{port}"),
        format!("http://localhost:{port}"),
    ] {
        let config = Config::new(origin(&text).unwrap(), token());
        assert!(Client::new(config).is_ok(), "{text}");
    }
    let accepted = listener.accept().map(|_| ());
    let nothing = accepted.unwrap_err();
    assert_eq!(
        nothing.kind(),
        std::io::ErrorKind::WouldBlock,
        "a build dials nothing"
    );

    for text in [
        "http://127.0.0.1.example.com:1",
        "http://0.0.0.0:1",
        "http://jobs.example",
        "http://[2001:db8::1]:1",
        "http://192.0.2.1:1",
    ] {
        assert!(
            matches!(origin(text), Err(ConfigError::PlainHttp(_))),
            "{text}"
        );
    }

    let https = || Config::new(origin("https://localhost:1").unwrap(), token());
    assert!(matches!(Client::new(https()), Err(ConfigError::MissingCa)));
    let http_with_ca = Config::new(origin("http://localhost:1").unwrap(), token())
        .with_ca_pem(b"-----BEGIN CERTIFICATE-----".to_vec());
    assert!(matches!(
        Client::new(http_with_ca),
        Err(ConfigError::UnusedCa)
    ));
    for pem in [
        &b"no certificate here"[..],
        b"-----BEGIN CERTIFICATE-----\nnot base64 !!\n-----END CERTIFICATE-----\n",
        b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    ] {
        let config = https().with_ca_pem(pem.to_vec());
        assert!(
            matches!(Client::new(config), Err(ConfigError::BadCa(_))),
            "{}",
            String::from_utf8_lossy(pem)
        );
    }
    // The same check with a PEM that holds a CA: the client builds, trusting that root.
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = rcgen::CertifiedIssuer::self_signed(params, key).unwrap();
    assert!(Client::new(https().with_ca_pem(ca.pem().into_bytes())).is_ok());
    let unnamed =
        Config::new(origin("https://a..b:1").unwrap(), token()).with_ca_pem(b"unused".to_vec());
    assert!(matches!(
        Client::new(unnamed),
        Err(ConfigError::Origin { reason, .. }) if reason == "not a name a certificate can carry"
    ));
}

/// Asserts that a token must be printable ASCII without spaces, goes into the metadata as
/// `Bearer <token>` marked sensitive, and never shows in a `Debug`. Catches a token that
/// would split the metadata or be logged by the HTTP/2 stack.
#[test]
fn a_token_is_checked_and_never_printed() {
    let token = Token::new("s3cret-Token_value~!").unwrap();
    assert_eq!(format!("{token:?}"), "Token(..)");
    assert!(token.value().is_sensitive());
    assert_eq!(token.value().as_bytes(), b"Bearer s3cret-Token_value~!");
    assert!(!format!("{:?}", token.value()).contains("s3cret"));
    for bad in ["", "has space", "tab\t", "new\nline", "caf\u{e9}"] {
        assert!(
            matches!(Token::new(bad), Err(ConfigError::BadToken(_))),
            "{bad:?}"
        );
    }
}

/// Asserts how the token file is read: modes 0600 and 0400 are taken; 0640, 0644 and the
/// owner-execute modes are refused as a mode; a directory is not a regular file; a file over
/// 4 KiB is refused, and one of exactly 4 KiB is read; one trailing newline is dropped and a
/// second makes the token bad; an empty file and a token with a space are bad tokens; a
/// missing file and one that is not UTF-8 cannot be read. Catches a file readable by others
/// being accepted (how a token leaks to another user of the box), a size check off by one,
/// and a newline stripped more than once, which would hide a malformed file.
#[test]
fn the_token_file_must_be_owner_only() {
    let dir = scratch("token-file");
    let ok = token_file(&dir, "ok", 0o600, b"abcdefghijklmnopqrstuvwxyz012345\n");
    assert_eq!(
        Token::from_file(&ok).unwrap().value().as_bytes(),
        b"Bearer abcdefghijklmnopqrstuvwxyz012345"
    );
    let read_only = token_file(&dir, "read-only", 0o400, SECRET);
    assert!(Token::from_file(&read_only).is_ok());
    for mode in [0o640, 0o644, 0o604, 0o660, 0o700, 0o500] {
        let shared = token_file(&dir, &format!("mode-{mode:o}"), mode, SECRET);
        assert!(
            matches!(
                Token::from_file(&shared),
                Err(ConfigError::TokenFileMode {
                    problem: "mode is not 0600 or 0400",
                    ..
                })
            ),
            "{mode:o}"
        );
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        Token::from_file(dir.path()),
        Err(ConfigError::TokenFileMode {
            problem: "not a regular file",
            ..
        })
    ));
    let big = token_file(&dir, "big", 0o600, &vec![b'a'; 4097]);
    assert!(matches!(
        Token::from_file(&big),
        Err(ConfigError::TokenFileMode {
            problem: "over 4 KiB",
            ..
        })
    ));
    let at_cap = token_file(&dir, "at-cap", 0o600, &vec![b'a'; 4096]);
    assert!(Token::from_file(&at_cap).is_ok());
    for (name, bytes) in [
        ("two-newlines", &b"abcdefghijklmnopqrstuvwxyz012345\n\n"[..]),
        ("empty", b""),
        ("only-newline", b"\n"),
        ("space", b"abcdefgh ijklmnop"),
    ] {
        let path = token_file(&dir, name, 0o600, bytes);
        assert!(
            matches!(Token::from_file(&path), Err(ConfigError::BadToken(_))),
            "{name}"
        );
    }
    let missing = dir.path().join("nothing");
    assert!(matches!(
        Token::from_file(&missing),
        Err(ConfigError::TokenFile { .. })
    ));
    let binary = token_file(&dir, "binary", 0o600, &[0xff, 0xfe]);
    assert!(matches!(
        Token::from_file(&binary),
        Err(ConfigError::TokenFile { .. })
    ));
}

/// Asserts the builder keeps what it is given, and the defaults it starts from.
#[test]
fn a_config_keeps_its_parts() {
    let config = Config::new(origin("http://localhost:1").unwrap(), token());
    assert_eq!(config.connect_timeout, Config::CONNECT_TIMEOUT);
    assert_eq!(config.request_timeout, Config::REQUEST_TIMEOUT);
    assert!(config.ca_pem.is_none());
    let config = config
        .with_ca_pem(b"pem".to_vec())
        .with_timeouts(Duration::from_secs(1), Duration::from_secs(2));
    assert_eq!(config.ca_pem.as_deref(), Some(&b"pem"[..]));
    assert_eq!(config.connect_timeout, Duration::from_secs(1));
    assert_eq!(config.request_timeout, Duration::from_secs(2));
    assert!(!format!("{config:?}").contains("abcdefgh"));
}
