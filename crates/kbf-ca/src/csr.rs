//! Certificate requests: the node's carrier for a public key and its proof of possession.
//!
//! The frontend takes nothing but the public key from a request. The parser therefore reads
//! only the skeleton (version, key, the attribute list, signature algorithm and signature)
//! and skips the subject and every extension unread, so nothing a request asks for can
//! reach a certificate by accident. The one attribute it looks at is `issued_at`, which a
//! renewal carries inside the signed part.

use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};

use crate::der;
use crate::error::CsrError;
use crate::key::PublicKey;

/// The longest request accepted, in bytes.
pub const MAX_CSR_LEN: usize = 2048;

/// The attribute type of the time a renewal request was made (`issued_at`): a SET holding
/// an OCTET STRING of the eight big-endian bytes of the unix seconds. There is no
/// registered arc for kbf; this one is in the UUID arc `2.25`, which needs no registration.
/// TODO: this constant moves to `kbf-types` (a P0 amendment), where the node and the
/// frontend can both take it.
pub const ISSUED_AT_OID: &[u64] = &[2, 25, 0x6b62_6672_6e65_776c];

/// The content octets of [`ISSUED_AT_OID`]: `2*40+25`, then the third arc in base 128.
const ISSUED_AT_OID_DER: [u8; 10] = [0x69, 0xeb, 0xb1, 0x99, 0xce, 0xa6, 0xf3, 0x95, 0xee, 0x6c];

/// `ecdsa-with-SHA256`: the whole content of the signature AlgorithmIdentifier (no
/// parameters, as RFC 5758 has it).
const ECDSA_WITH_SHA256: [u8; 10] = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];

/// What a valid request yields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCsr {
    /// The key the request proves possession of.
    pub key: PublicKey,
    /// The `issued_at` attribute, if the request has one.
    pub issued_at: Option<i64>,
}

/// Reads a request.
///
/// # Errors
/// [`CsrError`] naming the first rule it breaks.
pub fn parse_csr(der: &[u8]) -> Result<ParsedCsr, CsrError> {
    if der.len() > MAX_CSR_LEN {
        return Err(CsrError::TooLarge);
    }
    let (request, after) = der::read(der, 0x30).ok_or(CsrError::Parse)?;
    if !after.is_empty() {
        return Err(CsrError::Trailing);
    }
    let (info, rest) = der::read(request.content, 0x30).ok_or(CsrError::Parse)?;
    let (algorithm, rest) = der::read(rest, 0x30).ok_or(CsrError::Parse)?;
    let (signature, rest) = der::read(rest, 0x03).ok_or(CsrError::Parse)?;
    if !rest.is_empty() {
        return Err(CsrError::Parse);
    }

    let (version, rest) = der::read(info.content, 0x02).ok_or(CsrError::Parse)?;
    let (_subject, rest) = der::read(rest, 0x30).ok_or(CsrError::Parse)?;
    let (spki, rest) = der::read(rest, 0x30).ok_or(CsrError::Parse)?;
    let (attributes, rest) = der::read(rest, 0xa0).ok_or(CsrError::Parse)?;
    if !rest.is_empty() {
        return Err(CsrError::Parse);
    }

    if version.content != [0] {
        return Err(CsrError::Version);
    }
    let key = PublicKey::from_spki(spki.raw).map_err(|_| CsrError::KeyType)?;
    if algorithm.content != ECDSA_WITH_SHA256 {
        return Err(CsrError::SignatureAlgorithm);
    }
    let (&unused, signature) = signature.content.split_first().ok_or(CsrError::Parse)?;
    if unused != 0 {
        return Err(CsrError::Parse);
    }
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.point().as_slice())
        .verify(info.raw, signature)
        .map_err(|_| CsrError::Signature)?;

    Ok(ParsedCsr {
        key,
        issued_at: issued_at(attributes.content)?,
    })
}

/// The `issued_at` among the request's attributes, if there is one. Every attribute must
/// be `SEQUENCE { OID, SET }`; only `issued_at`'s value is read.
fn issued_at(mut attributes: &[u8]) -> Result<Option<i64>, CsrError> {
    let mut found = None;
    while !attributes.is_empty() {
        let (attribute, rest) = der::read(attributes, 0x30).ok_or(CsrError::Parse)?;
        attributes = rest;
        let (oid, rest) = der::read(attribute.content, 0x06).ok_or(CsrError::Parse)?;
        let (set, rest) = der::read(rest, 0x31).ok_or(CsrError::Parse)?;
        if !rest.is_empty() {
            return Err(CsrError::Parse);
        }
        if oid.content == ISSUED_AT_OID_DER {
            if found.is_some() {
                return Err(CsrError::IssuedAt);
            }
            found = Some(issued_at_value(set.content)?);
        }
    }
    Ok(found)
}

/// The value of an `issued_at` attribute: one OCTET STRING of eight bytes.
fn issued_at_value(set: &[u8]) -> Result<i64, CsrError> {
    let (octets, rest) = der::read(set, 0x04).ok_or(CsrError::IssuedAt)?;
    let bytes = <[u8; 8]>::try_from(octets.content).map_err(|_| CsrError::IssuedAt)?;
    if !rest.is_empty() {
        return Err(CsrError::IssuedAt);
    }
    Ok(i64::from_be_bytes(bytes))
}
