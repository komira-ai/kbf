//! The errors of the crate.

use std::io;

/// What is wrong with a key file or the directory it sits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyFileFault {
    /// The system refused or failed an operation.
    #[error("i/o error: {0:?}")]
    Io(io::ErrorKind),
    /// The path is a symbolic link.
    #[error("the path is a symbolic link")]
    Symlink,
    /// The key file is not a regular file, or the CA directory is not a directory.
    #[error("the path is not the kind of file expected")]
    NotRegular,
    /// Owned by someone other than the running account.
    #[error("owned by another account")]
    Owner,
    /// The key file is readable or writable by group or others.
    #[error("the key file's mode lets group or others in")]
    Mode,
    /// The CA directory lets group or others in.
    #[error("the CA directory's mode lets group or others in")]
    DirMode,
}

/// Why a certificate request is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CsrError {
    /// More than [`crate::MAX_CSR_LEN`] bytes.
    #[error("the request is longer than 2 KiB")]
    TooLarge,
    /// Not a PKCS #10 request in DER.
    #[error("the request does not parse")]
    Parse,
    /// Bytes after the request.
    #[error("bytes follow the request")]
    Trailing,
    /// A version other than 0.
    #[error("the request is not version 0")]
    Version,
    /// The key is not ECDSA on P-256.
    #[error("the key is not ECDSA on P-256")]
    KeyType,
    /// The signature algorithm is not ecdsa-with-SHA256.
    #[error("the signature algorithm is not ecdsa-with-SHA256")]
    SignatureAlgorithm,
    /// The request's signature does not verify under its own key.
    #[error("the request's signature does not verify")]
    Signature,
    /// The `issued_at` attribute is present but not one eight-byte time.
    #[error("the issued_at attribute is malformed or repeated")]
    IssuedAt,
}

/// Why the CA did not do what was asked.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaError {
    /// A key that is not ECDSA on P-256.
    #[error("the key is not ECDSA on P-256")]
    KeyType,
    /// The clock reads earlier than the build allows: the CA signs nothing.
    #[error("the clock ({now}) is before {floor}, the earliest time this build signs at")]
    ClockBeforeFloor {
        /// The time asked.
        now: i64,
        /// The earliest time this build signs at.
        floor: i64,
    },
    /// A time the certificate format cannot carry.
    #[error("the time is outside what a certificate carries")]
    TimeRange,
    /// A lifetime outside one hour to thirty days.
    #[error("the lifetime is outside 1 hour to 30 days")]
    Lifetime,
    /// A name that cannot go in a certificate.
    #[error("not a name a certificate can carry")]
    Name,
    /// A daemon certificate with an advertise host.
    #[error("a daemon certificate names only its node")]
    AdvertiseForDaemon,
    /// A frontend certificate with no names.
    #[error("a frontend certificate needs a name")]
    NoNames,
    /// The certificate could not be built or signed.
    #[error("certificate build failed: {0}")]
    Build(String),
    /// The in-process signer, where the key is a file in this process, in production.
    #[error("production refuses the in-process file signer")]
    InProcessSigner,
    /// The CA certificate is not what was expected.
    #[error("the CA certificate does not parse")]
    Parse,
    /// The certificate is not a CA certificate.
    #[error("the certificate is not a CA certificate")]
    NotCa,
    /// The certificate is not signed by its own key.
    #[error("the certificate is not self-signed")]
    NotSelfSigned,
    /// The signer's key is not the certificate's.
    #[error("the signer's key is not the certificate's key")]
    KeyMismatch,
    /// A rotation to the key already in use.
    #[error("the next CA key is the current one")]
    SameKey,
    /// A bundle with no certificate, or a PEM that is not certificates.
    #[error("the bundle is empty or not PEM certificates")]
    Bundle,
    /// A key file fault.
    #[error("key file: {0}")]
    KeyFile(#[from] KeyFileFault),
}

impl From<rcgen::Error> for CaError {
    fn from(e: rcgen::Error) -> Self {
        Self::Build(e.to_string())
    }
}
