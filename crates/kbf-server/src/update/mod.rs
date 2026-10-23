//! The durable rows of the desired release (`docs/design/pull-updates.md` section 6.2): the
//! run ([`Desired`]), the policy whose pause holds it ([`Policy`]), the release poller's
//! proposals ([`Proposal`]) and the rows themselves ([`Rows`]). Nothing picks hosts or steps
//! them: updates are OS packages, driven by `kbf-updater`. The rows older builds kept for
//! hosts and places are only read, by [`crate::stored_update`].

pub mod age;
mod codec;
mod policy;
mod proposal;
mod rows;
mod run;

pub use codec::Desired;
pub use policy::Policy;
pub use proposal::{PROPOSED, Proposal, proposal_key};
pub use rows::{RowError, Rows, Stored};
pub use run::{Generation, Release, Source};
