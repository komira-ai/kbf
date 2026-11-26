//! What an Execute tells the dispatcher about an action tree write it started before its input
//! check ended ([`crate::Dispatch::start_tree`]): whether the check found every input held.
//!
//! The Execute starts the write once it knows what the tree is (its Directories are read and its
//! Command decoded), while it still asks the store whether the files are held. The write may
//! store objects either way: they are content-addressed and written create-only, so a write for
//! inputs that turn out absent leaves nothing wrong in the store. What a later run trusts without
//! a check (the executor's memo of written trees) waits for the finding: [`HeldFinding::wait`] is
//! true only once the Execute found every input held.

use tokio::sync::oneshot;

/// The Execute's side of a finding: [`InputsHeld::held`] once it found the Command and every
/// input held. Dropped without it (an input MISSING, a refused platform, a store that failed, or
/// the Execute dropped by its client), the finding is that they are not.
#[derive(Debug)]
pub struct InputsHeld(oneshot::Sender<()>);

/// The dispatcher's side of a finding.
#[derive(Debug)]
pub struct HeldFinding(oneshot::Receiver<()>);

/// A finding not yet made: the Execute keeps the first, the dispatcher the second.
#[must_use]
pub fn inputs_held() -> (InputsHeld, HeldFinding) {
    let (held, finding) = oneshot::channel();
    (InputsHeld(held), HeldFinding(finding))
}

impl InputsHeld {
    /// The Execute found the Command and every input held.
    pub fn held(self) {
        // A dispatcher that dropped its side has no use for the answer.
        let _ = self.0.send(());
    }
}

impl HeldFinding {
    /// Waits for the Execute's finding: true when it found every input held, false when it
    /// ended any other way.
    pub async fn wait(self) -> bool {
        self.0.await.is_ok()
    }
}

#[cfg(test)]
#[path = "early_tree_tests.rs"]
mod tests;
