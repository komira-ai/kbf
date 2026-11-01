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
//! - [`ablation`]: the `kbf_ablation_*` counters that tell the rows of the placement ablation
//!   apart on `/metrics`.
//! - [`adapter`]: [`KbfAdapter`], the core's `Adapter`; [`host`]: what a host does around
//!   the core's calls with it; the events it tells the invocation recorder are those of
//!   [`crate::run_record::Recording`], which the farm's own side tells too;
//!   [`open`]: the core's configuration, storage, metrics and admission.
//!
//! `--scheduler-core js` runs the farm on these ([`crate::farm`] dispatches on the core, and
//! `crate::farm::js` is the hot path over them): the rows go to the state store through
//! `js-store`, and the scheduler is driven by `js-server-core`. The legacy core still writes
//! its rows through its own store layer and drives its own scheduler. The tests run the
//! scenario of the store layer's golden files through `js-store` and require the same rows,
//! and the scripted sessions of `tests/farm_core_trace` through the core and require the
//! same text.

pub(crate) mod ablation;
pub mod adapter;
pub mod codec;
pub mod dispatch;
pub mod ext;
mod facts;
pub mod host;
mod learning;
pub mod load;
pub mod open;
mod report_hooks;
pub(crate) mod usage;

pub use adapter::{KbfAdapter, Outbound, Wall};
pub use host::{Caller, Connection};
#[cfg_attr(not(any(feature = "farm-core-js", test)), allow(unused_imports))]
pub(crate) use learning::{Gate, write_behind_now, write_learned};
pub use open::KbfObserver;

#[cfg(test)]
mod ablation_tests;
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
mod failed_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod learning_tests;
#[cfg(test)]
mod load_tests;
#[cfg(test)]
mod placement_load_tests;
#[cfg(test)]
mod prediction_tests;
#[cfg(test)]
mod report_hooks_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod usage_tests;
