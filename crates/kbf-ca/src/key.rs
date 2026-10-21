//! An ECDSA P-256 public key, the only kind the CA signs for.

use crate::der;

/// `id-ecPublicKey` then `prime256v1`: the whole content of the AlgorithmIdentifier.
const P256_ALGORITHM: [u8; 19] = [
    0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, // id-ecPublicKey
    0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, // prime256v1
];

/// Not a SubjectPublicKeyInfo of an uncompressed ECDSA P-256 key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not an ECDSA P-256 SubjectPublicKeyInfo")]
pub struct KeyError;

/// A P-256 public key, held as the DER SubjectPublicKeyInfo and the 65-byte point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    spki: Vec<u8>,
    point: [u8; 65],
}

impl PublicKey {
    /// The key of a DER SubjectPublicKeyInfo.
    ///
    /// # Errors
    /// [`KeyError`] unless it is exactly `id-ecPublicKey` with curve `prime256v1` and an
    /// uncompressed point, with nothing after it. Whether the point is on the curve is
    /// not checked here: a request proves it by signing with the key.
    pub fn from_spki(spki: &[u8]) -> Result<Self, KeyError> {
        let (info, rest) = der::read(spki, 0x30).ok_or(KeyError)?;
        let (algorithm, after) = der::read(info.content, 0x30).ok_or(KeyError)?;
        let (bits, after) = der::read(after, 0x03).ok_or(KeyError)?;
        if !rest.is_empty() || !after.is_empty() || algorithm.content != P256_ALGORITHM {
            return Err(KeyError);
        }
        let (&unused, point) = bits.content.split_first().ok_or(KeyError)?;
        let point = <[u8; 65]>::try_from(point).map_err(|_| KeyError)?;
        if unused != 0 || point[0] != 4 {
            return Err(KeyError);
        }
        Ok(Self {
            spki: spki.to_vec(),
            point,
        })
    }

    /// The DER SubjectPublicKeyInfo: what a certificate for this key carries.
    #[must_use]
    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    /// The uncompressed point: `04`, then x and y.
    #[must_use]
    pub const fn point(&self) -> &[u8; 65] {
        &self.point
    }
}
