//! What `setup` does through the rest of the installer: the first release and the state
//! database. Each is a trait with one real implementation here, so the tests of `run` play the
//! host and the server and the implementations are tested on their own.

use std::cell::RefCell;

use kbf_host::Exec;
use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema;

use super::SetupError;
use crate::CommitId;
use crate::activate;
use crate::database::{Database, SchemaOps};
use crate::helper::fetcher::{Fetcher, ReleaseLocator};
use crate::layout::Layout;

/// The installer's one downloader, as `setup` uses it: fetch the release for `commit` from the
/// source `installer.toml` names (setup has written it by then), verify it as an update is
/// verified, install it as `releases/<commit>` and make it the release that runs.
pub trait Releases {
    /// # Errors
    /// Why the release could not be fetched, verified or made current.
    fn install_first(&self, commit: &CommitId) -> Result<(), SetupError>;
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

impl<E: Exec> Releases for Fetching<'_, E> {
    fn install_first(&self, commit: &CommitId) -> Result<(), SetupError> {
        let installed = self.fetcher.fetch(&ReleaseLocator {
            commit: commit.clone(),
            components: Vec::new(),
        })?;
        if installed.commit != *commit {
            return Err(SetupError::Fetch(
                crate::helper::fetcher::FetchError::Rejected(format!(
                    "asked for {commit}, got {}",
                    installed.commit
                )),
            ));
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
