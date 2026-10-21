//! The transfer engine against servers in this process: kbf-front's real CAS services,
//! and a fake (`engine/fake.rs`) that counts what it is asked, behind a proxy
//! (`engine/proxy.rs`) that counts connections and can cut them. All the engine's
//! tests are one binary, so its async code has one instance to measure.

#[path = "engine/deserted.rs"]
mod deserted;
#[path = "engine/fake.rs"]
mod fake;
#[path = "engine/front.rs"]
mod front;
#[path = "engine/limits.rs"]
mod limits;
#[path = "engine/memory.rs"]
mod memory;
#[path = "engine/proxy.rs"]
mod proxy;
#[path = "engine/reads.rs"]
mod reads;
#[path = "engine/segments.rs"]
mod segments;
#[path = "engine/spool.rs"]
mod spool;
#[path = "engine/uploads.rs"]
mod uploads;

use std::net::SocketAddr;
use std::sync::Arc;

use kbf_transfer::{TransferConfig, Transfers};
use tonic::transport::Endpoint;

/// An engine with `config` over one connection to `addr`, made on first use.
pub fn engine(addr: SocketAddr, config: TransferConfig) -> Arc<Transfers> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    Arc::new(Transfers::new(endpoint, config))
}

/// `len` patterned bytes, different for each `seed`.
pub fn patterned(seed: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed * 7) % 251) as u8)
        .collect()
}
