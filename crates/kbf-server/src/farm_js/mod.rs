//! What the farm server supplies to `js-store`, the scheduler's rows in the state store, so
//! that the rows it writes through that crate are the rows this server's own store layer
//! ([`crate::farm`]'s `durable` module) writes, byte for byte.
//!
//! - [`codec`]: the values of the `op/`, `lease/` and `session/` rows, in the `kbf-log`
//!   encoding the store layer already writes. A lease row carries the cost admission charged
//!   the lease when it has one (a field the log's lease record gained, which an older binary
//!   skips); a row written without it reads as a lease of no recorded cost.
//! - [`ext`]: the rows only a build server has: `subject/` (what the `action usage:` line
//!   says an operation is) and `doneof/` (the name and action of a finished operation).
//! - [`load`]: the load of the store, with the checks `js-store` leaves to its embedder and
//!   this server's reader makes.
//!
//! Nothing here is wired into the running server: the farm writes its rows through its own
//! store layer. The tests run the scenario of the store layer's golden files through
//! `js-store` and require the same rows.

pub mod codec;
pub mod ext;
pub mod load;

#[cfg(test)]
mod codec_tests;
#[cfg(test)]
mod ext_tests;
#[cfg(test)]
mod load_tests;
#[cfg(test)]
mod testing;
