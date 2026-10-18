//! The certificates a mode presents: the role, extended key usage and names its row of
//! the farm's table gives, and the chain to the CA now. The farm's issuer makes the
//! certificates and `kbf-deploy linux verify-cert` checks one before a unit points at
//! it; this is the same check by the process that presents it, at every start.

use std::path::Path;
use std::sync::Arc;

use kbf_peer::{PeerRole, RoleError};
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier as _;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, Error, InconsistentKeys};

use super::{CertNeed, Class, Host, Needs, Report, Row, files};

const SERVER_AUTH: &str = "serverAuth";
const CLIENT_AUTH: &str = "clientAuth";

impl Row {
    fn role(self) -> PeerRole {
        match self {
            Row::Frontend => PeerRole::Frontend,
            Row::Storage => PeerRole::Storage,
            Row::Daemon => PeerRole::Daemon,
        }
    }

    fn ekus(self) -> &'static [&'static str] {
        match self {
            Row::Frontend | Row::Storage => &[SERVER_AUTH, CLIENT_AUTH],
            Row::Daemon => &[CLIENT_AUTH],
        }
    }
}

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    for c in &needs.certs {
        one(c, host, report);
    }
}

fn certificates(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .filter_map(Result::ok)
        .collect()
}

fn one(c: &CertNeed, host: &dyn Host, report: &mut Report) {
    let Some(pem) = files::read(host, c.flag, &c.path, "TLS cannot start without it", report)
    else {
        return;
    };
    let mut chain = certificates(&pem);
    let flag = c.flag;
    let at = c.path.display();
    if chain.is_empty() {
        report.refuse(
            format!("{flag} {at} holds no certificate"),
            "TLS cannot start without it",
            "write the PEM certificate the farm's issuer made for this node there".to_owned(),
        );
        return;
    }
    let leaf = chain.remove(0);
    let row_role = c.row.role();
    let issue = format!(
        "ask the farm's issuer for this node's {} certificate (a URI subjectAltName \
         urn:kbf:role:{row_role})",
        row_role.name()
    );

    match kbf_peer::role_of(&leaf) {
        Ok(role) if role == row_role => {}
        Ok(role) => report.refuse(
            format!("{flag} {at} names the role urn:kbf:role:{role}, not urn:kbf:role:{row_role}"),
            "the peers admit a node by the role its certificate names",
            issue.clone(),
        ),
        Err(RoleError::NotACertificate(e)) => {
            report.refuse(
                format!("{flag} {at} does not parse as a certificate: {e}"),
                "TLS cannot start without it",
                "write the PEM certificate the farm's issuer made for this node there".to_owned(),
            );
            return;
        }
        Err(RoleError::Count(0)) => report.refuse(
            format!("{flag} {at} names no role (urn:kbf:role:{row_role})"),
            "the peers admit a node by the role its certificate names",
            issue.clone(),
        ),
        Err(e) => report.refuse(
            format!("{flag} {at}: {e}"),
            "the peers admit a node by the role its certificate names",
            issue.clone(),
        ),
    }

    let got = ekus(&leaf);
    let want = c.row.ekus();
    if got != want {
        let shown = if got.is_empty() {
            "none".to_owned()
        } else {
            got.join(",")
        };
        report.refuse(
            format!(
                "{flag} {at}: its extended key usage is {shown}, not {}",
                want.join(",")
            ),
            "a certificate allowed more or less than its role's uses is refused by the farm",
            issue.clone(),
        );
    }

    let names = kbf_peer::names_of(&leaf)
        .map(|n| n.all())
        .unwrap_or_default();
    if let Some(id) = &c.node_id
        && names != [id.clone()]
    {
        let shown = if names.is_empty() {
            "no DNS name or address".to_owned()
        } else {
            names.join(" ")
        };
        report.refuse(
            format!("{flag} {at} names {shown}, not exactly the node id {id}"),
            "the farm binds a daemon's certificate to its --node-id, the one name it speaks for",
            format!("ask the farm's issuer for a certificate whose one DNS name is {id}"),
        );
    }

    if let Some((key_flag, key_path)) = &c.key {
        pair(c, (key_flag, key_path), &leaf, host, report);
    }

    if let Some((ca_flag, ca_path)) = &c.ca {
        chains(c, (ca_flag, ca_path), (&leaf, &chain, &names), host, report);
    }
}

/// Checks the private key is PEM, one TLS can sign with, and the key of the leaf. A
/// mismatch or an unparseable key would otherwise fail where the mode loads its TLS
/// configuration, which exits 1 and is restarted every `RestartSec` for good.
fn pair(
    c: &CertNeed,
    (key_flag, key_path): (&str, &Path),
    leaf: &CertificateDer<'static>,
    host: &dyn Host,
    report: &mut Report,
) {
    let (flag, at, key_at) = (c.flag, c.path.display(), key_path.display());
    let Some(pem) = files::read(
        host,
        key_flag,
        key_path,
        "TLS cannot start without it",
        report,
    ) else {
        return;
    };
    let write = format!("write the PEM private key the farm's issuer made for {flag} to {key_at}");
    let Ok(der) = PrivateKeyDer::from_pem_slice(&pem) else {
        report.refuse(
            format!("{key_flag} {key_at} holds no PEM private key"),
            "TLS cannot start without it",
            write,
        );
        return;
    };
    let Ok(signer) = rustls::crypto::ring::sign::any_supported_type(&der) else {
        report.refuse(
            format!("{key_flag} {key_at} holds a private key TLS cannot sign with"),
            "TLS cannot start without it",
            write,
        );
        return;
    };
    let certified = CertifiedKey::new(vec![leaf.clone()], signer);
    if let Err(Error::InconsistentKeys(InconsistentKeys::KeyMismatch)) = certified.keys_match() {
        report.refuse(
            format!("{key_flag} {key_at} is not the private key of {flag} {at}"),
            "a certificate presented with another key fails every handshake",
            format!(
                "point {key_flag} at the key made with {flag} {at}, or {flag} at its certificate"
            ),
        );
    }
}

