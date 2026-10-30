//! The router against fake storage nodes, a settable map and, for TLS, real servers.

mod support;

mod anywhere;
mod catch_up;
mod closure;
mod durable;
mod finding;
mod flights;
mod holding;
mod placement;
mod reading;
mod repair;
mod sinking;
mod stale;
mod streaming;
mod tls;
mod verifying;
mod writing;
