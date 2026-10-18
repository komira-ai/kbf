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
