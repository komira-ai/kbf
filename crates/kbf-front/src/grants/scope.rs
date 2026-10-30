//! What a grant may read: the Action, its Command and its input closure.

use std::collections::HashSet;

use kbf_types::Digest;
use tonic::Status;

use super::token::Claims;

/// The digests a grant for one Action may read, known server side.
#[derive(Debug)]
pub struct Scope {
    action: Digest,
    command: Digest,
    closure: HashSet<Digest>,
}

impl Scope {
    /// The scope of `action`: itself, its `command` and the digests of its input
    /// closure (the Directory blobs and the files of the input root's walk).
    #[must_use]
    pub fn new(action: Digest, command: Digest, closure: impl IntoIterator<Item = Digest>) -> Self {
        Self {
            action,
            command,
            closure: closure.into_iter().collect(),
        }
    }

    /// Whether `digest` is in the scope.
    #[must_use]
    pub fn allows(&self, digest: &Digest) -> bool {
        *digest == self.action || *digest == self.command || self.closure.contains(digest)
    }

    /// Whether the grant `claims` may read every one of `digests`.
    ///
    /// # Errors
    /// PERMISSION_DENIED when the grant is for another Action or a digest is out of scope.
    pub fn check(&self, claims: &Claims, digests: &[Digest]) -> Result<(), Status> {
        if claims.action != self.action {
            return Err(Status::permission_denied(
                "the read grant is for another action",
            ));
        }
        match digests.iter().find(|d| !self.allows(d)) {
            Some(out) => Err(Status::permission_denied(format!(
                "{out} is not in the read grant's scope"
            ))),
            None => Ok(()),
        }
    }
}
