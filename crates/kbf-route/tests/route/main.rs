//! The router against fake storage nodes, a settable map and, for TLS, real servers.

mod support;

mod finding;
mod flights;
mod placement;
mod reading;
mod repair;
mod sinking;
mod stale;
mod tls;
mod verifying;
mod writing;
