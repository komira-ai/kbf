//! The OCI read front over real TLS on loopback: a client that trusts a test CA reads manifests
//! and blobs by IP address, the limits of the listener hold, and the binary's flags start it.

mod binary;
mod client;
mod front;
mod limits;
mod pki;
mod rig;
