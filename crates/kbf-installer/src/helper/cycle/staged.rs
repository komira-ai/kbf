//! Orders whose source is `staged`: the release is not fetched, it is the one the host's
//! package carries (see [`crate::seed`]). An order of that kind is a locator and nothing else;
//! the fetcher reads the package's manifest ([`Fetcher::seed`]) and the cycle holds the order
//! to two rules before anything is fetched, made or told:
//!
//! - it names the seed's commit, all 40 digits;
//! - the seed's `seq` is strictly above the floor in `seq_floor`, and a host with no floor
//!   admits nothing.
//!
//! Both refuse with the word `commit_or_ref`, say which rule, and leave the host as it was; the
//! order is acted on, so the floor of orders moves. The rules are asked after a node
//! activation's shortcut for the release that already runs (a repeated order is a no-op, not a
//! refusal that would hold the run), and the fetcher holds the release to the manifest's facts
//! and commit again, whoever asked.

use kbf_host::Exec;
use kbf_update::Source;

use super::Helper;
use crate::CommitId;
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::order::Claims;
use crate::seed::{self, SeedFacts};

/// What the release an activation fetches is held to.
pub(super) enum Held<'a> {
    /// The components and facts the order claims: it fetches, and the release must declare
    /// them.
    Claims(&'a Claims),
    /// The facts the package's manifest declares: a staged order claims none.
    Seed(SeedFacts),
}

/// The refusal of order `id` whose fetch said `e`. A refusal of the order itself leads with its
/// own word (`denied`, `commit_or_ref`), and the others are said as they always were.
pub(super) fn refusal_of(id: u64, e: FetchError) -> String {
    match e {
        FetchError::Order { word, why } => format!("{word}: order {id}: {why}"),
        other => format!("order {id}: {other}"),
    }
}

impl<E: Exec> Helper<E> {
    /// The seed this host's package carries, if an order for `commit` may use it: the
    /// manifest read through the fetcher, the order naming the seed's commit and the seed's
    /// `seq` above the floor ([`seed::admit`]).
    pub(super) fn admitted_seed(&self, commit: &CommitId) -> Result<SeedFacts, FetchError> {
        let seed = self.fetcher.seed()?;
        let floor = seed::read_floor(&self.host)?;
        seed::admit(&seed, commit, floor)?;
        Ok(seed)
    }

    /// The release an activation names, fetched and verified by the fetcher: from the source
    /// the host is configured with, by the order's own components, held to the facts it
    /// claims; or the package's, held to the facts its manifest declares.
    pub(super) fn fetch_held(
        &self,
        commit: &CommitId,
        held: &Held<'_>,
    ) -> Result<InstalledRelease, FetchError> {
        let (components, source, facts) = match held {
            Held::Claims(claims) => (claims.components.clone(), Source::Fetch, claims.facts()),
            Held::Seed(seed) => (Vec::new(), Source::Staged, seed.facts.clone()),
        };
        let rec = ReleaseLocator {
            commit: commit.clone(),
            components,
            source,
        };
        self.fetcher.fetch_activation(&rec, &facts)
    }
}
