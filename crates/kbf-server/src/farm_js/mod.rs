//! What the farm server supplies to `js-store`, the scheduler's rows in the state store, and
//! to `js-server-core`, the loop around the scheduler, so that the farm can run on them with
//! the same effect: the same rows, the same messages to the nodes and the same stages for
//! the callers as its own driver ([`crate::farm`]) gives.
//!
//! - [`codec`]: the values of the `op/`, `lease/` and `session/` rows, in the `kbf-log`
//!   encoding the store layer already writes. A lease row carries the cost admission charged
//!   the lease when it has one (a field the log's lease record gained, which an older binary
//!   skips); a row written without it reads as a lease of no recorded cost.
//! - [`ext`]: the rows only a build server has: `subject/` (what the `action usage:` line
//!   says an operation is) and `doneof/` (the name and action of a finished operation).
//! - [`load`]: the load of the store, with the checks `js-store` leaves to its embedder and
//!   this server's reader makes.
//! - [`adapter`]: [`KbfAdapter`], the core's `Adapter`; [`host`]: what a host does around
//!   the core's calls with it; [`record`]: the events it tells the invocation recorder;
//!   [`open`]: the core's configuration, storage, metrics and admission.
//!
//! Nothing here is wired into the running server: the farm writes its rows through its own
//! store layer and drives its own scheduler. The tests run the scenario of the store layer's
//! golden files through `js-store` and require the same rows, and the scripted sessions of
//! `tests/farm_core_trace` through the core and require the same text.

pub mod adapter;
pub mod codec;
pub mod dispatch;
pub mod ext;
mod facts;
pub mod host;
pub mod load;
pub mod open;
mod record;

pub use adapter::{KbfAdapter, Outbound, Wall};
pub use host::{Caller, Connection};
pub use open::KbfObserver;

#[cfg(test)]
mod adapter_tests;
#[cfg(test)]
mod codec_tests;
#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod ext_tests;
#[cfg(test)]
mod facts_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod load_tests;
#[cfg(test)]
mod testing;
