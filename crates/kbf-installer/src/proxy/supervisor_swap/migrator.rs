//! Placeholder: lets every release through.

use std::sync::Arc;

use super::super::model::PortError;
use super::super::ports::{Migrator, RootOrders};
use crate::CommitId;

pub type RunningSchema = Box<dyn Fn() -> Result<u32, String> + Send + Sync>;

pub struct ExpandOnlyDefault;

impl ExpandOnlyDefault {
    #[must_use]
    pub fn new(
        _orders: Arc<dyn RootOrders + Send + Sync>,
        _embedded_schema: u32,
        _running_schema: RunningSchema,
    ) -> ExpandOnlyDefault {
        ExpandOnlyDefault
    }
}

impl Migrator for ExpandOnlyDefault {
    fn migrate_forward(&self, _release: &CommitId) -> Result<(), PortError> {
        Ok(())
    }
}
