//! The build's stored-result digest, and its bridge to the scheduler's [`ResultRef`].

use crate::{Digest, DigestFunction, ResultRef};

impl From<Digest> for ResultRef {
    /// Matching on the function makes a second hash function a compile error here,
    /// where a reference would have to say which function it is.
    fn from(digest: Digest) -> Self {
        match digest.function {
            DigestFunction::Sha256 => Self::new(digest.hash, digest.size_bytes),
        }
    }
}

impl From<ResultRef> for Digest {
    fn from(stored: ResultRef) -> Self {
        Self::new(DigestFunction::Sha256, *stored.id(), stored.size())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(tag: u8, size: u64) -> Digest {
        Digest::new(DigestFunction::Sha256, [tag; 32], size)
    }

    /// Catches: a bridge that loses or swaps the hash and the size, which would store a
    /// result under another digest than the worker uploaded it under.
    #[test]
    fn a_digest_survives_the_trip_through_a_result_ref() {
        for original in [digest(0, 0), digest(0xab, 42), digest(0xff, u64::MAX)] {
            let stored = ResultRef::from(original);
            assert_eq!(stored.id(), &original.hash);
            assert_eq!(stored.size(), original.size_bytes);
            assert_eq!(Digest::from(stored), original);
        }
    }

    /// Catches: references that order differently from the digests they stand for, which
    /// would reorder any sorted set of outcomes.
    #[test]
    fn result_refs_order_as_their_digests_do() {
        let digests = [digest(1, 9), digest(1, 10), digest(2, 0), digest(2, 1)];
        for a in digests {
            for b in digests {
                assert_eq!(ResultRef::from(a).cmp(&ResultRef::from(b)), a.cmp(&b));
            }
        }
    }
}
