//! The ports of a farm, claimed so that no other farm of the host can have them.
//!
//! A farm's processes bind their ports themselves, so the harness cannot hold the sockets
//! until they do: a port found free by binding `127.0.0.1:0` and closing the socket is free
//! only until another program takes it (a test binding port 0, a connection's source port),
//! and then a process dies with "address in use" or two roles share one. Instead a farm
//! claims a block of [`BLOCK`] consecutive ports below the range the kernel hands out to
//! connections, by holding an exclusive lock on a file named for the block, and takes
//! every port of the farm from it. The lock is held as long as the [`Claim`] lives, and the
//! kernel drops it when the holder dies (a harness killed with SIGKILL leaves nothing
//! stale). Two farms of one process, of two processes, or of two users sharing a temporary
//! directory never hold one block; a block with any port in use now (by a program outside
//! the harness) is skipped.
//!
//! What it does not cover: a program that binds a port inside a block that no farm holds,
//! between the probe and the farm's process binding it. The blocks are below the kernel's
//! own ephemeral range, where nothing binds without asking for that port.

use std::fs::{File, OpenOptions};
use std::net::TcpListener;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use rustix::fs::{FlockOperation, flock};

use crate::envoy::BoxPorts;

/// Ports in a block.
pub const BLOCK: u16 = 64;

/// The first port of the first block.
const FIRST: u16 = 20_000;

/// Where the kernel's ephemeral range starts when `/proc` does not say.
const EPHEMERAL: u16 = 32_768;

/// Offsets in a block. The frontend's three listeners are the box's instance `a` (Envoy's
/// config also names instance `b`, ten above), so they sit at the start.
const FRONT: u16 = 0;
const ENVOY: u16 = 16;
const SPARE: u16 = 19;
const STORAGE: u16 = 20;
const METRICS: u16 = 28;

/// The most storage nodes a block holds ports for.
pub const MAX_NODES: usize = 8;

/// A block of ports, held until it is dropped.
#[derive(Debug)]
pub struct Claim {
    index: u32,
    base: u16,
    _lock: File,
}

/// Where the kernel's ephemeral range starts, from the text of
/// `/proc/sys/net/ipv4/ip_local_port_range` (`32768\t60999`); the default for text that
/// does not say.
#[must_use]
pub fn ephemeral_floor(text: &str) -> u16 {
    text.split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .unwrap_or(EPHEMERAL)
}

/// How many blocks there are below the ephemeral range that starts at `floor`.
fn blocks(floor: u16) -> u32 {
    u32::from(floor.min(EPHEMERAL).saturating_sub(FIRST) / BLOCK)
}

/// The end of a search that found no block.
fn exhausted(count: u32) -> ! {
    panic!("all {count} blocks of ports are held or busy")
}

/// The first `Some` that `try_block` gives for block `start % count`, then the blocks after
/// it, round the `count` blocks once; `None` when every one gives `None`.
fn search<T>(count: u32, start: u32, try_block: impl Fn(u32) -> Option<T>) -> Option<T> {
    (0..count).find_map(|i| try_block((start % count + i) % count))
}

fn lock_dir() -> PathBuf {
    std::env::temp_dir().join("kbf-harness-ports")
}

impl Claim {
    /// A block no other farm holds, searching from a place of this process's own (its id)
    /// so that processes starting together do not all look at the same blocks first.
    ///
    /// # Panics
    /// When every block is held or busy: a host running that many farms has none to spare.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(std::process::id())
    }

    /// [`Claim::new`], searching from block `start` (modulo the number of blocks).
    ///
    /// # Panics
    /// As [`Claim::new`].
    #[must_use]
    pub fn starting_at(start: u32) -> Self {
        let floor = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .map_or(EPHEMERAL, |text| ephemeral_floor(&text));
        let count = blocks(floor);
        assert!(count > 0, "no room for a block of ports below {floor}");
        let _ = std::fs::create_dir_all(lock_dir());
        search(count, start, Self::try_block).unwrap_or_else(|| exhausted(count))
    }

    /// Block `index` if nobody holds it and every port of it can be bound now.
    fn try_block(index: u32) -> Option<Self> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o666)
            .open(lock_dir().join(format!("block-{index}.lock")))
            .ok()?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).ok()?;
        let base = FIRST + u16::try_from(index).ok()? * BLOCK;
        // Any listener on a port of the block, on any loopback address, makes the bind on
        // the wildcard address fail.
        (base..base + BLOCK)
            .all(|port| TcpListener::bind(("0.0.0.0", port)).is_ok())
            .then_some(Self {
                index,
                base,
                _lock: lock,
            })
    }

    /// The block's first port.
    #[must_use]
    pub fn base(&self) -> u16 {
        self.base
    }

    /// The block's number: distinct among the farms that hold blocks together, so a value
    /// derived from it (Envoy's base id) is too.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// The port `offset` into the block.
    ///
    /// # Panics
    /// When `offset` is past the block.
    #[must_use]
    pub fn port(&self, offset: u16) -> u16 {
        assert!(offset < BLOCK, "offset {offset} is past the block");
        self.base + offset
    }
}

impl Default for Claim {
    fn default() -> Self {
        Self::new()
    }
}

/// Every port a farm of `storage_nodes` nodes is told, taken from one [`Claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ports {
    /// The frontend's REAPI, worker and operator API listeners.
    pub front: (u16, u16, u16),
    /// Each storage node's storage listener, node 1 first.
    pub storage: Vec<u16>,
    /// A listener no one dials, for the roles that must be told one.
    pub spare: u16,
    /// Each storage node's `/metrics`, node 1 first.
    pub metrics: Vec<u16>,
    /// Envoy's three, when it runs: `base` is the frontend's own first port.
    pub envoy: Option<BoxPorts>,
}

impl Ports {
    /// The ports of a farm of `storage_nodes` nodes (with Envoy's, if `envoy`) in `claim`.
    ///
    /// # Panics
    /// When `storage_nodes` is past [`MAX_NODES`].
    #[must_use]
    pub fn of(claim: &Claim, storage_nodes: usize, envoy: bool) -> Self {
        assert!(
            storage_nodes <= MAX_NODES,
            "{storage_nodes} storage nodes: a block holds ports for {MAX_NODES}"
        );
        let nth = |start: u16| {
            (0..storage_nodes)
                .map(|i| claim.port(start + u16::try_from(i).expect("at most 8")))
                .collect()
        };
        Self {
            front: (
                claim.port(FRONT),
                claim.port(FRONT + 1),
                claim.port(FRONT + 2),
            ),
            storage: nth(STORAGE),
            spare: claim.port(SPARE),
            metrics: nth(METRICS),
            envoy: envoy.then(|| BoxPorts {
                base: claim.base(),
                reapi: claim.port(ENVOY),
                worker: claim.port(ENVOY + 1),
                admin: claim.port(ENVOY + 2),
            }),
        }
    }

    /// Every port, for a check that none is told twice.
    #[must_use]
    pub fn all(&self) -> Vec<u16> {
        let mut all = vec![self.front.0, self.front.1, self.front.2, self.spare];
        all.extend(&self.storage);
        all.extend(&self.metrics);
        if let Some(b) = &self.envoy {
            all.extend([b.reapi, b.worker, b.admin]);
        }
        all
    }
}

#[cfg(test)]
#[path = "ports_tests.rs"]
mod tests;
