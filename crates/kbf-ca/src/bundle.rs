//! The CA bundle: every root a node should trust, current first.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_types::CaPin;
use rustls::pki_types::pem::{self, SectionKind};
use sha2::{Digest, Sha256};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::error::CaError;

/// The pin of a root: the SHA-256 of its DER.
pub(crate) fn pin_of(der: &[u8]) -> CaPin {
    CaPin::from_bytes(Sha256::digest(der).into())
}

/// One or more DER CA certificates, the current root first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    certs: Vec<Vec<u8>>,
}

impl Bundle {
    /// A bundle of certificates this crate just built: not parsed again.
    pub(crate) const fn trusted(certs: Vec<Vec<u8>>) -> Self {
        Self { certs }
    }

    /// A bundle of `certs`, each one whole DER certificate.
    ///
    /// # Errors
    /// [`CaError::Bundle`] for none, and for one that does not parse.
    pub fn new(certs: Vec<Vec<u8>>) -> Result<Self, CaError> {
        let whole = |der: &Vec<u8>| matches!(X509Certificate::from_der(der), Ok((rest, _)) if rest.is_empty());
        if certs.is_empty() || !certs.iter().all(whole) {
            return Err(CaError::Bundle);
        }
        Ok(Self { certs })
    }

    /// The bundle of PEM text.
    ///
    /// # Errors
    /// [`CaError::Bundle`] for text with no certificate, with a block that is not a
    /// certificate, with damaged encoding, or with a certificate that does not parse.
    pub fn from_pem(pem: &[u8]) -> Result<Self, CaError> {
        let mut reader = pem;
        let mut certs = Vec::new();
        while let Some((kind, der)) = pem::from_buf(&mut reader).map_err(|_| CaError::Bundle)? {
            if kind != SectionKind::Certificate {
                return Err(CaError::Bundle);
            }
            certs.push(der);
        }
        Self::new(certs)
    }

    /// The certificates, current first.
    #[must_use]
    pub fn certs(&self) -> &[Vec<u8>] {
        &self.certs
    }

    /// The pin of the current root: the SHA-256 of its DER.
    #[must_use]
    pub fn pin(&self) -> CaPin {
        pin_of(&self.certs[0])
    }

    /// The PEM text: one `CERTIFICATE` block per certificate, 64 columns, in order.
    #[must_use]
    pub fn to_pem(&self) -> String {
        let mut out = String::new();
        for cert in &self.certs {
            out.push_str("-----BEGIN CERTIFICATE-----\n");
            let body: Vec<char> = STANDARD.encode(cert).chars().collect();
            for line in body.chunks(64) {
                out.extend(line);
                out.push('\n');
            }
            out.push_str("-----END CERTIFICATE-----\n");
        }
        out
    }
}
