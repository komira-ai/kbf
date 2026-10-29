//! The two forms of a digest the tool handles: the REAPI message the Action and the collected
//! result carry, and the [`Digest`] the pack's manifest is written with.
//!
//! The text form of a digest is `<64 lowercase hex>/<size>`; one digest has exactly one.

use kbf_proto::reapi;
use kbf_types::{Digest, DigestFunction, ParseDigestError};

/// Reads `<64 lowercase hex>/<size>` as a SHA-256 digest: the form of `--action`.
///
/// # Errors
/// [`ParseDigestError`] when the text is anything else, uppercase hex included.
pub fn parse(text: &str) -> Result<Digest, ParseDigestError> {
    Digest::parse(DigestFunction::Sha256, text)
}

/// The digest an REAPI message holds, or why it is not a SHA-256 digest of the text form (a
/// short or uppercase hash, a negative size).
///
/// # Errors
/// [`ParseDigestError`].
pub fn from_reapi(digest: &reapi::Digest) -> Result<Digest, ParseDigestError> {
    parse(&format!("{}/{}", digest.hash, digest.size_bytes))
}

/// The REAPI message for `digest`. A size above `i64::MAX` saturates; no blob of this tool is
/// that large.
#[must_use]
pub fn to_reapi(digest: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: hex::encode(digest.hash),
        size_bytes: i64::try_from(digest.size_bytes).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a digest that does not survive the trip through its text form, or whose size is
    /// dropped on the way.
    #[test]
    fn the_text_form_round_trips() {
        let id = kbf_segments::sha256(b"hello");
        assert_eq!(
            id.to_string(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824/5"
        );
        assert_eq!(parse(&id.to_string()), Ok(id));
    }

    /// Catches an `--action` that is not one canonical digest being accepted: missing size,
    /// uppercase hex, a short hash.
    #[test]
    fn a_digest_in_another_form_is_refused() {
        let h = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(parse(h).is_err());
        assert!(parse(&format!("{}/5", h.to_uppercase())).is_err());
        assert!(parse("abc/5").is_err());
        assert!(parse(&format!("{h}/+5")).is_err());
    }

    /// Catches a conversion that loses the hash or the size in either direction.
    #[test]
    fn the_reapi_message_converts_both_ways() {
        let id = kbf_segments::sha256(b"x");
        let message = to_reapi(&id);
        assert_eq!(message.size_bytes, 1);
        assert_eq!(from_reapi(&message), Ok(id));
    }

    /// Catches a malformed REAPI digest (a bad hash, a negative size) being taken for a
    /// manifest digest, and an oversized one wrapping to a negative size.
    #[test]
    fn a_malformed_reapi_digest_is_refused() {
        let good = to_reapi(&kbf_segments::sha256(b"x"));
        let bad_hash = reapi::Digest {
            hash: "zz".to_owned(),
            size_bytes: 1,
        };
        assert!(from_reapi(&bad_hash).is_err());
        let negative = reapi::Digest {
            size_bytes: -1,
            ..good
        };
        assert!(from_reapi(&negative).is_err());
        let huge = Digest::new(DigestFunction::Sha256, [0; 32], u64::MAX);
        assert_eq!(to_reapi(&huge).size_bytes, i64::MAX);
    }
}
