//! The object-store control API (`/v1/object-stores`): the routes through their gate, the
//! registry under them, the answers. Every test names the defect it catches.
//!
//! One binary on purpose: the coverage ratchet counts the best single build of each function,
//! so the routes' tests live together.

mod audit_unknown;
mod bindings;
mod client;
mod connections;
mod gate;
mod harness;
mod leaks;
mod repoint;
mod status;
mod verify;
