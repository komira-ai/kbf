//! Why a migration stopped, and the exit each way ends with ([`Exit`]).

use std::io;

use super::tree::TreeError;
use crate::exit::Exit;
use crate::state::StateError;

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// Applying needs the user that owns the new tree.
    #[error("migrating changes root's files: run it as root (this is uid {have}, not {want})")]
    NotRoot { have: u32, want: u32 },
    /// A swap is on trial: there is one confirm window at a time, and the move must not
    /// take a release out from under it.
    #[error(
        "release {commit} is on trial (activated, not confirmed): confirm or revert it with the old tool first"
    )]
    Pending { commit: String },
    /// The old state and the old links say different things (an interrupted verb of the
    /// old tool): the migration will not guess which is right.
    #[error("the old layout disagrees with itself: {0}")]
    Disagree(String),
    /// The old layout moved between the migration's first look and its locks (a verb of the
    /// old tools, or another run of this verb): what was probed and planned is not what is
    /// there now. Nothing was migrated; a run again starts from what is there.
    #[error(
        "the old layout changed while the migration waited for its locks ({0}): nothing was migrated, run it again"
    )]
    Changed(String),
    /// A link of the old layout that is not `releases/<40 hex>`.
    #[error("{link} points at {target:?}, not at releases/<commit>")]
    BadLink { link: &'static str, target: String },
    /// The directory is named for one commit and the binary in it is another.
    #[error("releases/{dir} holds a build of {probed}: its name does not match its binary")]
    Mislabeled { dir: String, probed: String },
    /// `--version --json` of an old release could not be had.
    #[error("{release}: the version probe failed: {why}")]
    Probe { release: String, why: String },
    /// The floor cannot be seeded and the operator did not give one.
    #[error("{0}")]
    FloorUnavailable(String),
    /// A file the migration wrote earlier does not read back as it should.
    #[error("{file}: {why}")]
    Corrupt { file: &'static str, why: String },
    /// The new layout already manages this host without a migration having run.
    #[error("{0}")]
    Conflict(String),
    /// The old layout's state is still the host's: `setup` and the installer's verbs wait
    /// for the migration, so the old helper and the installer never both manage a host.
    #[error(
        "this host still has the old layout's state ({0}) and is not migrated: run `kbf-installer migrate --apply` first"
    )]
    Unmigrated(String),
    /// A rollback that would throw away what the installer did since.
    #[error("cannot roll back: {0}")]
    MovedOn(String),
    /// There is nothing to roll back.
    #[error("cannot roll back: this host was not migrated (no migration.json)")]
    NoJournal,
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("{what}: {source}")]
    Io { what: String, source: io::Error },
}

impl MigrateError {
    pub fn io<E: Into<io::Error>>(what: impl Into<String>) -> impl FnOnce(E) -> MigrateError {
        let what = what.into();
        move |source| MigrateError::Io {
            what,
            source: source.into(),
        }
    }

    /// How the verb ends (docs/design/kbf-installer.md section 2).
    pub fn exit(&self) -> Exit {
        match self {
            MigrateError::NotRoot { .. } => Exit::Usage,
            MigrateError::BadLink { .. }
            | MigrateError::Mislabeled { .. }
            | MigrateError::Probe { .. } => Exit::Refused,
            // A swap of the old tool on trial, and the disagreements only a person settles.
            MigrateError::Pending { .. }
            | MigrateError::Disagree(_)
            | MigrateError::Changed(_)
            | MigrateError::FloorUnavailable(_)
            | MigrateError::Conflict(_)
            | MigrateError::Unmigrated(_)
            | MigrateError::MovedOn(_)
            | MigrateError::NoJournal => Exit::NeedsOperator,
            MigrateError::State(e) if e.needs_operator() => Exit::NeedsOperator,
            MigrateError::Corrupt { .. }
            | MigrateError::Tree(_)
            | MigrateError::State(_)
            | MigrateError::Io { .. } => Exit::Failed,
        }
    }
}
