//! What `setup` does through the rest of the installer: the state database. It is a trait with
//! one real implementation here, so the tests of `run` play the server and the implementation is
//! tested on its own.

use std::path::Path;

use super::SetupError;
use crate::database::{Database, SchemaOps};

/// Where the state database is reached from.
pub trait Schemas {
    /// The schema runner for the database `url` names (validated: no password), with the
    /// password of `password_file` (`--database-password-file`), else PGPASSWORD, else
    /// `~/.pgpass`, as setup's own process has them.
    ///
    /// # Errors
    /// The URL is not one the Postgres client takes, or the password cannot be had.
    fn of(&self, url: &str, password_file: Option<&str>) -> Result<Box<dyn SchemaOps>, SetupError>;
}

/// The real server, with the migrations embedded in this build, reached with the password
/// sources of `context`.
pub struct Postgres {
    pub context: kbf_pg_auth::Context,
}

impl Postgres {
    /// The real server, with this process's PGPASSWORD and libpq file.
    pub fn of_process() -> Postgres {
        Postgres {
            context: kbf_pg_auth::Context::of_process(),
        }
    }
}

impl Schemas for Postgres {
    fn of(&self, url: &str, password_file: Option<&str>) -> Result<Box<dyn SchemaOps>, SetupError> {
        let file = password_file.map(Path::new);
        let found = kbf_pg_auth::authenticate(url, file, &self.context).map_err(refusal)?;
        if let Some(skipped) = &found.skipped {
            eprintln!("setup: warning: {skipped}");
        }
        Ok(Box::new(Database::new(
            found.config,
            kbf_pack_index::schema::embedded(),
        )))
    }
}

/// What setup says when the database cannot be reached from its flags. The client's own messages
/// may quote the URL; none is kept.
fn refusal(why: kbf_pg_auth::AuthError) -> SetupError {
    SetupError::Database(match why {
        kbf_pg_auth::AuthError::Url(_) => "the --database URL is not usable".to_owned(),
        kbf_pg_auth::AuthError::InUrl => format!(
            "--database carries a password: {}",
            kbf_pg_auth::ways(Some("--database-password-file"))
        ),
        why @ kbf_pg_auth::AuthError::File { .. } => format!("--database-password-file: {why}"),
        why => format!("the password of --database: {why}"),
    })
}

/// What `run` takes from outside the host.
pub struct Backend<'a> {
    pub schemas: &'a dyn Schemas,
}
