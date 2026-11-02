//! Absolute paths and https URLs, the two small value types the settings use.

use mdm_core::https::{HttpsUrl, HttpsUrlError};
use mdm_core::path::{AbsPath, PathError};

/// Catches a path check that lets a unit name something outside its directory (`..`),
/// names `/` itself, takes a relative path or an empty component, or a NUL byte.
#[test]
fn an_absolute_path_has_clean_components() {
    let ok = AbsPath::new("/var/lib/js-mdm").expect("valid");
    assert_eq!(ok.as_str(), "/var/lib/js-mdm");
    assert_eq!(ok.to_string(), "/var/lib/js-mdm");
    let cases = [
        ("var/lib", PathError::NotAbsolute("var/lib".to_owned())),
        ("", PathError::NotAbsolute(String::new())),
        ("/", PathError::Empty("/".to_owned())),
        ("/a//b", PathError::Component("/a//b".to_owned())),
        ("/a/", PathError::Component("/a/".to_owned())),
        ("/a/./b", PathError::Component("/a/./b".to_owned())),
        ("/a/../b", PathError::Component("/a/../b".to_owned())),
        ("/a/b\0c", PathError::Component("/a/b\0c".to_owned())),
    ];
    for (text, want) in cases {
        assert_eq!(AbsPath::new(text), Err(want), "{text:?}");
    }
    let said = |text: &str| AbsPath::new(text).expect_err(text).to_string();
    assert_eq!(said("a"), "\"a\" is not absolute");
    assert!(said("/").contains("names no file"));
    assert!(said("/a/../b").contains("component"));
}

/// Catches a URL parser that takes another scheme, a user part (it would be printed
/// with the URL), an IPv6 literal, a port that is not 1 to 65535, or an empty host, and
/// one that loses the port, path or file name of a good URL.
#[test]
fn an_https_url_is_parsed_into_host_port_and_path() {
    let u = HttpsUrl::new("https://example.com/r/v1.0/a.zip?x=1").expect("valid");
    assert_eq!(
        (u.host(), u.port(), u.path(), u.file_name()),
        ("example.com", 443, "/r/v1.0/a.zip?x=1", "a.zip")
    );
    assert_eq!(u.to_string(), "https://example.com/r/v1.0/a.zip?x=1");
    let u = HttpsUrl::new("https://example.com:8443").expect("valid");
    assert_eq!(
        (u.host(), u.port(), u.path(), u.file_name()),
        ("example.com", 8443, "/", "")
    );
    let cases = [
        ("http://example.com/", "scheme"),
        ("example.com", "scheme"),
        ("https://user:key@example.com/", "host"),
        ("https://[::1]/", "host"),
        ("https://[::1/", "host"),
        ("https://::1]/", "host"),
        ("https://:443/", "host"),
        ("https:///x", "host"),
        ("https://example.com:0/", "port"),
        ("https://example.com:x/", "port"),
        ("https://example.com:65536/", "port"),
    ];
    for (text, kind) in cases {
        let got = HttpsUrl::new(text).expect_err(text);
        let want = match kind {
            "scheme" => HttpsUrlError::Scheme(text.to_owned()),
            "host" => HttpsUrlError::Host(text.to_owned()),
            _ => HttpsUrlError::Port(text.to_owned()),
        };
        assert_eq!(got, want, "{text}");
    }
    let said = |text: &str| HttpsUrl::new(text).expect_err(text).to_string();
    assert!(said("http://a/").contains("is not an https:// URL"));
    assert!(said("https://a@b/").contains("names no host"));
    assert!(said("https://a:0/").contains("port"));
}
