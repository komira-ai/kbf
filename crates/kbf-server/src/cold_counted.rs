//! A storage half's cold store, counted: [`CountedPick`] wraps what [`ColdPick`] chooses
//! in [`Counted`], so every request the node's cache makes to the cold store, on the
//! store its flags name or the one its registration gives, is in `kbf_cold_store_*`.

use std::error::Error;
use std::sync::Arc;

use kbf_front::store_metrics::{Counted, StoreMetrics};

use crate::cold_apply::{Applied, ColdPick};
use crate::join::{Joined, Joiner};

/// `C`, whose store is counted in the metrics.
pub(crate) struct CountedPick<C>(pub C, pub Arc<StoreMetrics>);

impl<C: ColdPick> ColdPick for CountedPick<C> {
    type Store = Counted<C::Store>;

    fn alone(self) -> Result<Self::Store, Box<dyn Error>> {
        Ok(Counted::new(self.0.alone()?, self.1))
    }

    async fn registered(
        self,
        joiner: &Joiner,
        joined: &Joined,
    ) -> Result<(Self::Store, Applied), Box<dyn Error>> {
        let (store, applied) = self.0.registered(joiner, joined).await?;
        Ok((Counted::new(store, self.1), applied))
    }
}

#[cfg(test)]
#[path = "cold_counted_tests.rs"]
mod tests;
