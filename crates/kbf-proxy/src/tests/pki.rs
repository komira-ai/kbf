//! A throwaway farm CA and the certificates the tests present. Nothing here is committed key
//! material: every key is made by the test that uses it.

use std::path::Path;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::tls::TlsPaths;

/// A certificate and its key, as PEM, and the certificate's DER.
pub(crate) struct Leaf {
    pub cert: String,
    pub key: String,
    pub der: Vec<u8>,
}

/// A CA that signs the server's and the clients' certificates.
pub(crate) struct Pki {
    issuer: CertifiedIssuer<'static, KeyPair>,
    pub ca_pem: String,
}

impl Pki {
    pub(crate) fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key"))
            .expect("CA certificate");
        let ca_pem = issuer.pem();
        Self { issuer, ca_pem }
    }

    fn leaf(&self, sans: Vec<SanType>, usage: ExtendedKeyUsagePurpose, expired: bool) -> Leaf {
        issue(&self.issuer, sans, usage, expired)
    }

    /// The server's certificate, for `localhost`.
    pub(crate) fn server(&self) -> Leaf {
        let name = SanType::DnsName("localhost".try_into().expect("ia5"));
        self.leaf(vec![name], ExtendedKeyUsagePurpose::ServerAuth, false)
    }

    /// A server certificate whose validity ended before now.
    pub(crate) fn expired_server(&self) -> Leaf {
        let name = SanType::DnsName("localhost".try_into().expect("ia5"));
        self.leaf(vec![name], ExtendedKeyUsagePurpose::ServerAuth, true)
    }

    /// A client certificate naming `dns` and, if given, the role `role` (`urn:kbf:role:...`).
    pub(crate) fn client(&self, dns: &[&str], role: Option<&str>) -> Leaf {
        self.leaf(
            client_sans(dns, role),
            ExtendedKeyUsagePurpose::ClientAuth,
            false,
        )
    }

    /// A client certificate whose validity ended before now.
    pub(crate) fn expired_client(&self, dns: &[&str], role: &str) -> Leaf {
        self.leaf(
            client_sans(dns, Some(role)),
            ExtendedKeyUsagePurpose::ClientAuth,
            true,
        )
    }

    /// A certificate with a client's names and role that is for servers only (its extended
    /// key usage is `serverAuth`).
    pub(crate) fn server_only_client(&self, dns: &[&str], role: &str) -> Leaf {
        self.leaf(
            client_sans(dns, Some(role)),
            ExtendedKeyUsagePurpose::ServerAuth,
            false,
        )
    }

    /// An intermediate CA signed by this one.
    pub(crate) fn intermediate(&self) -> Intermediate {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, "farm intermediate");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let issuer = CertifiedIssuer::signed_by(
            params,
            KeyPair::generate().expect("intermediate key"),
            &self.issuer,
        )
        .expect("intermediate certificate");
        Intermediate { issuer }
    }

    /// The files of a proxy in `dir`: this CA's server certificate and key, and this CA as
    /// the client CA.
    pub(crate) fn write(&self, dir: &Path, server: &Leaf) -> TlsPaths {
        let paths = TlsPaths {
            cert: dir.join("server.pem"),
            key: dir.join("server.key"),
            client_ca: dir.join("ca.pem"),
        };
        self.install(&paths, server, &self.ca_pem);
        paths
    }

    /// Replaces the three files whole (write aside, rename), as a renewal tool does.
    pub(crate) fn install(&self, paths: &TlsPaths, server: &Leaf, ca_pem: &str) {
        for (path, content) in [
            (&paths.cert, &server.cert),
            (&paths.key, &server.key),
            (&paths.client_ca, &ca_pem.to_owned()),
        ] {
            let aside = path.with_extension("new");
            std::fs::write(&aside, content).expect("write aside");
            std::fs::rename(&aside, path).expect("rename into place");
        }
    }
}

fn issue(
    issuer: &CertifiedIssuer<'static, KeyPair>,
    sans: Vec<SanType>,
    usage: ExtendedKeyUsagePurpose,
    expired: bool,
) -> Leaf {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![usage];
    if expired {
        params.not_before = rcgen::date_time_ymd(2026, 9, 2);
        params.not_after = rcgen::date_time_ymd(2026, 9, 3);
    }
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, issuer).expect("sign");
    Leaf {
        cert: cert.pem(),
        key: key.serialize_pem(),
        der: cert.der().to_vec(),
    }
}

fn client_sans(dns: &[&str], role: Option<&str>) -> Vec<SanType> {
    let mut sans: Vec<SanType> = dns
        .iter()
        .map(|d| SanType::DnsName((*d).try_into().expect("ia5")))
        .collect();
    if let Some(role) = role {
        let uri = format!("{}{role}", kbf_peer::ROLE_URI_PREFIX);
        sans.push(SanType::URI(uri.try_into().expect("ia5")));
    }
    sans
}

/// An intermediate CA under the farm CA, as an operator who keeps the root offline has.
pub(crate) struct Intermediate {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl Intermediate {
    /// A client certificate issued by this intermediate, sent as the chain
    /// `[leaf, intermediate]`.
    pub(crate) fn client_chain(&self, dns: &[&str], role: Option<&str>) -> Leaf {
        let mut leaf = issue(
            &self.issuer,
            client_sans(dns, role),
            ExtendedKeyUsagePurpose::ClientAuth,
            false,
        );
        leaf.cert.push_str(&self.issuer.pem());
        leaf
    }
}

/// A TLS client configuration that trusts `trust_pem`, presents `identity` if given, and
/// offers `alpn`.
pub(crate) fn client_config(
    trust_pem: &str,
    identity: Option<&Leaf>,
    alpn: &[&str],
) -> Arc<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(trust_pem.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("add root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots);
    let mut config = match identity {
        Some(identity) => builder
            .with_client_auth_cert(
                CertificateDer::pem_slice_iter(identity.cert.as_bytes())
                    .collect::<Result<_, _>>()
                    .expect("client chain"),
                PrivateKeyDer::from_pem_slice(identity.key.as_bytes()).expect("client key"),
            )
            .expect("client identity"),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    Arc::new(config)
}
