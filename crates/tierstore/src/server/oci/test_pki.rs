//! A test CA and leaves with chosen subjectAltNames, written as the files the front reads.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

/// A self-signed CA.
pub(crate) struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

pub(crate) fn ca() -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "test CA");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().expect("CA key");
    let issuer = CertifiedIssuer::self_signed(params, key).expect("CA certificate");
    Ca { issuer }
}

pub(crate) fn ip(text: &str) -> SanType {
    SanType::IpAddress(text.parse::<IpAddr>().expect("an address"))
}

pub(crate) fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("a DNS name"))
}

/// The files of a leaf.
pub(crate) struct Leaf {
    pub(crate) cert: PathBuf,
    pub(crate) key: PathBuf,
}

/// A leaf of `ca` with exactly the names `sans`, written to `<name>.pem` and `<name>.key` in
/// `dir`.
pub(crate) fn leaf(dir: &Path, name: &str, ca: &Ca, sans: Vec<SanType>) -> Leaf {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, &ca.issuer).expect("sign leaf");
    std::fs::create_dir_all(dir).expect("the directory");
    let files = Leaf {
        cert: dir.join(format!("{name}.pem")),
        key: dir.join(format!("{name}.key")),
    };
    std::fs::write(&files.cert, cert.pem()).expect("certificate");
    std::fs::write(&files.key, key.serialize_pem()).expect("key");
    files
}
