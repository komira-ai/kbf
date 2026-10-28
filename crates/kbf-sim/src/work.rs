//! The work key a build scenario submits under.

use kbf_types::{ActionKey, Digest, WorkKey};

/// The key of action `action` on REAPI instance `instance`: the class is the instance
/// name and the dedup text is the digest's text form, as the build's bridge makes it.
#[must_use]
pub fn work_key(instance: &str, action: Digest) -> WorkKey {
    ActionKey {
        instance: instance.to_owned(),
        action,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use kbf_types::DigestFunction;

    use super::*;

    /// Catches: a helper that builds a key the bridge reads back as another action.
    #[test]
    fn the_key_reads_back_as_the_instance_and_digest() {
        let action = Digest::new(DigestFunction::Sha256, [3; 32], 9);
        let key = work_key("main", action);
        assert_eq!(key.class, "main");
        assert_eq!(
            ActionKey::try_from(&key),
            Ok(ActionKey {
                instance: "main".to_owned(),
                action
            })
        );
    }
}
