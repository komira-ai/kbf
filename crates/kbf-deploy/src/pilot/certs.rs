//! The server certificate check of `server-install`, in process (rustls with ring), in
//! place of `openssl verify` and a comparison of `openssl` public keys.
//!
//! [`check`] refuses a `server.pem` that does not chain to `ca.pem` for TLS server use
//! with the configured name at the current time (webpki's checks: the signatures, the
//! validity period, an extended key usage that allows serverAuth when one is given,
//! and the name among the subject alternative names), and a `server.key` that is not
//! the private key of `server.pem`'s first certificate.

use std::sync::Arc;

use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::sign::CertifiedKey;

/// Why the certificate or key was refused; the caller names the file.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CertError {
    #[error("ca.pem holds no usable certificate: {0}")]
    Ca(String),
    #[error("server.pem holds no certificate: {0}")]
    Server(String),
    #[error("server.pem does not chain to ca.pem for serverAuth with the name {name}: {why}")]
    Chain { name: String, why: String },
    #[error("server.key is not a private key rustls can read: {0}")]
    Key(String),
    #[error("server.key is not the key of server.pem: {0}")]
    Mismatch(String),
    #[error("{0:?} is not a DNS name or an IP address")]
    Name(String),
}

/// `server_pem` chains to a certificate of `ca_pem` for serverAuth with `name` now,
/// and `key_pem` is its key.
pub fn check(
    ca_pem: &[u8],
    server_pem: &[u8],
    key_pem: &[u8],
    name: &str,
) -> Result<(), CertError> {
    let server_name =
        ServerName::try_from(name.to_owned()).map_err(|_| CertError::Name(name.to_owned()))?;
    let mut roots = RootCertStore::empty();
    for cert in certificates(ca_pem).map_err(CertError::Ca)? {
        roots.add(cert).map_err(|e| CertError::Ca(e.to_string()))?;
    }
    let mut chain = certificates(server_pem).map_err(CertError::Server)?;
    if chain.is_empty() {
        return Err(CertError::Server("no CERTIFICATE block".to_owned()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|e| CertError::Ca(e.to_string()))?;
    let end_entity = chain.remove(0);
    verifier
        .verify_server_cert(&end_entity, &chain, &server_name, &[], UnixTime::now())
        .map_err(|e| CertError::Chain {
            name: name.to_owned(),
            why: e.to_string(),
        })?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| CertError::Key(e.to_string()))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| CertError::Key(e.to_string()))?;
    CertifiedKey::new(vec![end_entity], signing)
        .keys_match()
        .map_err(|e| CertError::Mismatch(e.to_string()))
}

/// Every certificate in `pem`; none when it holds no CERTIFICATE block.
fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair,
    };

    struct Pki {
        ca: String,
        server: String,
        key: String,
    }

    /// A CA and a server certificate it signs for `names`, with `eku`.
    fn pki(names: &[&str], eku: Vec<ExtendedKeyUsagePurpose>) -> Pki {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("ca params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer =
            CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("ca key")).expect("ca");
        let key = KeyPair::generate().expect("server key");
        let mut params =
            CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
                .expect("server params");
        params.extended_key_usages = eku;
        let server = params.signed_by(&key, &issuer).expect("server cert");
        Pki {
            ca: issuer.pem(),
            server: server.pem(),
            key: key.serialize_pem(),
        }
    }

    fn server_auth() -> Vec<ExtendedKeyUsagePurpose> {
        vec![ExtendedKeyUsagePurpose::ServerAuth]
    }

    fn checked(p: &Pki, name: &str) -> Result<(), CertError> {
        check(p.ca.as_bytes(), p.server.as_bytes(), p.key.as_bytes(), name)
    }

    /// Catches a check that refuses a good pair: a certificate for the name, signed by
    /// the CA, with its own key (and one with no extended key usage, which TLS servers
    /// accept as `openssl verify -purpose sslserver` does).
    #[test]
    fn a_certificate_for_the_name_with_its_key_passes() {
        assert_eq!(
            checked(
                &pki(&["kbf-pilot-server"], server_auth()),
                "kbf-pilot-server"
            ),
            Ok(())
        );
        assert_eq!(
            checked(&pki(&["kbf-pilot-server"], Vec::new()), "kbf-pilot-server"),
            Ok(())
        );
    }

    /// Catches a check that skips the name (a certificate for another server would
    /// pass), the purpose (a client certificate would pass), or the issuer (a
    /// certificate of another CA would pass).
    #[test]
    fn another_name_a_client_only_certificate_or_another_ca_is_refused() {
        let p = pki(&["kbf-pilot-server"], server_auth());
        let chain = |r: Result<(), CertError>| matches!(r, Err(CertError::Chain { .. }));
        assert!(chain(checked(&p, "other-server")));
        let client = pki(
            &["kbf-pilot-server"],
            vec![ExtendedKeyUsagePurpose::ClientAuth],
        );
        assert!(chain(checked(&client, "kbf-pilot-server")));
        let other = pki(&["kbf-pilot-server"], server_auth());
        let mixed = Pki { ca: other.ca, ..p };
        assert!(chain(checked(&mixed, "kbf-pilot-server")));
    }

    /// Catches a check that never compares the key with the certificate (the server
    /// would refuse to start, or serve with a key that is not its certificate's).
    #[test]
    fn a_key_of_another_certificate_is_refused() {
        let p = pki(&["kbf-pilot-server"], server_auth());
        let other = pki(&["kbf-pilot-server"], server_auth());
        let swapped = Pki {
            key: other.key,
            ..p
        };
        assert!(matches!(
            checked(&swapped, "kbf-pilot-server"),
            Err(CertError::Mismatch(_))
        ));
    }

    /// Catches empty or garbled files read as no constraint (an empty CA file would
    /// trust nothing yet report a chain error that hides the cause), and a name that is
    /// not a name.
    #[test]
    fn empty_or_garbled_files_and_a_bad_name_are_refused() {
        let p = pki(&["kbf-pilot-server"], server_auth());
        let garbled = "-----BEGIN CERTIFICATE-----\nMIIBAAAA\n-----END CERTIFICATE-----\n";
        let not_base64 = "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n";
        let bad_key = "-----BEGIN PRIVATE KEY-----\nMIIBAAAA\n-----END PRIVATE KEY-----\n";
        // The variant's name, as Debug writes it before its field.
        let kind = |e: &CertError| format!("{e:?}").split('(').next().map(str::to_owned);
        let (ca, server, key) = ("Ca", "Server", "Key");
        let n = "kbf-pilot-server";
        let cases: [(&str, &str, &str, &str, &str); 10] = [
            ("", &p.server, &p.key, n, ca),
            ("x", &p.server, &p.key, n, ca),
            (garbled, &p.server, &p.key, n, ca),
            (not_base64, &p.server, &p.key, n, ca),
            (&p.ca, "", &p.key, n, server),
            (&p.ca, not_base64, &p.key, n, server),
            (&p.ca, &p.server, "", n, key),
            (&p.ca, &p.server, &p.server, n, key),
            (&p.ca, &p.server, bad_key, n, key),
            (&p.ca, &p.server, &p.key, "not a name", "Name"),
        ];
        for (ca, server, key, name, want) in cases {
            let got = check(ca.as_bytes(), server.as_bytes(), key.as_bytes(), name);
            assert_eq!(
                got.as_ref().err().and_then(kind).as_deref(),
                Some(want),
                "{got:?}"
            );
        }
    }
}
