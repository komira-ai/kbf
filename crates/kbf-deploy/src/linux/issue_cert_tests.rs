use std::fs;
use std::os::unix::fs::PermissionsExt;

use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::*;

/// A CA: its certificate and its key, both PEM.
fn ca() -> (String, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "kbf test CA");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let key = KeyPair::generate().expect("key");
    let cert = params.self_signed(&key).expect("self-signed");
    (cert.pem(), key.serialize_pem())
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// The certificate's subjectAltName entries, each spelled `DNS:`, `IP:` or `URI:`.
fn sans(der: &[u8]) -> Vec<String> {
    let (_, cert) = X509Certificate::from_der(der).expect("der");
    let mut out = Vec::new();
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for name in &san.general_names {
                out.push(match name {
                    GeneralName::DNSName(d) => format!("DNS:{d}"),
                    GeneralName::URI(u) => format!("URI:{u}"),
                    GeneralName::IPAddress(b) => format!("IP:{b:?}"),
                    other => format!("{other:?}"),
                });
            }
        }
    }
    out
}

/// Catches a certificate that names its role otherwise than storage reads it (another
/// prefix, the role missing, two roles), a name or address left out of the
/// subjectAltName, a CA certificate issued in place of a leaf, an extended key usage
/// left out (a frontend could not call storage as a client), and a validity period not
/// starting before now or not lasting `--days`.
#[test]
fn a_certificate_names_the_node_and_its_role_and_chains_to_the_ca() {
    let (ca_cert, ca_key) = ca();
    let now = SystemTime::now();
    let issued = issue(
        &ca_cert,
        &ca_key,
        Role::Storage,
        &names(&["storage-1.farm.example", "storage-1"]),
        &["192.0.2.10".parse().expect("ip")],
        now,
        30,
    )
    .expect("issued");
    assert_eq!(
        sans(&issued.cert_der),
        [
            "DNS:storage-1.farm.example",
            "DNS:storage-1",
            "IP:[192, 0, 2, 10]",
            "URI:urn:kbf:role:storage"
        ]
    );
    let (_, cert) = X509Certificate::from_der(&issued.cert_der).expect("der");
    assert!(!cert.is_ca(), "a leaf, not a CA");
    let eku = cert
        .extended_key_usage()
        .expect("parses")
        .expect("an extended key usage")
        .value;
    assert!(eku.server_auth && eku.client_auth, "{eku:?}");
    let epoch = |t: SystemTime| {
        i64::try_from(t.duration_since(UNIX_EPOCH).expect("after 1970").as_secs()).expect("fits")
    };
    let validity = cert.validity();
    assert_eq!(validity.not_before.timestamp(), epoch(now) - 300);
    assert_eq!(
        validity.not_after.timestamp() - validity.not_before.timestamp(),
        30 * 86_400 + 300
    );
    // An address alone: no common name, the address the one name.
    let bare = issue(
        &ca_cert,
        &ca_key,
        Role::Storage,
        &[],
        &["192.0.2.11".parse().expect("ip")],
        now,
        1,
    )
    .expect("issued");
    assert_eq!(
        sans(&bare.cert_der),
        ["IP:[192, 0, 2, 11]", "URI:urn:kbf:role:storage"]
    );
    // The frontend's role, too.
    let frontend = issue(
        &ca_cert,
        &ca_key,
        Role::Frontend,
        &names(&["kbf.farm.example"]),
        &[],
        now,
        1,
    )
    .expect("issued");
    assert_eq!(
        sans(&frontend.cert_der),
        ["DNS:kbf.farm.example", "URI:urn:kbf:role:frontend"]
    );
}

/// Catches a certificate written without the in-process check: one signed by a key
/// that is not the CA certificate's does not chain, and is refused before anything is
/// printed; and a CA key in a form this kit cannot read is refused with the fix.
#[test]
fn a_ca_key_that_is_not_the_cas_is_refused() {
    let (ca_cert, _) = ca();
    let (_, other_key) = ca();
    let err = issue(
        &ca_cert,
        &other_key,
        Role::Storage,
        &names(&["storage-1"]),
        &[],
        SystemTime::now(),
        30,
    )
    .err()
    .expect("refused");
    assert!(
        matches!(err, IssueError::Check(CertError::Chain { .. })),
        "{err}"
    );
    let err = issue(
        &ca_cert,
        "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n",
        Role::Storage,
        &names(&["storage-1"]),
        &[],
        SystemTime::now(),
        30,
    )
    .err()
    .expect("refused");
    assert!(matches!(err, IssueError::CaKey(_)), "{err}");
    assert!(err.to_string().contains("openssl pkcs8 -topk8"), "{err}");
}

fn scratch(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("kbf-deploy-issue-cert-{name}"))
}

