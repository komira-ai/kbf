//! `kbf-gateway`: the gateway of a client box.
//!
//! Build clients on the box talk REAPI to a local Envoy. Envoy must send every call to
//! the farm's current leader server, and the leader can move. Servers register with the
//! gateway; the gateway serves the leader to Envoy as xDS:
//!
//! - [`registry`]: the registrations (soft state, expiring) and the election of the
//!   leader among them;
//! - [`cert`] and [`registration`]: the `kbf.gateway.v1.Registration` service, which
//!   admits server certificates only;
//! - [`ads`]: the xDS aggregated discovery service (the one cluster `kbf_leader`);
//! - [`envoy`]: the Envoy bootstrap that points at it;
//! - [`gateway`] and [`cli`]: the process.

pub mod ads;
pub mod cert;
pub mod cli;
pub mod envoy;
pub mod gateway;
pub mod registration;
pub mod registry;

#[cfg(test)]
mod testlog;
