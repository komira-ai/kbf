//! A [`Migrator`] that migrates nothing and says so: it lets an update through only when no
//! migration is needed, and refuses every other one with a stable word.
//!
//! The proxy's swap does not use it (the handover driver wires
//! [`crate::migrator::DbMigrator`]); it is the migrator of the API tests over a real
//! supervisor, where a release at the schema the host runs must pass and any other must stop
//! at step `migrate`. It never opens a database.
//!
//! A refusal is `<word>: <message>`, the form of the root half's refusals:
//! - `not_staged`: the staged release is not the one asked for (or `status.json` cannot say).
//! - `installer_too_old`: the release needs a state schema this installer build does not
//!   embed.
//! - `not_additive`: the release needs a state schema other than the one the host runs, or
//!   the running schema cannot be told.

use std::sync::Arc;

use super::super::model::PortError;
use super::super::ports::{Migrator, RootOrders};
use crate::CommitId;

/// The schema the host runs now: `Err` when it cannot be told.
pub type RunningSchema = Box<dyn Fn() -> Result<u32, String> + Send + Sync>;

/// A [`Migrator`] that passes only a release whose `state_schema` is the schema the host
/// already runs.
pub struct ExpandOnlyDefault {
    orders: Arc<dyn RootOrders + Send + Sync>,
    embedded_schema: u32,
    running_schema: RunningSchema,
}

impl ExpandOnlyDefault {
    /// `embedded_schema` is the newest state schema this installer build knows;
    /// `running_schema` reads the one the host runs.
    #[must_use]
    pub fn new(
        orders: Arc<dyn RootOrders + Send + Sync>,
        embedded_schema: u32,
        running_schema: RunningSchema,
    ) -> ExpandOnlyDefault {
        ExpandOnlyDefault {
            orders,
            embedded_schema,
            running_schema,
        }
    }
}

fn refuse(word: &str, why: impl std::fmt::Display) -> PortError {
    PortError(format!("{word}: {why}"))
}

impl Migrator for ExpandOnlyDefault {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        let staged = match self.orders.staged() {
            Ok(Some(facts)) if facts.commit == *release => facts,
            Ok(_) => {
                return Err(refuse(
                    "not_staged",
                    format!("{release} is not the staged release"),
                ));
            }
            Err(e) => return Err(refuse("not_staged", format!("status.json: {e}"))),
        };
        if self.embedded_schema < staged.state_schema {
            return Err(refuse(
                "installer_too_old",
                format!(
                    "the release needs state schema {}, this installer embeds {}",
                    staged.state_schema, self.embedded_schema
                ),
            ));
        }
        let running = (self.running_schema)().map_err(|e| {
            refuse(
                "not_additive",
                format!(
                    "the schema the host runs is unknown ({e}), so a migration cannot be ruled out"
                ),
            )
        })?;
        if staged.state_schema == running {
            Ok(())
        } else {
            Err(refuse(
                "not_additive",
                format!(
                    "the release needs state schema {}, the host runs {running}, and this \
                     installer has no migrator wired in",
                    staged.state_schema
                ),
            ))
        }
    }
}