fn args(dir: &Path, apply: bool) -> IssueArgs {
    IssueArgs {
        role: Role::Storage,
        names: names(&["storage-1"]),
        ips: Vec::new(),
        ca_cert: dir.join("ca.pem"),
        ca_key: dir.join("ca.key"),
        out: dir.join("out"),
        days: 30,
        apply,
    }
}

/// Catches a dry run that writes, an apply that writes a key others can read (or the
/// certificate private), the two files under other names than the storage unit reads
/// (`storage.pem`, `storage.key`), and an existing certificate or key replaced.
#[test]
fn a_dry_run_writes_nothing_and_an_apply_never_replaces_a_key() {
    let dir = scratch("apply");
    let (ca_cert, ca_key) = ca();
    fs::write(dir.join("ca.pem"), ca_cert).expect("ca");
    fs::write(dir.join("ca.key"), ca_key).expect("ca key");
    fs::create_dir(dir.join("out")).expect("out");

    let lines = run(&args(&dir, false), SystemTime::now()).expect("dry run");
    assert_eq!(fs::read_dir(dir.join("out")).expect("out").count(), 0);
    let out = dir.join("out");
    assert_eq!(
        lines[1..],
        [
            format!("DRY-RUN: write {}/storage.pem (644)", out.display()),
            format!("DRY-RUN: write {}/storage.key (600)", out.display()),
            "DRY-RUN: nothing written; re-run with --apply".to_owned(),
        ]
    );

    run(&args(&dir, true), SystemTime::now()).expect("apply");
    let mode = |name: &str| {
        fs::metadata(out.join(name))
            .expect("written")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("storage.key"), 0o600);
    assert_eq!(mode("storage.pem") & 0o600, 0o600);
    assert_eq!(mode("storage.pem") & 0o022, 0, "only the owner writes it");
    let key = fs::read(out.join("storage.key")).expect("key");

    let err = run(&args(&dir, true), SystemTime::now()).expect_err("refused");
    assert!(
        matches!(err, IssueError::Exists(ref p) if p.ends_with("storage.pem")),
        "{err}"
    );
    fs::remove_file(out.join("storage.pem")).expect("rm");
    let err = run(&args(&dir, true), SystemTime::now()).expect_err("refused");
    assert!(
        matches!(err, IssueError::Exists(ref p) if p.ends_with("storage.key")),
        "{err}"
    );
    assert_eq!(fs::read(out.join("storage.key")).expect("key"), key);
    assert!(
        !out.join("storage.pem").exists(),
        "nothing written on a refusal"
    );
}

/// The verb's refusals, each exit 2 with a `REFUSED:` line and nothing written: a CA
/// file that cannot be read, a CA certificate that is not one, a name that is not ASCII
/// (no DNS name), and an `--out` that is not a directory to write into. Its success is
/// exit 0 with the lines `run` makes.
#[test]
fn the_verb_exits_0_issued_and_2_refused() {
    let dir = scratch("verb");
    let (ca_cert, ca_key) = ca();
    fs::write(dir.join("ca.pem"), &ca_cert).expect("ca");
    fs::write(dir.join("ca.key"), &ca_key).expect("ca key");
    let verb = |a: &IssueArgs| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli(a, &mut out, &mut err);
        let text = |b: Vec<u8>| String::from_utf8(b).expect("utf-8");
        (code, text(out), text(err))
    };
    let (code, out, err) = verb(&args(&dir, false));
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        out.starts_with(
            "storage certificate for storage-1 (role urn:kbf:role:storage), valid 30 days, chains to "
        ),
        "{out}"
    );

    let mut a = args(&dir, false);
    a.ca_key = dir.join("missing.key");
    let (code, _, err) = verb(&a);
    assert_eq!(code, ExitCode::from(2));
    assert!(
        err.starts_with("REFUSED: cannot read ") && err.contains("missing.key"),
        "{err}"
    );

    let mut a = args(&dir, false);
    a.ca_cert = dir.join("ca.key");
    let (code, _, err) = verb(&a);
    assert_eq!(code, ExitCode::from(2));
    assert!(
        err.starts_with("REFUSED: the CA certificate cannot be read"),
        "{err}"
    );

    let mut a = args(&dir, false);
    a.names = names(&["n\u{f6}de"]);
    let (code, _, err) = verb(&a);
    assert_eq!(code, ExitCode::from(2));
    assert!(
        err.starts_with("REFUSED: the certificate cannot be made"),
        "{err}"
    );

    // --out names no directory.
    let (code, _, err) = verb(&args(&dir, true));
    assert_eq!(code, ExitCode::from(2));
    assert!(
        err.starts_with("REFUSED: cannot write ") && err.contains("storage.pem"),
        "{err}"
    );
}
