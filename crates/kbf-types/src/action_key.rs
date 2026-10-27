//! The build's dedup key, and its bridge to the scheduler's [`WorkKey`].

use crate::{Digest, DigestFunction, ParseDigestError, WorkKey};

/// What makes two executions the same work: the REAPI instance name and the action
/// digest. In-flight dedup joins a second call to a running twin only when both match;
/// the same digest under another instance is different work (another namespace, maybe
/// another cache), so it never joins.
///
/// The scheduler does not see this type: it keys on a [`WorkKey`]. An `ActionKey` becomes
/// one with `WorkKey::from` (the instance name is the key's class, the digest's text form
/// `<lowercase hex>/<size>` is its dedup text) and comes back with `ActionKey::try_from`,
/// which fails for a `WorkKey` whose dedup text is not such a digest.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionKey {
    /// The REAPI instance name the client sent.
    pub instance: String,
    /// The digest of the `Action` message.
    pub action: Digest,
}

/// Why a [`WorkKey`] is not the key of an action.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the work key's dedup text is not an action digest: {0}")]
pub struct NotAnActionKey(#[from] pub ParseDigestError);

/// The dedup text of an action digest. Matching on the function makes a second hash
/// function a compile error here, where its text form has to say which function it is.
fn dedup_text(action: &Digest) -> String {
    match action.function {
        DigestFunction::Sha256 => action.to_string(),
    }
}

impl From<ActionKey> for WorkKey {
    fn from(key: ActionKey) -> Self {
        Self {
            dedup: dedup_text(&key.action),
            class: key.instance,
        }
    }
}

impl From<&ActionKey> for WorkKey {
    fn from(key: &ActionKey) -> Self {
        Self {
            class: key.instance.clone(),
            dedup: dedup_text(&key.action),
        }
    }
}

impl TryFrom<&WorkKey> for ActionKey {
    type Error = NotAnActionKey;

    fn try_from(key: &WorkKey) -> Result<Self, Self::Error> {
        Ok(Self {
            instance: key.class.clone(),
            action: Digest::parse(DigestFunction::Sha256, &key.dedup)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(instance: &str, tag: u8, size: u64) -> ActionKey {
        ActionKey {
            instance: instance.to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], size),
        }
    }

    /// Catches: a bridge that loses the instance name or any part of the digest, which
    /// would join two different actions or split one.
    #[test]
    fn an_action_key_round_trips_through_a_work_key() {
        for original in [key("main", 7, 0), key("a/b", 255, u64::MAX), key("", 0, 1)] {
            let work = WorkKey::from(&original);
            assert_eq!(WorkKey::from(original.clone()), work);
            assert_eq!(ActionKey::try_from(&work), Ok(original));
        }
    }

    /// Catches: a dedup text that is not the digest's own text form, which would change
    /// every durable row name derived from it.
    #[test]
    fn the_dedup_text_is_the_digest_text_and_the_class_is_the_instance() {
        let work = WorkKey::from(key("main", 0xab, 42));
        assert_eq!(work.class, "main");
        assert_eq!(work.dedup, format!("{}/42", "ab".repeat(32)));
    }

    /// Catches: two keys that differ only in the instance or only in the digest
    /// comparing equal as work keys.
    #[test]
    fn keys_differing_in_either_part_stay_different() {
        let base = WorkKey::from(key("main", 1, 1));
        assert_ne!(base, WorkKey::from(key("other", 1, 1)));
        assert_ne!(base, WorkKey::from(key("main", 2, 1)));
        assert_ne!(base, WorkKey::from(key("main", 1, 2)));
    }

    /// Catches: keys ordered differently as work keys, which would change the order a
    /// map of in-flight work is walked in (the instance first, then the hash).
    #[test]
    fn work_keys_order_as_the_action_keys_do() {
        let keys = [
            key("a", 2, 5),
            key("a", 1, 9),
            key("b", 0, 0),
            key("a", 1, 3),
        ];
        let mut by_action: Vec<ActionKey> = keys.to_vec();
        by_action.sort();
        let mut by_work: Vec<WorkKey> = keys.iter().map(WorkKey::from).collect();
        by_work.sort();
        let back: Vec<ActionKey> = by_work
            .iter()
            .map(|w| ActionKey::try_from(w).unwrap())
            .collect();
        assert_eq!(back, by_action);
    }

    /// Catches: a work key that is not an action converting anyway, which would hand a
    /// made-up digest to the code that answers a client.
    #[test]
    fn a_work_key_that_is_not_a_digest_is_refused() {
        let bad = WorkKey::new("main", "not a digest");
        let err = ActionKey::try_from(&bad).unwrap_err();
        assert!(matches!(err.0, ParseDigestError::MissingSeparator(_)));
        assert!(err.to_string().contains("not an action digest"), "{err}");
    }
}
