//! The router against fake storage nodes, a settable map and, for TLS, real servers.

mod support;

mod anywhere;
mod catch_up;
mod closure;
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
