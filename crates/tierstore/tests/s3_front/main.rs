//! The S3 front of the tiering proxy and its binary: the client of kbf against the server over
//! loopback, requests signed by hand, the drain, and the `tierstore` process.

mod activated;
mod binary;
mod cache_node;
mod client;
mod config_file;
mod drain;
mod files;
mod idle;
mod raw;
mod roles;
mod run;
mod sdk;
mod signer;
mod stalled;
mod support;
