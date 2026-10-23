//! The flags of `setup` that no longer do anything. `setup` used to install a first release and
//! to write where releases come from (a release source, and the keys that go with one); kbf
//! updates are OS packages now, so it does neither. The flags that asked for it are still taken
//! for one release, so that a script passing them goes on working: they are ignored, and `setup`
//! says so once, naming the ones that were given.
//!
//! Nothing reads the values, so they are not checked: a value that was refused before is
//! accepted now.

/// The flags as the command line gave them.
#[derive(clap::Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct RetiredFlags {
    /// Retired: accepted and ignored for one release, with a warning (setup records no release
    /// source)
    #[arg(long)]
    pub release_source: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup records no release
    /// source)
    #[arg(long)]
    pub release_url: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup writes no release
    /// keys)
    #[arg(long)]
    pub release_ca: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup writes no release
    /// keys)
    #[arg(long)]
    pub frontend_worker_url: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup writes no release
    /// keys)
    #[arg(long)]
    pub frontend_ca: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup writes no release
    /// keys)
    #[arg(long)]
    pub frontend_ca_sha256: Option<String>,
    /// Retired: accepted and ignored for one release, with a warning (setup installs no
    /// release)
    #[arg(long)]
    pub commit: Option<String>,
}

impl RetiredFlags {
    /// The names of the flags that were given, in the order they are declared in.
    pub fn given(&self) -> Vec<&'static str> {
        [
            ("--release-source", &self.release_source),
            ("--release-url", &self.release_url),
            ("--release-ca", &self.release_ca),
            ("--frontend-worker-url", &self.frontend_worker_url),
            ("--frontend-ca", &self.frontend_ca),
            ("--frontend-ca-sha256", &self.frontend_ca_sha256),
            ("--commit", &self.commit),
        ]
        .into_iter()
        .filter(|(_, value)| value.is_some())
        .map(|(name, _)| name)
        .collect()
    }

    /// The one line that tells the operator these flags did nothing; `None` when none was given.
    pub fn warning(&self) -> Option<String> {
        let given = self.given();
        if given.is_empty() {
            return None;
        }
        Some(format!(
            "warning: ignored: {}: setup no longer installs a release or writes a release source or its keys (kbf updates are OS packages); these flags are accepted for one release so that scripts keep working, and will then be removed",
            given.join(", ")
        ))
    }
}
