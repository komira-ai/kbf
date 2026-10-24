//! `kbf-coordinator`: tells each host's updater which version to run, within per-role
//! limits (`docs/design/updates.md`).
//!
//! - [`identity`]: who is calling, from the client certificate's subjectAltNames alone;
//! - [`desired`]: the one thing stored, the desired state, in a file replaced atomically;
//! - [`fleet`]: the last heartbeat of every host, the live and down rule, the quiet start;
//! - [`converge`]: whether the farm has reached the desired version, for CI to wait on;
//! - [`decide`]: the decision as a pure function of the desired state, the fleet, the host
//!   and the time;
//! - [`service`]: the coordinator itself: heartbeats in, the desired state set by CI's OIDC
//!   token, the status read by the operator token;
//! - [`http`] and [`listen`]: the two HTTP surfaces and the TLS listeners under them;
//! - [`config`]: the flags and the start.
//!
//! The wire types of the heartbeat and the reply are `kbf_updater::wire`.

pub mod config;
pub mod converge;
pub mod decide;
pub mod desired;
pub mod fleet;
pub mod http;
pub mod identity;
pub mod listen;
pub mod service;

#[cfg(test)]
mod converge_tests;
#[cfg(test)]
mod decide_tests;
#[cfg(test)]
mod desired_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod testutil;
