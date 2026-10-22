//! What `setup` does through the rest of the installer: the first release, the state
//! database. Each is a trait with one real implementation here, so the tests of `run` play the
//! host and the server and the implementations are tested on their own.

use std::cell::RefCell;

use kbf_host::Exec;
use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema;
use kbf_update::Source;

use super::SetupError;
use crate::CommitId;
use crate::activate;
use crate::binary;
use crate::database::{Database, SchemaOps};
use crate::helper::fetcher::{FetchError, Fetcher, ReleaseLocator};
use crate::layout::Layout;
use crate::release::ReleaseError;
use crate::seq_floor::{self, FloorError};
use crate::state;

/// The installer's one downloader, as `setup` uses it: fetch the release for `commit` from the
/// source `installer.toml` names (setup has written it by then), verify it as an update is
/// verified, install it as `releases/<commit>` and make it the release that runs.
pub trait Releases {
    /// # Errors
    /// Why the release could not be fetched, verified or made current.
    fn install_first(&self, commit: &CommitId) -> Result<(), SetupError>;

    /// [`Releases::install_first`] for the release the host's package carries, which a host
    /// set up with `--release-source bundled` has no commit to name for: verified as a
    /// fetched one is, installed and made the release that runs. Returns its commit. A
    /// downloader with no package says so.
    ///
    /// # Errors
    /// There is no package, or why the release could not be verified or made current.
    fn install_seed(&self) -> Result<CommitId, SetupError> {
        Err(SetupError::Fetch(FetchError::NotWired))
    }

    /// Whether the release that runs on this host declares the relay a host that fetches
    /// nothing needs of it (`kbf --version --json`, [`crate::binary::RELAY_NEEDED`]): `false`
    /// when none is installed. A downloader that cannot run a binary says so.
    ///
    /// # Errors
    /// The release that runs cannot be asked.
    fn running_relays(&self) -> Result<bool, SetupError> {
        Err(SetupError::Fetch(FetchError::NotWired))
    }
}

/// The first release through the [`Fetcher`]: the one path by which release bytes reach the
/// host. It names only the commit; the source says what the commit holds.
pub struct Fetching<'f, E> {
    fetcher: &'f dyn Fetcher,
    layout: Layout,
    exec: RefCell<E>,
}

impl<'f, E: Exec> Fetching<'f, E> {
    pub fn new(fetcher: &'f dyn Fetcher, layout: Layout, exec: E) -> Fetching<'f, E> {
        Fetching {
            fetcher,
            layout,
            exec: RefCell::new(exec),
        }
    }
}

impl<E: Exec> Fetching<'_, E> {
    /// The release `commit` from `source`, verified, installed and made current. The first
    /// release is the one case in which nothing runs yet, so no floor is asked of the seed.
    fn install(&self, commit: &CommitId, source: Source) -> Result<(), SetupError> {
        let installed = self.fetcher.fetch(&ReleaseLocator {
            commit: commit.clone(),
            components: Vec::new(),
            source,
        })?;
        if installed.commit != *commit {
            return Err(SetupError::Fetch(FetchError::Rejected(format!(
                "asked for {commit}, got {}",
                installed.commit
            ))));
        }
        activate::first(
            &mut *self.exec.borrow_mut(),
            &self.layout,
            commit,
            &mut std::io::sink(),
        )?;
        Ok(())
    }
}

impl<E: Exec> Releases for Fetching<'_, E> {
    fn install_first(&self, commit: &CommitId) -> Result<(), SetupError> {
        self.install(commit, Source::Fetch)
    }

    fn install_seed(&self) -> Result<CommitId, SetupError> {
        let seed = self.fetcher.seed()?;
        self.install(&seed.commit, Source::Staged)?;
        // The package's release is the release that runs now, so the floor starts at its `seq`:
        // no package at or below it is a seed above the floor. The release stands if the file
        // cannot be written; the run loop writes it once the cause is gone.
        let kept = seq_floor::raise(&self.layout, seed.seq);
        kept.map_err(floor_not_kept)?;
        Ok(seed.commit)
    }

    fn running_relays(&self) -> Result<bool, SetupError> {
        let state = state::read_unlocked(&self.layout).map_err(ReleaseError::from)?;
        let Some(current) = state.and_then(|state| state.current) else {
            return Ok(false);
        };
        Ok(binary::relays(&self.fetcher.probe(&current)?))
    }
}

/// The first release is installed and current; only its floor could not be written, which is the
/// operator's to put right (the run loop writes it once they have).
fn floor_not_kept(e: FloorError) -> SetupError {
    SetupError::NeedsOperator(format!(
        "the package's release is installed and current, but its floor on seq cannot be kept: \
         {e}; once that is fixed, run `setup` again (the release stands) and `kbf-installer run` \
         then keeps it"
    ))
}

/// Where the state database is reached from.
pub trait Schemas {
    /// The schema runner for the database `url` names (validated: no password).
    ///
    /// # Errors
    /// The URL is not one the Postgres client takes.
    fn of(&self, url: &str) -> Result<Box<dyn SchemaOps>, SetupError>;
}

/// The real server, with the migrations embedded in this build.
pub struct Postgres;

impl Schemas for Postgres {
    fn of(&self, url: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        // The client's own messages may quote the URL; none is kept.
        let config = PostgresConfig::parse(url)
            .map_err(|_| SetupError::Database("the --database URL is not usable".to_owned()))?;
        Ok(Box::new(Database::new(config, schema::embedded())))
    }
}

/// The two, as `run` takes them.
pub struct Backend<'a> {
    pub releases: &'a dyn Releases,
    pub schemas: &'a dyn Schemas,
}