/// The EKU names of `der`, serverAuth and clientAuth first, then any others by name or
/// OID; none for a certificate without the extension.
fn ekus(der: &[u8]) -> Vec<String> {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return Vec::new();
    };
    let Some(ext) = cert.extended_key_usage().ok().flatten() else {
        return Vec::new();
    };
    let e = ext.value;
    [
        (e.server_auth, SERVER_AUTH),
        (e.client_auth, CLIENT_AUTH),
        (e.any, "anyExtendedKeyUsage"),
        (e.code_signing, "codeSigning"),
        (e.email_protection, "emailProtection"),
        (e.time_stamping, "timeStamping"),
        (e.ocsp_signing, "OCSPSigning"),
    ]
    .into_iter()
    .filter(|(on, _)| *on)
    .map(|(_, name)| name.to_owned())
    .chain(e.other.iter().map(|oid| oid.to_id_string()))
    .collect()
}

/// Checks the leaf chains to the CA for each use in its row, as webpki does. Expired is a
/// warning (the first load of an expired certificate starts on purpose, and the reload
/// picks the renewed one up); not yet valid is not ready (a clock behind heals); any
/// other failure is a refusal. A purpose mismatch was reported with the key usage.
fn chains(
    c: &CertNeed,
    (ca_flag, ca_path): (&str, &Path),
    (leaf, intermediates, names): (&CertificateDer<'_>, &[CertificateDer<'_>], &[String]),
    host: &dyn Host,
    report: &mut Report,
) {
    let Some(ca_pem) = files::read(
        host,
        ca_flag,
        ca_path,
        "the node's certificate is checked against it",
        report,
    ) else {
        return;
    };
    let mut roots = RootCertStore::empty();
    for ca in certificates(&ca_pem) {
        let _ = roots.add(ca);
    }
    let (flag, at, ca_at) = (c.flag, c.path.display(), ca_path.display());
    if roots.is_empty() {
        report.refuse(
            format!("{ca_flag} {ca_at} holds no usable CA certificate"),
            "the node's certificate is checked against it",
            format!("write the farm CA's PEM certificate to {ca_at}"),
        );
        return;
    }
    let roots = Arc::new(roots);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (Ok(server), Ok(client)) = (
        WebPkiServerVerifier::builder_with_provider(roots.clone(), provider.clone()).build(),
        WebPkiClientVerifier::builder_with_provider(roots, provider).build(),
    ) else {
        return;
    };
    let now = UnixTime::since_unix_epoch(
        host.now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default(),
    );
    let (mut expired, mut early) = (false, false);
    for &eku in c.row.ekus() {
        let checked = if eku == SERVER_AUTH {
            let name = names
                .first()
                .and_then(|n| ServerName::try_from(n.clone()).ok());
            let Some(name) = name else {
                report.refuse(
                    format!("{flag} {at} names no DNS name or address"),
                    "no TLS client can check it as a server certificate",
                    "ask the farm's issuer for a certificate that names this host".to_owned(),
                );
                continue;
            };
            server
                .verify_server_cert(leaf, intermediates, &name, &[], now)
                .map(drop)
        } else {
            client
                .verify_client_cert(leaf, intermediates, now)
                .map(drop)
        };
        match checked {
            Ok(()) => {}
            Err(Error::InvalidCertificate(
                CertificateError::Expired | CertificateError::ExpiredContext { .. },
            )) => expired = true,
            Err(Error::InvalidCertificate(
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. },
            )) => early = true,
            Err(Error::InvalidCertificate(
                CertificateError::InvalidPurpose | CertificateError::InvalidPurposeContext { .. },
            )) => {}
            Err(e) => report.refuse(
                format!("{flag} {at} does not chain to {ca_flag} {ca_at} for {eku}: {e}"),
                "a peer refuses a certificate the farm CA did not issue",
                format!("use the certificate the farm's issuer made, or the CA it signs with, at {ca_flag}"),
            ),
        }
    }
    if expired {
        report.warnings.push(format!(
            "{flag} {at} has expired: kbf starts on it and takes the renewed file at the next \
             reload, but peers refuse it until then; renew it"
        ));
    }
    if early {
        report.push(
            Class::NotReady,
            format!("{flag} {at} is not valid yet"),
            "this host's clock may be behind the issuer's",
            "wait for the clock to sync (the unit restarts)".to_owned(),
        );
    }
}
