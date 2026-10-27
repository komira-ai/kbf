//! The VM driver against the fake host, which runs the real `kbf-guest` agent on a Unix
//! socket. They are unit tests (not a `tests/` directory) so that the driver's generic
//! code is instantiated in this crate, where its coverage is measured as one.

mod edges;
mod lifecycle;
mod support;
