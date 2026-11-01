//! `kbf-coordinator`: tells each host's updater which version to run, within per-role
//! limits (`docs/design/updates.md`).
//!
//! - [`identity`]: who is calling, from the client certificate's subjectAltNames alone;
//! - [`desired`]: the one thing stored, the desired state, in a file replaced atomically;
//! - [`fleet`]: the last heartbeat of every host, the live and down rule, the quiet start;
//! - [`bake`] and [`canary`]: how long an updated host holds its slot, and the hosts that go
//!   alone first;
//! - [`converge`]: whether the farm has reached the desired version, for CI to wait on;
//! - [`decide`]: the decision as a pure function of the desired state, the fleet, the host
//!   and the time;
//! - [`services`]: services as data: roles, limits and the order between services;
//! - [`service`]: the coordinator itself: heartbeats in, the desired state set by CI's OIDC
//!   token, the status read by the operator token;
//! - [`http`] and [`listen`]: the two HTTP surfaces and the TLS listeners under them;
//! - [`config`]: the flags and the start.
//!
//! The wire types of the heartbeat and the reply are `kbf_updater::wire`.

/// The commit this coordinator was built from: the 12 digits `--version` shows after the
/// `+`, or `unknown` where the build had no git checkout. `GET /v1/status` and
/// `GET /v1/convergence` say it as `commit`.
pub const BUILD_COMMIT: &str = env!("KBF_BUILD_COMMIT");

pub mod bake;
pub mod canary;
pub mod config;
pub mod converge;
pub mod decide;
pub mod defer;
pub mod desired;
pub mod fleet;
pub mod http;
pub mod identity;
pub mod listen;
pub mod service;
pub mod services;

#[cfg(test)]
mod bake_canary_tests;
#[cfg(test)]
mod bake_review_tests;
#[cfg(test)]
mod converge_role_tests;
#[cfg(test)]
mod converge_tests;
#[cfg(test)]
mod decide_golden_tests;
#[cfg(test)]
mod decide_role_tests;
#[cfg(test)]
mod decide_tests;
#[cfg(test)]
mod decide_why_tests;
#[cfg(test)]
mod defer_tests;
#[cfg(test)]
mod desired_dropped_tests;
#[cfg(test)]
mod desired_role_tests;
#[cfg(test)]
mod desired_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod kbf_definition_tests;
#[cfg(test)]
mod kbf_review_tests;
#[cfg(test)]
mod priority_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod services_tests;
#[cfg(test)]
mod status_service_tests;
#[cfg(test)]
mod testutil;
