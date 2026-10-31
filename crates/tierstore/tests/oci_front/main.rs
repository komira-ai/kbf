//! The OCI read front over real TLS on loopback (a client that trusts a test CA reads manifests
//! and blobs by IP address) and over plain HTTP on loopback, the limits of the listener hold in
//! both, and the binary's flags start it.

mod binary;
mod cache_node;
mod client;
mod front;
mod limits;
mod pki;
mod plain;
mod rig;
