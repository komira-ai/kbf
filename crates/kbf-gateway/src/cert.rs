//! Who may register: a server certificate.
//!
//! TLS has already verified the caller's chain against the farm CA when the registration
//! listener sees it. What the chain does not say is which kind of farm member holds it,
//! and the daemons and clients of the farm hold certificates from the same CA. A server
//! certificate is one whose extended key usage allows serverAuth (the same certificate
//! must allow clientAuth too, or TLS would not have admitted it as a client) and which
//! names exactly one DNS name in its subjectAltName: the server's node name. A daemon's
//! certificate allows clientAuth only; a certificate with no extended key usage is not
//! a server certificate either, because the role is a statement the CA signed.

use tonic::Status;
use x509_parser::extensions::{GeneralName, ParsedExtension};

/// Why a certificate is not a server certificate.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The bytes are not an X.509 certificate.
    #[error("the client certificate does not parse: {0}")]
    Unparsed(String),
    /// The extended key usage does not allow serverAuth.
    #[error(
        "the client certificate is not a server certificate: its extended key usage does not allow serverAuth"
    )]
    NotServerAuth,
    /// The subjectAltName does not hold exactly one DNS name.
    #[error("a server certificate names exactly one DNS name as its node; this one names {0}")]
    DnsNames(usize),
}

impl From<Refusal> for Status {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unparsed(_) => Self::unauthenticated(refusal.to_string()),
            Refusal::NotServerAuth | Refusal::DnsNames(_) => {
                Self::permission_denied(refusal.to_string())
            }
        }
    }
}

/// The node name of the server certificate `der`.
///
/// # Errors
/// [`Refusal`]: it is not a certificate, not a server certificate, or does not name
/// exactly one node.
pub fn server_node(der: &[u8]) -> Result<String, Refusal> {
    let (_, cert) =
        x509_parser::parse_x509_certificate(der).map_err(|e| Refusal::Unparsed(e.to_string()))?;
    let mut server_auth = false;
    let mut names = Vec::new();
    for extension in cert.extensions() {
        match extension.parsed_extension() {
            ParsedExtension::ExtendedKeyUsage(usage) => server_auth |= usage.server_auth,
            ParsedExtension::SubjectAlternativeName(san) => {
                names.extend(san.general_names.iter().filter_map(|name| match name {
                    GeneralName::DNSName(dns) => Some((*dns).to_owned()),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    if !server_auth {
        return Err(Refusal::NotServerAuth);
    }
    match <[String; 1]>::try_from(names) {
        Ok([node]) => Ok(node),
        Err(names) => Err(Refusal::DnsNames(names.len())),
    }
}

#[cfg(test)]
#[path = "cert_tests.rs"]
mod tests;
