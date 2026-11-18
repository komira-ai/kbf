//! The state schema at a frontend's start: a frontend on a Postgres `--state-store` brings the
//! database to the schema this build needs before it serves.
//!
//! A frontend that runs as a container (Docker Swarm) has no unit whose `ExecStartPre=` runs
//! `kbf boot-check --mode=frontend`, so a release that raises the schema would start on the old
//! one and fail every operation that needs the new tables. The start does the step itself:
//! after the state store is open (so after `--state-store-wait-secs` has waited for a server
//! that is not up yet), and before the pack index is opened, the routes are served and the
//! start line is printed, it runs [`kbf_state::postgres::boot::BootStep::migrate_to`] with
//! [`kbf_pack_index::schema::latest`] over the frontend's own `--state-store` URL: the step
//! `kbf boot-check` runs, expand migrations only, under the step's lock, waiting at most
//! [`kbf_state::postgres::boot::LOCK_WAIT`] for any lock. A database already at that schema (a
//! unit that ran `kbf boot-check` first leaves one) is left alone. The listening sockets are
//! bound by then, as they are during the wait for the store, but nothing accepts on them.
//!
//! A start-first rolling update runs the new frontend's step while the old frontend serves.
//! That is safe because a step only adds (new tables, columns with defaults): the old release
//! keeps working on a schema one step ahead (`docs/design/kbf-cicd.md`, the N and N-1 section).
//!
//! A step that fails (the database role may not create tables, a pending migration is a
//! `contract` step, the lock is busy past the wait, the database cannot be reached) refuses the
//! start with [`SchemaRefused`], which names the cause and `kbf boot-check --mode=frontend`.
//! The step reports a line of text, not a type, so the failure is not told apart by what a
//! retry would do: the start exits 2, as every refusal does. A SQLite store has no step.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use crate::{StateStore, StateStoreUri};

/// The start was refused because the state schema could not be brought to this build's.
#[derive(Debug)]
pub struct SchemaRefused(pub String);

impl fmt::Display for SchemaRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the state schema cannot be brought to the one this build needs: {}; \
             `kbf boot-check --mode=frontend` runs the same step and shows it",
            self.0
        )
    }
}

impl Error for SchemaRefused {}

/// Opens the state store `uri` names ([`super::state_wait::open`]) and then, for a Postgres
/// one, brings its schema to this build's.
pub(super) fn open_migrated(
    uri: &StateStoreUri,
    window: Duration,
) -> Result<StateStore, Box<dyn Error>> {
    let store = super::state_wait::open(uri, window)?;
    if let Some(step) = uri.schema_step() {
        migrate_with(&|wanted| step.migrate_to(wanted))?;
    }
    Ok(store)
}

/// Runs `step` for the schema this build needs and logs what it did; the text it fails with
/// is the refusal.
pub(super) fn migrate_with(
    step: &dyn Fn(u32) -> Result<String, String>,
) -> Result<(), SchemaRefused> {
    let said = step(kbf_pack_index::schema::latest()).map_err(SchemaRefused)?;
    tracing::info!("state schema: {said}");
    Ok(())
}
