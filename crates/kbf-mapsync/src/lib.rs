//! The holder of the storage map a node keeps in motion (`docs/design/architecture-v2.md`,
//! section 6).
//!
//! Every process knows one name, the frontend's. The frontend holds the map and pushes it;
//! this crate is the receiving end, with nothing about the wire or the network in it:
//!
//! - [`MapHolder`]: what a storage node keeps the newest pushed map in. It
//!   never goes back: a map of an older or the same generation is refused, so a push that
//!   arrives late, twice or out of order changes nothing.
//!
//! # Lost pushes
//!
//! A push is not relied on. A node that finds it is behind (a storage node answered
//! `STALE_MAP` naming a newer generation) calls [`MapHolder::reach`], which records that
//! generation as wanted ([`MapHolder::asks`]) and waits for a map that new. A
//! process that is ahead of a node, and a node that was never told, are the node's to
//! wait out the same way.

mod holder;

pub use holder::{MapHolder, RETAINED_MAPS};
