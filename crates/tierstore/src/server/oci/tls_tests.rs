//! The start-up checks of the front's certificate and key.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use super::OciError;
use super::test_pki::{ca, dns, ip, leaf};
use super::tls::{acceptor, ip_of, unnamed};

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

/// Why `acceptor` refuses these files for `listen`.
fn refusal(cert: &Path, key: &Path, listen: &str) -> String {
    let refused = acceptor(cert, key, addr(listen)).err();
    refused.expect("refused").to_string()
}

/// Asserts a certificate that names the address is accepted: the address itself, or any IP when
/// the listen address is the unspecified one, and a certificate with several names.
#[test]
fn a_certificate_that_names_the_address_is_accepted() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-named");
    let ca = ca();
    let one = leaf(dir.path(), "one", &ca, vec![ip("127.0.0.1")]);
    acceptor(&one.cert, &one.key, addr("127.0.0.1:443")).expect("named");
    acceptor(&one.cert, &one.key, addr("0.0.0.0:443")).expect("any IP will do for 0.0.0.0");
    let many = vec![dns("registry.test"), ip("192.0.2.7"), ip("::1")];
    let many = leaf(dir.path(), "many", &ca, many);
    for listen in ["192.0.2.7:443", "[::1]:443", "[::]:443"] {
        acceptor(&many.cert, &many.key, addr(listen)).expect(listen);
    }
}

/// Asserts a certificate without an IP subjectAltName for the address is refused with the
/// names it does have: none at all, a DNS name only, or another address.
#[test]
fn a_certificate_that_does_not_name_the_address_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-unnamed");
    let ca = ca();
    let bare = leaf(dir.path(), "bare", &ca, vec![]);
    let named = leaf(dir.path(), "dns", &ca, vec![dns("registry.test")]);
    let other = leaf(dir.path(), "other", &ca, vec![ip("192.0.2.8"), ip("::2")]);
    for (files, listen) in [
        (&bare, "127.0.0.1:443"),
        (&bare, "0.0.0.0:443"),
        (&named, "127.0.0.1:443"),
        (&named, "[::]:443"),
        (&other, "192.0.2.7:443"),
    ] {
        let why = refusal(&files.cert, &files.key, listen);
        assert!(why.starts_with("--oci-tls-cert "), "{why}");
        assert!(why.contains("no IP subjectAltName"), "{why}");
        assert!(
            why.contains(&listen.replace(":443", "").replace(['[', ']'], "")),
            "{why}"
        );
    }
    let why = refusal(&other.cert, &other.key, "192.0.2.7:443");
    assert!(why.contains("[192.0.2.8, ::2]"), "{why}");
}

/// Asserts what is wrong with the files is said with the flag that names the file: a file that
/// is not there, text with no certificate, a certificate that is not X.509, a key that is not a
/// key, and a key that is another certificate's.
#[test]
fn files_that_are_not_a_certificate_and_a_key_are_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-tls-files");
    let ca = ca();
    let good = leaf(dir.path(), "good", &ca, vec![ip("127.0.0.1")]);
    let stranger = leaf(dir.path(), "stranger", &ca, vec![ip("127.0.0.1")]);
    let missing: PathBuf = dir.path().join("missing");
    let text = dir.path().join("text");
    std::fs::write(&text, "not a PEM file\n").expect("write");
    let junk = dir.path().join("junk.pem");
    let block = "-----BEGIN CERTIFICATE-----\nanVuaw==\n-----END CERTIFICATE-----\n";
    std::fs::write(&junk, block).expect("write");
    let broken = dir.path().join("broken.pem");
    let broken_block = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
    std::fs::write(&broken, broken_block).expect("write");

    let listen = "127.0.0.1:443";
    let why = refusal(&missing, &good.key, listen);
    assert!(why.starts_with("--oci-tls-cert "), "{why}");
    assert!(why.contains("missing"), "{why}");
    let why = refusal(&good.cert, &missing, listen);
    assert!(why.starts_with("--oci-tls-key "), "{why}");
    assert!(why.contains("missing"), "{why}");
    assert!(refusal(&text, &good.key, listen).contains("no certificate"));
    assert!(refusal(&junk, &good.key, listen).contains("not X.509"));
    assert!(refusal(&broken, &good.key, listen).contains("not PEM certificates"));
    let why = refusal(&good.cert, &text, listen);
    assert!(why.starts_with("--oci-tls-key "), "{why}");
    assert!(why.contains("not a PEM private key"), "{why}");
    let why = refusal(&good.cert, &stranger.key, listen);
    assert!(
        why.starts_with("--oci-tls-cert and --oci-tls-key: "),
        "{why}"
    );
}

/// Asserts which errors a restart can mend: an unreadable file and an address that cannot be
/// bound; the rest are flags and files that are wrong as they are.
#[test]
fn only_unreadable_files_and_bind_errors_are_not_invalid_flags() {
    let io = || std::io::Error::from(std::io::ErrorKind::NotFound);
    let read = OciError::Read {
        flag: "--oci-tls-cert",
        path: PathBuf::from("c"),
        source: io(),
    };
    let bind = OciError::Bind {
        addr: addr("127.0.0.1:1"),
        source: io(),
    };
    assert!(!read.is_invalid_flags());
    assert!(!bind.is_invalid_flags());
    assert!(bind.to_string().starts_with("binding 127.0.0.1:1: "));
    let pem = OciError::Pem {
        flag: "--oci-tls-key",
        path: PathBuf::from("k"),
        why: "bad".to_owned(),
    };
    for invalid in [
        pem,
        OciError::Config("x".to_owned()),
        OciError::Missing("y".to_owned()),
    ] {
        assert!(invalid.is_invalid_flags(), "{invalid}");
    }
}

/// Asserts a subjectAltName address is read from 4 bytes or 16 and from no other length, and
/// that an empty list of names names no address.
#[test]
fn an_address_is_four_bytes_or_sixteen() {
    assert_eq!(ip_of(&[192, 0, 2, 7]), Some("192.0.2.7".parse().unwrap()));
    let v6 = "2001:db8::1".parse::<std::net::Ipv6Addr>().unwrap();
    assert_eq!(ip_of(&v6.octets()), Some(v6.into()));
    for length in [0, 3, 5, 15, 17] {
        assert_eq!(ip_of(&vec![1; length]), None, "{length}");
    }
    assert!(unnamed(&[], addr("0.0.0.0:1")).is_some());
    assert!(unnamed(&["192.0.2.7".parse().unwrap()], addr("0.0.0.0:1")).is_none());
}
