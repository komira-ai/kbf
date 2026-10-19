//! `kbf-proxy`: the public front door of a frontend host. One TLS terminator on the
//! public port routes every HTTP/2 stream, not every connection, to the active child
//! frontend on loopback, and sends each child the identity of the verified client
//! certificate. The design is `docs/design/frontend-proxy.md`; this crate's API and the
//! decisions behind it are `docs/design/kbf-proxy-api.md`.
//!
//! The modules that hold the decisions are plain values, so each can be tested without a
//! socket:
//!
//! - [`slot`]: the two child slots, the active-slot handle and the per-stream permit.
//! - [`hold`]: the hold gate's bounds and deadline.
//! - [`route`]: which child port, auth class, hold bypass and idle timeout a path has.
//! - [`auth`]: the per-path-class auth decision.
//! - [`identity`]: the verified certificate to node id and role, and the `x-kbf-*` headers.
//! - [`files`]: the secret files, read the way the child reads them and written crash-safely.
//! - [`replay`]: the replay buffer and the retry rule.
//! - [`config`]: the `[proxy]` table of `installer.toml`.
//! - [`control`]: the update-API handler and the status the proxy reports.
//!
//! The I/O is [`Proxy::bind`], [`ProxyRunner::run`] and [`ProxyHandle`]: a TLS terminator
//! (HTTP/2, and HTTP/1.1 for `/v1/*` and the probes) that verifies a client certificate when
//! one is presented, decides who may call each path, tells the child who the caller is,
//! routes each stream to the active child, keeps an upstream pool per (slot, generation,
//! target) and runs the hold gate.

// The I/O modules are not part of the loom build (`--cfg kbf_loom`): the models cover only
// `slot`, and tokio and hyper are not loom-aware.
pub mod auth;
#[cfg(not(kbf_loom))]
mod body;
pub mod config;
pub mod control;
#[cfg(not(kbf_loom))]
mod core;
pub mod files;
#[cfg(not(kbf_loom))]
mod h1;
pub mod hold;
pub mod identity;
#[cfg(not(kbf_loom))]
mod keepalive;
#[cfg(not(kbf_loom))]
mod limits;
#[cfg(not(kbf_loom))]
mod metrics;
#[cfg(not(kbf_loom))]
mod pool;
#[cfg(not(kbf_loom))]
mod proxy;
pub mod replay;
#[cfg(not(kbf_loom))]
mod reply;
pub mod route;
#[cfg(test)]
mod scratch;
#[cfg(not(kbf_loom))]
mod security;
#[cfg(not(kbf_loom))]
mod serve;
pub mod slot;
#[cfg(not(kbf_loom))]
mod stream;
mod sync;
#[cfg(not(kbf_loom))]
mod tls;

#[cfg(not(kbf_loom))]
pub use proxy::{Proxy, ProxyHandle, ProxyRunner};
#[cfg(not(kbf_loom))]
pub use tls::TlsReloadError;

#[cfg(all(test, not(kbf_loom)))]
mod tests;
