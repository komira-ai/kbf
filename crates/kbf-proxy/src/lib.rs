//! `kbf-proxy`: the public front door of a frontend host. One TLS terminator on the public
//! port routes every HTTP/2 stream, not every connection, to a frontend of its pool on
//! loopback, and sends each frontend the identity of the verified client certificate. The
//! design is `docs/design/frontend-proxy.md`; this crate's API and the decisions behind it are
//! `docs/design/kbf-proxy-api.md`.
//!
//! The modules that hold the decisions are plain values, so each can be tested without a
//! socket:
//!
//! - [`route`]: which member port, auth class and idle timeout a path has.
//! - [`auth`]: the per-path-class auth decision.
//! - [`identity`]: the verified certificate to node id and role, and the `x-kbf-*` headers.
//! - [`files`]: the secret files, read the way the child reads them and written crash-safely.
//! - [`replay`]: the replay buffer and the retry rule.
//! - [`config`]: the `[proxy]` table of `installer.toml`.
//! - [`control`]: the update-API handler.
//!
//! The I/O is [`Proxy::bind`], [`ProxyRunner::run`] and [`ProxyHandle`]: a TLS terminator
//! (HTTP/2, and HTTP/1.1 for `/v1/*` and the probes) that verifies a client certificate when
//! one is presented, decides who may call each path, tells the child who the caller is, places
//! each stream on a member of the pool and keeps an upstream pool per (member, target).
//!
//! **The pool.** The proxy is a plain load balancer over frontends that run on their own, the
//! `[[frontend]]` tables of its config: no child is started and nothing is swapped.
//! `upstreams` holds the members (up or draining, ready or not, streams open) and picks the
//! least busy ready one for each stream; [`config::Frontend`] is a member's table. A poll of
//! each member's operator listener sets its readiness (`pool_probe`), and `/readyz` is the
//! proxy's own answer: 200 while some member is up and ready. An admin API on `admin_listen`
//! lists the members and drains or undrains one (`pool_admin`).

pub mod auth;
mod body;
pub mod config;
pub mod control;
mod core;
pub mod files;
mod h1;
pub mod identity;
mod keepalive;
mod limits;
mod metrics;
mod pool;
mod pool_admin;
mod pool_probe;
mod probe;
mod proxy;
pub mod replay;
mod reply;
pub mod route;
#[cfg(test)]
mod scratch;
mod security;
mod serve;
mod stream;
mod tls;
mod upstreams;

pub use proxy::{Proxy, ProxyHandle, ProxyRunner};
pub use tls::TlsReloadError;

#[cfg(test)]
mod tests;
