//! What names a run in the `update/desired` row: the release, where it comes from, and the
//! generation. Plain data; nothing here reads a clock or a store.

use serde::{Deserialize, Serialize};

/// A release, by whatever is compared: a commit or a software-set digest. Only equality is
/// defined, so a rollback is another target and never "an older one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release(String);

impl Release {
    /// The release called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Its name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the release of a run comes from. The default is [`Source::Fetch`], what every run
/// has meant so far, so a row that names no source is a fetch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The hosts pull the release from the source their installer is configured with.
    #[default]
    Fetch,
    /// The hosts use the release their installer's package carries.
    Staged,
}

/// The generation of a run (`update/desired`): it grows with every change of the desired
/// release, and a rollback is the older release under a higher generation
/// (`docs/design/pull-updates.md` section 5). A higher generation is a new run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);
