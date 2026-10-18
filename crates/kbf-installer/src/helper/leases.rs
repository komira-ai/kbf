//! The installer's own view of the leases on this host: the check that refuses an
//! activation while work is running is made here, from what root observes, never from
//! anything in the order record (the node's check is a courtesy, and the record is the
//! service user's word).
//!
//! The real source (Linux: the cgroup of the action unit; macOS: the launchd job and the
//! processes by lease uid) is wired by a later change; until then the binary uses
//! [`NotWired`], which says it cannot tell, and an activation is refused (fail closed).

/// How many leases are live on this host.
pub trait LeaseSource {
    /// The count, or why it cannot be told.
    fn live(&self) -> Result<usize, String>;
}

/// The source of a build that has none.
pub struct NotWired;

impl LeaseSource for NotWired {
    fn live(&self) -> Result<usize, String> {
        Err("this installer has no lease source wired in".to_owned())
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::LeaseSource;

    /// A count a test sets; `usize::MAX` is a source that cannot answer.
    #[derive(Clone, Default)]
    pub struct FakeLeases(pub Arc<AtomicUsize>);

    impl FakeLeases {
        pub fn set(&self, n: usize) {
            self.0.store(n, Ordering::SeqCst);
        }
    }

    impl LeaseSource for FakeLeases {
        fn live(&self) -> Result<usize, String> {
            match self.0.load(Ordering::SeqCst) {
                usize::MAX => Err("the cgroup cannot be read".to_owned()),
                n => Ok(n),
            }
        }
    }
}
