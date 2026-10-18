//! The public key type and the text of every error: what a caller reads back.

mod support;

use std::io::ErrorKind;

use kbf_ca::{CaError, CsrError, KeyError, KeyFileFault, PublicKey, SignError};
use rcgen::{KeyPair, PublicKeyData};
use support::{EC_PUBLIC_KEY, PRIME256V1, cat, seq, spki_with, tlv};

// Catches: a key type that keeps bytes other than the key it was given, or that accepts a
// SubjectPublicKeyInfo with anything around it (bytes after it, an empty key, a wrong tag).
#[test]
fn a_key_is_exactly_a_p256_subject_public_key_info() {
    let pair = KeyPair::generate().expect("key");
    let spki = pair.subject_public_key_info();
    let key = PublicKey::from_spki(&spki).expect("a P-256 key");
    assert_eq!(key.spki(), spki);
    assert_eq!(key.point().as_slice(), pair.public_key_raw());
    assert_eq!(key.point()[0], 4);

    let mut after = spki.clone();
    after.push(0);
    let ec = cat(&[&EC_PUBLIC_KEY, &PRIME256V1]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("bytes after the key info", after),
        ("nothing", Vec::new()),
        ("not a sequence", tlv(0x31, &[0])),
        (
            "an empty bit string element",
            seq(&cat(&[&seq(&ec), &tlv(0x03, &[])])),
        ),
        ("a one-byte bit string", spki_with(&ec, 0, &[])),
    ];
    for (what, bytes) in cases {
        assert_eq!(PublicKey::from_spki(&bytes), Err(KeyError), "{what}");
    }
}

// Catches: a message that changed unannounced, or one that is empty: these strings reach
// logs and the operator, and the wire refusals are chosen from them elsewhere.
#[test]
fn every_error_has_its_message() {
    let cases: Vec<(String, &str)> = vec![
        (
            KeyError.to_string(),
            "not an ECDSA P-256 SubjectPublicKeyInfo",
        ),
        (
            SignError("hsm offline".to_owned()).to_string(),
            "the signer failed: hsm offline",
        ),
        (
            CsrError::TooLarge.to_string(),
            "the request is longer than 2 KiB",
        ),
        (CsrError::Parse.to_string(), "the request does not parse"),
        (CsrError::Trailing.to_string(), "bytes follow the request"),
        (
            CsrError::Version.to_string(),
            "the request is not version 0",
        ),
        (
            CsrError::KeyType.to_string(),
            "the key is not ECDSA on P-256",
        ),
        (
            CsrError::SignatureAlgorithm.to_string(),
            "the signature algorithm is not ecdsa-with-SHA256",
        ),
        (
            CsrError::Signature.to_string(),
            "the request's signature does not verify",
        ),
        (
            CsrError::IssuedAt.to_string(),
            "the issued_at attribute is malformed or repeated",
        ),
        (
            KeyFileFault::Io(ErrorKind::NotFound).to_string(),
            "i/o error: NotFound",
        ),
        (
            KeyFileFault::Symlink.to_string(),
            "the path is a symbolic link",
        ),
        (
            KeyFileFault::NotRegular.to_string(),
            "the path is not the kind of file expected",
        ),
        (KeyFileFault::Owner.to_string(), "owned by another account"),
        (
            KeyFileFault::Mode.to_string(),
            "the key file's mode lets group or others in",
        ),
        (
            KeyFileFault::DirMode.to_string(),
            "the CA directory's mode lets group or others in",
        ),
        (
            CaError::KeyType.to_string(),
            "the key is not ECDSA on P-256",
        ),
        (
            CaError::ClockBeforeFloor { now: 5, floor: 9 }.to_string(),
            "the clock (5) is before 9, the earliest time this build signs at",
        ),
        (
            CaError::TimeRange.to_string(),
            "the time is outside what a certificate carries",
        ),
        (
            CaError::Lifetime.to_string(),
            "the lifetime is outside 1 hour to 30 days",
        ),
        (
            CaError::Name.to_string(),
            "not a name a certificate can carry",
        ),
        (
            CaError::AdvertiseForDaemon.to_string(),
            "a daemon certificate names only its node",
        ),
        (
            CaError::NoNames.to_string(),
            "a frontend certificate needs a name",
        ),
        (
            CaError::Build("why".to_owned()).to_string(),
            "certificate build failed: why",
        ),
        (
            CaError::InProcessSigner.to_string(),
            "production refuses the in-process file signer",
        ),
        (
            CaError::Parse.to_string(),
            "the CA certificate does not parse",
        ),
        (
            CaError::NotCa.to_string(),
            "the certificate is not a CA certificate",
        ),
        (
            CaError::NotSelfSigned.to_string(),
            "the certificate is not self-signed",
        ),
        (
            CaError::KeyMismatch.to_string(),
            "the signer's key is not the certificate's key",
        ),
        (
            CaError::SameKey.to_string(),
            "the next CA key is the current one",
        ),
        (
            CaError::Bundle.to_string(),
            "the bundle is empty or not PEM certificates",
        ),
        (
            CaError::KeyFile(KeyFileFault::Symlink).to_string(),
            "key file: the path is a symbolic link",
        ),
    ];
    for (got, want) in cases {
        assert_eq!(got, want);
    }
}
