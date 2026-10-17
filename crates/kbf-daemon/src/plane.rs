//! [`FarmCas`]: the data plane of a daemon that may or may not be told a storage map.
//!
//! With a map (`Welcome.map`, kept in a [`MapHolder`]) every blob goes straight to the
//! storage nodes that own it, through a [`RoutedCas`] over a [`Router`] fed by the held
//! maps ([`HeldMaps`]), each node verified against its own name from the map. The
//! frontend then carries work and results over the session and no blob bytes. Without a
//! map (a server that sends none, or one-box all-in-one) every blob goes through the
//! frontend's CAS, the transfer engine over `--cas`, as before the map existed.
//!
//! The choice is made again for each lease and for each call made outside one, from
//! whether a map is held, and is logged each time it changes: once at the first call,
//! and again if a map arrives later. A lease keeps the side it started on, so its reads
//! and its writes are never split between the two.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use kbf_mapsync::MapHolder;
use kbf_proto::reapi::Digest;
use kbf_route::{Nodes, Router, TlsNodes};
use kbf_transfer::cas::{Cas, CasError, Chunks, Intake};
use kbf_transfer::{Record, RoutedCas, Scope, TransferConfig, Transfers};
use kbf_types::LeaseId;

use crate::config::{ConfigError, TlsFiles};
use crate::map::{HeldMaps, REFETCH_WITHIN};

/// Storage nodes dialled over mutual TLS with the daemon's CA and certificate, each
/// verified against its own host from the map. `files.server_name` (the frontend's
/// name) is not used: [`TlsNodes`] has no way to be given one name for every node.
///
/// # Errors
/// A TLS file cannot be read.
pub fn storage_nodes(files: &TlsFiles) -> Result<Arc<TlsNodes>, ConfigError> {
    let (ca, identity) = files.trust()?;
    Ok(Arc::new(TlsNodes::new(ca, identity)))
}

/// Which side a call went to, last time one was made.
const UNSAID: u8 = 0;
const SERVER: u8 = 1;
const ROUTED: u8 = 2;

/// The daemon's data plane: the storage owners when a map is held, the frontend's CAS
/// when none is.
pub struct FarmCas {
    server: Arc<Transfers>,
    routed: Arc<RoutedCas>,
    maps: Arc<MapHolder>,
    said: AtomicU8,
}

impl FarmCas {
    /// A plane that goes through `server` until `maps` holds a map, and then to the
    /// nodes `nodes` dials, within the limits `config` names.
    #[must_use]
    pub fn new(
        server: Arc<Transfers>,
        maps: Arc<MapHolder>,
        nodes: Arc<dyn Nodes>,
        config: &TransferConfig,
    ) -> Self {
        let held = Arc::new(HeldMaps::new(Arc::clone(&maps), REFETCH_WITHIN));
        let routed = RoutedCas::new(Router::new(held, nodes), config);
        Self {
            server,
            routed: Arc::new(routed),
            maps,
            said: AtomicU8::new(UNSAID),
        }
    }

    /// The side the next call goes to, logged when it differs from the last call's.
    fn side(&self) -> Side<&Arc<Transfers>, &Arc<RoutedCas>> {
        let generation = self.maps.generation();
        let now = if generation > 0 { ROUTED } else { SERVER };
        if self.said.swap(now, Ordering::Relaxed) != now {
            if now == ROUTED {
                tracing::info!(
                    generation,
                    "data plane: blobs go to the storage nodes that own them; \
                     none cross the frontend"
                );
            } else {
                tracing::info!(
                    "data plane: no storage map held; blobs go through the frontend's CAS"
                );
            }
        }
        if now == ROUTED {
            Side::Routed(&self.routed)
        } else {
            Side::Server(&self.server)
        }
    }
}

/// The two sides of the plane. A macro, not a generic function, delegates a call to
/// whichever it is (see [`via`]).
enum Side<S, R> {
    Server(S),
    Routed(R),
}

/// Runs `$call` on the side `$side` names, bound to `$c`.
macro_rules! via {
    ($side:expr, $c:ident => $call:expr) => {
        match $side {
            Side::Server($c) => $call,
            Side::Routed($c) => $call,
        }
    };
}

impl Cas for FarmCas {
    type Lease = FarmLease;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> FarmLease {
        let side = match self.side() {
            Side::Server(server) => Side::Server(server.scope(lease)),
            Side::Routed(routed) => Side::Routed(Arc::clone(routed)),
        };
        FarmLease {
            side,
            farm: Arc::clone(self),
        }
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        via!(self.side(), c => c.get(digest).await)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        via!(self.side(), c => c.put(bytes).await)
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        via!(self.side(), c => c.put_chunks(digest, chunks).await)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        via!(self.side(), c => c.read_to(digest, file).await)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        via!(self.side(), c => c.missing(digests).await)
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        via!(self.side(), c => c.put_records(records).await)
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        via!(self.side(), c => c.intake())
    }

    fn in_flight(&self) -> usize {
        via!(self.side(), c => c.in_flight())
    }
}

/// One lease's view of a [`FarmCas`]: the side the plane was on when the lease began.
pub struct FarmLease {
    side: Side<Scope, Arc<RoutedCas>>,
    farm: Arc<FarmCas>,
}

impl Cas for FarmLease {
    type Lease = Self;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Self {
        self.farm.lease(lease)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        via!(&self.side, c => c.get(digest).await)
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        via!(&self.side, c => c.put(bytes).await)
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        via!(&self.side, c => c.put_chunks(digest, chunks).await)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        via!(&self.side, c => c.read_to(digest, file).await)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        via!(&self.side, c => c.missing(digests).await)
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        via!(&self.side, c => c.put_records(records).await)
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        via!(&self.side, c => c.intake())
    }

    fn in_flight(&self) -> usize {
        via!(&self.side, c => c.in_flight())
    }
}
