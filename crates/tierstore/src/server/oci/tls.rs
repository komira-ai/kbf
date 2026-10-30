//! The TLS of the OCI front: the certificate and key, read once at start, and the check that the
//! certificate names the address clients dial.

use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use x509_parser::extensions::GeneralName;

/// Why the OCI front cannot start.
#[derive(Debug, thiserror::Error)]
pub enum OciError {
    /// `--oci-prefix` is not usable.
    #[error("--oci-prefix {prefix:?}: {why}")]
    Prefix {
        /// The prefix given.
        prefix: String,
        /// What is wrong with it.
        why: String,
    },
    /// `--oci-tls-cert` or `--oci-tls-key` is missing.
    #[error("{0}")]
    Missing(String),
    /// A certificate or key file cannot be read.
    #[error("{flag} {path}: {source}")]
    Read {
        /// The flag that names the file.
        flag: &'static str,
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        #[source]
        source: std::io::Error,
    },
    /// A file is not what it must be, or the certificate does not name the address.
    #[error("{flag} {path}: {why}")]
    Pem {
        /// The flag that names the file.
        flag: &'static str,
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The certificate and key do not make a TLS configuration.
    #[error("--oci-tls-cert and --oci-tls-key: {0}")]
    Config(String),
    /// The address cannot be bound.
    #[error("binding {addr}: {source}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// Why it cannot be bound.
        #[source]
        source: std::io::Error,
    },
}

impl OciError {
    /// Whether flags and files that a restart cannot mend are at fault, rather than a file that
    /// could not be read or an address that could not be bound.
    #[must_use]
    pub fn is_invalid_flags(&self) -> bool {
        !matches!(self, Self::Read { .. } | Self::Bind { .. })
    }
}

const CERT_FLAG: &str = "--oci-tls-cert";
const KEY_FLAG: &str = "--oci-tls-key";

/// The acceptor for the certificate chain in `cert` and the key in `key` (both PEM), which serves
/// HTTP/1.1 only and asks no client for a certificate. The leaf must carry an IP subjectAltName
/// for `listen` (any IP subjectAltName, when `listen` is the unspecified address), since the
/// clients of the front dial an IP address and accept only a certificate that names it. A leaf
/// that is expired or not yet valid is refused, and so is a key file that others can read (a
/// group may, as the key of a service often is).
///
/// The files are read once: a new certificate takes a restart, so renew it before it expires.
///
/// # Errors
/// A file that cannot be read, PEM that is not a certificate chain and a key, a certificate that
/// does not name the address or is not valid now, a key file any user can read, or a key that is
/// not the leaf's.
pub fn acceptor(cert: &Path, key: &Path, listen: SocketAddr) -> Result<TlsAcceptor, OciError> {
    let read = |flag, path: &Path| {
        std::fs::read(path).map_err(|source| OciError::Read {
            flag,
            path: path.to_owned(),
            source,
        })
    };
    let pem_error = |flag, path: &Path, why: String| OciError::Pem {
        flag,
        path: path.to_owned(),
        why,
    };
    let cert_pem = read(CERT_FLAG, cert)?;
    let key_pem = read(KEY_FLAG, key)?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|why| pem_error(CERT_FLAG, cert, format!("not PEM certificates: {why}")))?;
    let Some(leaf) = chain.first() else {
        return Err(pem_error(CERT_FLAG, cert, "no certificate".to_owned()));
    };
    let facts = Facts::of(leaf).map_err(|why| pem_error(CERT_FLAG, cert, why))?;
    if let Some(why) = unnamed(&facts.ips, listen) {
        return Err(pem_error(CERT_FLAG, cert, why));
    }
    if let Some(why) = facts.not_valid_now() {
        return Err(pem_error(CERT_FLAG, cert, why));
    }
    // A mode that cannot be read counts as open to all.
    let mode = std::fs::metadata(key).map_or(0o777, |meta| meta.permissions().mode());
    if mode & 0o007 != 0 {
        let why = "any user can read the key; make it mode 0640 or 0600".to_owned();
        return Err(pem_error(KEY_FLAG, key, why));
    }
    let private = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|why| pem_error(KEY_FLAG, key, format!("not a PEM private key: {why}")))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let built = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| {
            builder
                .with_no_client_auth()
                .with_single_cert(chain, private)
        });
    let mut config = built.map_err(|why| OciError::Config(why.to_string()))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// What the front needs to know of a leaf certificate.
struct Facts {
    /// The IP addresses it names in its subjectAltName.
    ips: Vec<IpAddr>,
    /// Whether the time now is within its validity.
    valid: bool,
    /// Its validity, as text, for the message of a refusal.
    period: String,
}

impl Facts {
    fn of(leaf: &CertificateDer<'_>) -> Result<Self, String> {
        let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
            .map_err(|why| format!("the first certificate is not X.509: {why}"))?;
        let validity = parsed.validity();
        Ok(Self {
            ips: ip_names(&parsed),
            valid: validity.is_valid(),
            period: format!(
                "notBefore {}, notAfter {}",
                validity.not_before, validity.not_after
            ),
        })
    }

    /// Why the certificate cannot be served now, or `None` if it can.
    fn not_valid_now(&self) -> Option<String> {
        let period = &self.period;
        (!self.valid).then(|| format!("the certificate is not valid now ({period})"))
    }
}

/// The IP addresses the certificate `parsed` names in its subjectAltName.
fn ip_names(parsed: &x509_parser::certificate::X509Certificate<'_>) -> Vec<IpAddr> {
    // A subjectAltName that cannot be read names nothing.
    let alt = parsed.subject_alternative_name().ok().flatten();
    let names = alt
        .map(|ext| &ext.value.general_names[..])
        .unwrap_or_default();
    let ips = names.iter().filter_map(|name| match name {
        GeneralName::IPAddress(bytes) => ip_of(bytes),
        _ => None,
    });
    ips.collect()
}

/// The address of 4 bytes (IPv4) or 16 (IPv6).
pub(super) fn ip_of(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => <[u8; 4]>::try_from(bytes).ok().map(IpAddr::from),
        16 => <[u8; 16]>::try_from(bytes).ok().map(IpAddr::from),
        _ => None,
    }
}

/// Why `named` does not name `listen`, or `None` if it does.
pub(super) fn unnamed(named: &[IpAddr], listen: SocketAddr) -> Option<String> {
    let ip = listen.ip();
    let covered = if ip.is_unspecified() {
        !named.is_empty()
    } else {
        named.contains(&ip)
    };
    let list: Vec<String> = named.iter().map(ToString::to_string).collect();
    let why = format!(
        "the certificate has no IP subjectAltName for {ip} (it names [{}]); clients dial the \
         front by IP address and accept only a certificate that names it",
        list.join(", ")
    );
    (!covered).then_some(why)
}
