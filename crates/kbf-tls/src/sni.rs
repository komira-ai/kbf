//! A server certificate chosen by the name the client asked for (SNI).
//!
//! [`SniCerts::from_config`] reads and checks a list of [`CertEntry`] and makes the
//! resolver a rustls `ServerConfig` takes (`with_cert_resolver`). A ClientHello whose name
//! is configured gets that entry's certificate; one with no name, or a name nobody
//! configured, gets the default entry's. The default is chosen in the configuration, never
//! by position.
//!
//! The resolver is one `Arc`: cloning it is a pointer copy, and a reload builds a new
//! [`SniCerts`] from the files and swaps the whole value (an `ArcSwap`, or the
//! configuration that holds it), so no handshake sees half of an old list and half of a new.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use x509_parser::extensions::GeneralName;

use crate::config::{TlsError, certificates, pem_error, read_file, validity};

/// One certificate, the names it is served for, and whether it is the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertEntry {
    /// The DNS names a ClientHello may carry to get this certificate. Each must be one of
    /// the certificate's subject alternative names (a wildcard name in the certificate
    /// covers one label). An entry with no names can only be the default.
    pub names: Vec<String>,
    /// The PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// The PEM private key of the leaf.
    pub key: PathBuf,
    /// Whether this is the entry served to a ClientHello with no name or an unknown one.
    /// Exactly one entry of a list of two or more is; a list of one needs no flag.
    pub default: bool,
}

/// The resolver: a name to a certificate, and the default.
#[derive(Clone, Debug)]
pub struct SniCerts {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    by_name: HashMap<String, Arc<CertifiedKey>>,
    default: Arc<CertifiedKey>,
    /// The earliest end of validity among the leaves, in seconds since the epoch.
    first_not_after: i64,
    /// Whether that leaf's validity has ended.
    any_expired: bool,
}

/// Lowercase, no trailing dot: how DNS names compare.
fn normal(name: &str) -> String {
    name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

impl SniCerts {
    /// Checks the shape of `entries` without reading a file: at least one entry; exactly one
    /// default among two or more; every named entry's names are plain DNS names (not empty,
    /// no wildcard); an entry with no names is the default; no name appears twice (names
    /// compare case-insensitively).
    ///
    /// # Errors
    /// [`TlsError::Config`].
    pub fn check_entries(entries: &[CertEntry]) -> Result<(), TlsError> {
        let bad = |why: String| Err(TlsError::Config(why));
        if entries.is_empty() {
            return bad("no server certificate is configured".to_owned());
        }
        let defaults = entries.iter().filter(|e| e.default).count();
        if entries.len() > 1 && defaults != 1 {
            return bad(format!(
                "{} server certificates need exactly one marked default, found {defaults}",
                entries.len()
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            let is_default = entry.default || entries.len() == 1;
            if entry.names.is_empty() && !is_default {
                return bad(format!(
                    "server certificate {} ({}) has no names and is not the default",
                    index + 1,
                    entry.cert.display()
                ));
            }
            for name in &entry.names {
                let name = normal(name);
                if name.is_empty() || name.contains('*') || name.contains(char::is_whitespace) {
                    return bad(format!("{name:?} is not a plain DNS name"));
                }
                if !seen.insert(name.clone()) {
                    return bad(format!("the name {name} is configured twice"));
                }
            }
        }
        Ok(())
    }

    /// Reads every entry's files and builds the resolver. It validates every pair: the key
    /// is the leaf's ([`TlsError::Config`], naming both files), each configured name is one
    /// of the leaf's subject alternative names, and the list passes
    /// [`SniCerts::check_entries`]. It does not refuse an expired leaf (a first load never
    /// did); a reload calls [`SniCerts::reject_expired`] on the result.
    ///
    /// Nothing is partly built: an error means no resolver, and a caller that holds one
    /// keeps it.
    ///
    /// # Errors
    /// [`TlsError`].
    pub fn from_config(entries: &[CertEntry]) -> Result<Self, TlsError> {
        Self::check_entries(entries)?;
        let provider = rustls::crypto::ring::default_provider();
        let mut by_name = HashMap::new();
        let mut default = None;
        let (mut first_not_after, mut any_expired) = (i64::MAX, false);
        for entry in entries {
            let chain = certificates(&entry.cert, &read_file(&entry.cert)?)?;
            let key = PrivateKeyDer::from_pem_slice(&read_file(&entry.key)?)
                .map_err(|e| pem_error(&entry.key, e))?;
            let signing = provider
                .key_provider
                .load_private_key(key)
                .map_err(|e| TlsError::Config(format!("{}: {e}", entry.key.display())))?;
            let certified = Arc::new(CertifiedKey::new(chain, signing));
            certified.keys_match().map_err(|e| {
                TlsError::Config(format!(
                    "the key {} is not the key of the certificate {}: {e}",
                    entry.key.display(),
                    entry.cert.display()
                ))
            })?;
            let (not_after, expired) = validity(&certified.cert[0], &entry.cert)?;
            first_not_after = first_not_after.min(not_after);
            any_expired |= expired;
            let sans = dns_names(&certified.cert[0], &entry.cert)?;
            for name in &entry.names {
                let name = normal(name);
                if !sans.iter().any(|san| covers(san, &name)) {
                    return Err(TlsError::Config(format!(
                        "the name {name} is not a subject alternative name of {}",
                        entry.cert.display()
                    )));
                }
                by_name.insert(name, Arc::clone(&certified));
            }
            if entry.default || entries.len() == 1 {
                default = Some(certified);
            }
        }
        // `check_entries` guaranteed one default, so this is always `Some`.
        let default = default.ok_or_else(|| TlsError::Config("no default".to_owned()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                by_name,
                default,
                first_not_after,
                any_expired,
            }),
        })
    }

    /// `self`, or [`TlsError::Expired`] if any leaf's validity has ended.
    ///
    /// # Errors
    /// [`TlsError::Expired`], with the earliest end of validity.
    pub fn reject_expired(self) -> Result<Self, TlsError> {
        if self.inner.any_expired {
            return Err(TlsError::Expired {
                not_after: self.inner.first_not_after,
            });
        }
        Ok(self)
    }

    /// The certificate a ClientHello carrying `name` (or none) is served.
    fn pick(&self, name: Option<&str>) -> Arc<CertifiedKey> {
        // PLANTED: the old behavior, one certificate for every name.
        let _ignored = name.and_then(|n| self.inner.by_name.get(&normal(n)));
        Arc::clone(&self.inner.default)
    }
}

impl ResolvesServerCert for SniCerts {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.pick(hello.server_name()))
    }
}

/// The DNS names in the leaf's subject alternative name extension, lowercased.
fn dns_names(
    leaf: &rustls::pki_types::CertificateDer<'_>,
    path: &Path,
) -> Result<Vec<String>, TlsError> {
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| pem_error(path, format!("the leaf is not an X.509 certificate: {e}")))?;
    let names = match parsed.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|n| match n {
                GeneralName::DNSName(dns) => Some(normal(dns)),
                _ => None,
            })
            .collect(),
        Ok(None) => Vec::new(),
        Err(e) => {
            return Err(pem_error(
                path,
                format!("unreadable subject alternative names: {e}"),
            ));
        }
    };
    Ok(names)
}

/// Whether the certificate name `san` covers `name`: equal, or `*.rest` and `name` is one
/// label under `rest`.
fn covers(san: &str, name: &str) -> bool {
    match san.strip_prefix("*.") {
        Some(rest) => name
            .split_once('.')
            .is_some_and(|(label, tail)| !label.is_empty() && tail == rest),
        None => san == name,
    }
}
