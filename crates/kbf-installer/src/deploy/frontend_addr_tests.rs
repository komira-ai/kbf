//! Deriving the REAPI address from the frontend's own config.

use std::fs;
use std::path::{Path, PathBuf};

use super::frontend_addr::{Derived, derive};
use super::smoke_tests::pki;

fn dir(name: &str) -> PathBuf {
    crate::testkit::host_root(name).1
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    path
}

fn toml(dir: &Path, text: &str) -> PathBuf {
    write(dir, "kbf.toml", text)
}

fn unit(dir: &Path, flags: &str) -> PathBuf {
    unit_named(dir, "kbf-frontend@.service", flags)
}

fn unit_named(dir: &Path, name: &str, flags: &str) -> PathBuf {
    write(
        dir,
        name,
        &format!("[Service]\nExecStart=/usr/local/bin/kbf frontend \\\n{flags}\n"),
    )
}

fn url(d: Option<Derived>) -> Option<String> {
    d.map(|d| d.url)
}

/// Proves the listener's address comes from `[listener] listen` in the file, with an
/// unspecified address read as loopback of the same family and the port kept. Catches a
/// derivation that dials the wildcard, drops the port or reads another table's key.
#[test]
fn the_toml_listen_address_is_dialled_on_loopback() {
    let d = dir("fa-listen");
    for (listen, want) in [
        ("0.0.0.0:9000", "http://127.0.0.1:9000"),
        ("127.0.0.1:8980", "http://127.0.0.1:8980"),
        ("[::]:9001", "http://[::1]:9001"),
        // A listener bound to one address is not on loopback: dial that address.
        ("192.0.2.10:8980", "http://192.0.2.10:8980"),
    ] {
        let t = toml(&d, &format!("[listener]\nlisten = \"{listen}\"\n"));
        assert_eq!(
            url(derive(&t, &[]).unwrap()).as_deref(),
            Some(want),
            "{listen}"
        );
    }
    // `listen` in another table is not the REAPI listener's.
    let t = toml(
        &d,
        "[storage-node]\nlisten = \"0.0.0.0:1\"\n[listener]\nlisten = \"0.0.0.0:2\"\n",
    );
    assert_eq!(
        url(derive(&t, &[]).unwrap()).as_deref(),
        Some("http://127.0.0.1:2")
    );
}

/// Proves a file that does not set `listen` means the flag's default, 127.0.0.1:8980.
#[test]
fn a_file_without_listen_means_the_default_port() {
    let d = dir("fa-default");
    let t = toml(&d, "[log]\n");
    assert_eq!(
        url(derive(&t, &[]).unwrap()).as_deref(),
        Some("http://127.0.0.1:8980")
    );
}

/// Proves a configured certificate makes the address https, verified under the
/// certificate's own first DNS name. Catches http for a TLS listener and a name taken from
/// anywhere else.
#[test]
fn a_reapi_certificate_makes_it_https_under_its_name() {
    let d = dir("fa-tls");
    let (_, cert, _) = pki("farm.example");
    let cert = write(&d, "reapi.pem", &cert);
    let t = toml(
        &d,
        &format!(
            "[listener]\nlisten = \"0.0.0.0:9000\"\n[tls]\nreapi-tls-cert = {:?}\n",
            cert.to_str().unwrap()
        ),
    );
    assert_eq!(
        derive(&t, &[]).unwrap(),
        Some(Derived {
            url: "https://127.0.0.1:9000".into(),
            server_name: Some("farm.example".into())
        })
    );
    // A certificate that cannot be read or holds no name is an error, not a plain URL.
    let bad = toml(&d, "[tls]\nreapi-tls-cert = \"/nonexistent/reapi.pem\"\n");
    assert!(derive(&bad, &[]).unwrap_err().contains("reapi-tls-cert"));
    let junk = write(&d, "junk.pem", "not a certificate");
    let bad = toml(
        &d,
        &format!("[tls]\nreapi-tls-cert = {:?}\n", junk.to_str().unwrap()),
    );
    assert!(derive(&bad, &[]).is_err());
}

/// Proves the unit's flags are used when the file is absent, in both `--flag=value` and
/// `--flag value` forms, and override the file where both say something (a flag on the
/// command line overrides its key).
#[test]
fn the_units_flags_stand_in_for_an_absent_file_and_override_a_present_one() {
    let d = dir("fa-unit");
    let absent = d.join("absent.toml");
    let u = unit(
        &d,
        "    --listen=0.0.0.0:7000 \\\n    --api-listen=127.0.0.1:1",
    );
    assert_eq!(
        url(derive(&absent, std::slice::from_ref(&u)).unwrap()).as_deref(),
        Some("http://127.0.0.1:7000")
    );
    let spaced = unit_named(&d, "spaced.service", "    --listen 0.0.0.0:7001");
    assert_eq!(
        url(derive(&absent, &[spaced]).unwrap()).as_deref(),
        Some("http://127.0.0.1:7001")
    );
    let t = toml(&d, "[listener]\nlisten = \"0.0.0.0:9000\"\n");
    assert_eq!(
        url(derive(&t, std::slice::from_ref(&u)).unwrap()).as_deref(),
        Some("http://127.0.0.1:7000")
    );
    // A unit with a certificate flag: https under the certificate's name.
    let (_, cert, _) = pki("unit.example");
    let cert = write(&d, "reapi.pem", &cert);
    let u = unit(
        &d,
        &format!(
            "    --listen=0.0.0.0:7000 \\\n    --reapi-tls-cert={}",
            cert.display()
        ),
    );
    let got = derive(&absent, &[u]).unwrap().unwrap();
    assert_eq!(
        (got.url.as_str(), got.server_name.as_deref()),
        ("https://127.0.0.1:7000", Some("unit.example"))
    );
}

