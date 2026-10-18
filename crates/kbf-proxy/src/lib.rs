//! `kbf-proxy`: the public front door of a frontend host. One TLS terminator on the
//! public port routes every HTTP/2 stream, not every connection, to the active child
//! frontend on loopback, and sends each child the identity of the verified client
//! certificate. The design is `docs/design/frontend-proxy.md`; this crate's API and the
//! decisions behind it are `docs/design/kbf-proxy-api.md`.
//!
//! The modules that exist so far hold the decisions that are plain values, so each can be
//! tested without a socket:
//!
//! - [`slot`]: the two child slots, the active-slot handle and the per-stream permit.
//! - [`hold`]: the hold gate's bounds and deadline.
//! - [`route`]: which child port, auth class, hold bypass and idle timeout a path has.
//! - [`auth`]: the per-path-class auth decision.
//! - [`identity`]: the verified certificate to node id and role, and the `x-kbf-*` headers.
//! - [`replay`]: the replay buffer and the retry rule.
//! - [`config`]: the `[proxy]` table of `installer.toml`.
//! - [`control`]: the update-API handler and the status the proxy reports.

pub mod auth;
pub mod config;
pub mod control;
pub mod hold;
pub mod identity;
pub mod replay;
pub mod route;
pub mod slot;
