//! The verb's output and exit codes over certificates made in the test; the checks
//! themselves are tested in kbf-ca (`verify_cert_tests.rs`).

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose as Eku, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

use super::*;

type Ca = CertifiedIssuer<'static, KeyPair>;

fn ca() -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA")
}

/// A storage node's certificate as the issuer makes it.
fn storage(ca: &Ca) -> String {
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::DnsName("frontend.farm.example".try_into().expect("IA5")),
        SanType::URI("urn:kbf:role:storage".try_into().expect("IA5")),
    ];
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![Eku::ServerAuth, Eku::ClientAuth];
    params.use_authority_key_identifier_extension = true;
    params
        .signed_by(&KeyPair::generate().expect("key"), ca)
        .expect("signed")
        .pem()
}

fn node_id(row: RowArg) -> Option<&'static str> {
    (row == RowArg::Daemon).then_some("node-1")
}

/// Catches the verb printing OK for a broken certificate or exiting 0 for one, an
/// unreadable file or one with no certificate not refused (exit 2), and the OK line not
/// naming the row's role and uses.
#[test]
fn the_verb_prints_its_verdict_and_exits_by_it() {
    let dir = kbf_host::testkit::scratch("linux_verify_cert");
    let ca = ca();
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("storage.pem"), storage(&ca)).expect("write");
    let args = |row: RowArg, cert: &str| VerifyArgs {
        row,
        cert: dir.join(cert),
        ca_cert: dir.join("ca.pem"),
        node_id: node_id(row).map(str::to_owned),
    };
    let verb = |a: &VerifyArgs| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli(a, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    };
    let (code, out, err) = verb(&args(RowArg::Storage, "storage.pem"));
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert_eq!(
        out,
        format!(
            "CERT OK: {} is a storage certificate: role urn:kbf:role:storage, extended key usage serverAuth,clientAuth, chains to {}\n",
            dir.join("storage.pem").display(),
            dir.join("ca.pem").display()
        )
    );
    let (code, out, _) = verb(&args(RowArg::FrontendWorker, "storage.pem"));
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        out,
        "FAIL role: it names the role urn:kbf:role:storage, not urn:kbf:role:frontend\nCERT BROKEN: 1 failed check(s)\n"
    );
    std::fs::write(dir.join("junk.pem"), "no certificate here\n").expect("write");
    let (code, out, err) = verb(&args(RowArg::Storage, "junk.pem"));
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            ExitCode::from(2),
            "",
            "REFUSED: the certificate cannot be read: no CERTIFICATE block\n"
        )
    );
    let (code, out, err) = verb(&args(RowArg::Daemon, "absent.pem"));
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(
        err.starts_with(&format!(
            "REFUSED: cannot read {}: ",
            dir.join("absent.pem").display()
        )),
        "{err}"
    );
}