/// Proves the first existing unit that sets a listener is the one read, and a unit with
/// no `--listen` (a storage node's) is passed over.
#[test]
fn the_first_unit_with_a_listener_is_read() {
    let d = dir("fa-units");
    let absent = d.join("absent.toml");
    let storage = write(
        &d,
        "kbf-storage.service",
        "[Service]\nExecStart=/usr/local/bin/kbf storage \\\n    --worker-listen=0.0.0.0:1\n",
    );
    let missing = d.join("missing.service");
    let u = unit(&d, "    --listen=0.0.0.0:7002");
    assert_eq!(
        url(derive(&absent, &[missing, storage, u]).unwrap()).as_deref(),
        Some("http://127.0.0.1:7002")
    );
}

/// Proves a host with neither file nor unit derives nothing (the smoke is then skipped,
/// which a deploy reports as exit 5), and a broken file is an error naming it.
#[test]
fn no_config_derives_nothing_and_a_broken_file_is_an_error() {
    let d = dir("fa-none");
    assert_eq!(
        derive(&d.join("absent.toml"), &[d.join("absent.service")]).unwrap(),
        None
    );
    let t = toml(&d, "[listener\nlisten = ");
    assert!(derive(&t, &[]).unwrap_err().contains("kbf.toml"));
    let t = toml(&d, "[listener]\nlisten = \"not an address\"\n");
    assert!(derive(&t, &[]).unwrap_err().contains("listen"));
}

/// A certificate signed for these subject alternative names and this common name, as PEM.
fn cert_for(sans: &[&str], common_name: Option<&str>) -> String {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    let mut params =
        CertificateParams::new(sans.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .expect("params");
    params.distinguished_name = DistinguishedName::new();
    if let Some(cn) = common_name {
        params.distinguished_name.push(DnType::CommonName, cn);
    }
    params
        .self_signed(&KeyPair::generate().expect("key"))
        .expect("sign")
        .pem()
}

/// The name derived from a `[tls]` certificate with these names.
fn name_of(d: &Path, pem: &str) -> Result<Option<String>, String> {
    let cert = write(d, "reapi.pem", pem);
    let t = toml(
        d,
        &format!("[tls]\nreapi-tls-cert = {:?}\n", cert.to_str().unwrap()),
    );
    derive(&t, &[]).map(|d| d.and_then(|d| d.server_name))
}

/// Proves the name a certificate is dialled under is its first DNS name, else its common
/// name, and a certificate that names a host in neither way (an IP address alone) is an
/// error; a PEM block that is no certificate is refused as such, and a listener on one
/// loopback address of the other family is dialled as written. Catches a derivation that
/// takes an IP SAN for a name, ignores the common name, or reads anything with a PEM frame.
///
/// Mutants planted: the common-name fallback dropped; an IP SAN returned as the name; the
/// non-certificate refusal turned into "no name".
#[test]
fn a_certificates_name_is_its_dns_name_else_its_common_name() {
    let d = dir("fa-names");
    assert_eq!(
        name_of(&d, &cert_for(&["127.0.0.1", "farm.example"], None)).unwrap(),
        Some("farm.example".to_owned()),
        "the DNS name after an IP SAN"
    );
    assert_eq!(
        name_of(&d, &cert_for(&[], Some("cn.example"))).unwrap(),
        Some("cn.example".to_owned()),
        "no SAN: the common name"
    );
    let err = name_of(&d, &cert_for(&["127.0.0.1"], None)).unwrap_err();
    assert!(err.contains("names no host"), "{err}");
    let err = name_of(
        &d,
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .unwrap_err();
    assert!(err.contains("not an X.509 certificate"), "{err}");

    let t = toml(&d, "[listener]\nlisten = \"[::1]:9000\"\n");
    assert_eq!(
        url(derive(&t, &[]).unwrap()).as_deref(),
        Some("http://[::1]:9000")
    );
}

/// Proves a config that is there but cannot be read (a directory in its place) is an error
/// naming it, not an absent config. Catches an error other than not-found read as "no config".
///
/// Mutants planted: every read error treated as not-found.
#[test]
fn a_config_that_cannot_be_read_is_an_error_not_an_absent_one() {
    let d = dir("fa-unreadable");
    let err = derive(&d, &[]).unwrap_err();
    assert!(err.contains(d.to_str().unwrap()), "{err}");
}

/// Proves a unit without an `ExecStart` (a template that only has `[Install]`, say) says
/// nothing about the listener, so the next unit is read. Catches a derivation that stops at
/// a unit it cannot read an address from.
///
/// Mutants planted: a unit with no `ExecStart` ending the search with no answer.
#[test]
fn a_unit_without_execstart_is_passed_over() {
    let d = dir("fa-no-exec");
    let empty = write(&d, "a.service", "[Install]\nWantedBy=multi-user.target\n");
    let real = unit_named(&d, "b.service", "--listen 127.0.0.1:8123");
    assert_eq!(
        url(derive(&d.join("absent.toml"), &[empty, real]).unwrap()).as_deref(),
        Some("http://127.0.0.1:8123")
    );
}
