//! Whole-cell simulation: a server, daemons, clients and an object store assembled on
//! the `kbf-sim` kernel and driven by seeds (docs/design/simulation.md, "The cell").
//!
//! This is the one-server skeleton of docs/design/ha.md section 15. The server
//! ([`Server`]) is assembled from the code `kbf-server --meta-log=raft` runs: the
//! metadata log is a [`kbf_server::RaftMetaLog`] (a single-voter `kbf-raft` host over
//! a `kbf-store` store, its entries `kbf-log` commands applied to a `MetaState`) on a
//! fault-injecting in-memory disk ([`kbf_store::FaultFs`]); the farm core is a
//! `kbf-sched` [`kbf_sched::Scheduler`] and a [`kbf_server::machine::FarmMachine`],
//! fed as `kbf-server`'s farm core feeds them. Daemons ([`FakeDaemon`]) keep their
//! contact with the server in the daemon's own [`kbf_daemon::Contact`] and drop the
//! leases of an earlier epoch by [`kbf_daemon::superseded`]. Clients ([`FakeClient`])
//! upload an input, check it with `FindMissingBlobs` and `Execute` an action. The
//! object store ([`ObjectStore`]) keeps every object it is given.
//!
//! [`run`] runs a [`Scenario`] step by step and checks properties 1 to 7 of ha.md
//! section 15 after every step ([`check`]); a violation panics with the scenario, the
//! seed and the property. A seed replays to the same trace.

mod cell;
pub mod check;
mod client;
mod daemon;
mod replica;
mod server;
mod store;

use kbf_meta::ObjectId;
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, Outcome, Resources};

pub use cell::{Kind, RunReport, Scenario, run};
pub use client::{FakeClient, Job};
pub use daemon::FakeDaemon;
pub use server::{Seen, Server, ServerFault, ServerPlan};
pub use store::ObjectStore;

/// What the nodes of a cell send each other. `stream` stands for a daemon's gRPC
/// stream: a daemon numbers its streams, and a new one replaces the old.
#[derive(Clone, Debug)]
pub enum Msg {
    /// Client to server: store this blob (the bytes travel with it).
    Upload {
        digest: Digest,
    },
    /// Server to client: the blob is stored and indexed.
    Uploaded {
        digest: Digest,
    },
    /// Client to server: `FindMissingBlobs`.
    FindMissing {
        digests: Vec<Digest>,
    },
    /// Server to client: of `asked`, these are missing.
    Missing {
        asked: Vec<Digest>,
        missing: Vec<Digest>,
    },
    /// Client to server: `Execute`.
    Execute {
        action: Digest,
    },
    /// Server to client: the action's answer; `cached` if from the action cache.
    Answered {
        action: Digest,
        cached: bool,
    },
    /// Server to object store: write `object`, holding `digest`.
    Put {
        object: ObjectId,
        digest: Digest,
    },
    /// Object store to server: `object` is written.
    PutDone {
        object: ObjectId,
        digest: Digest,
    },
    /// Daemon to server: the first message of a stream.
    Hello {
        stream: u64,
        instance: String,
        capacity: Resources,
    },
    /// Server to daemon: the answer to a stream's first `Hello`.
    Welcome {
        stream: u64,
        epoch: u64,
        interval_ms: u64,
    },
    Heartbeat {
        stream: u64,
        seq: u64,
        running: Vec<LeaseId>,
    },
    HeartbeatAck {
        stream: u64,
        seq: u64,
    },
    Start {
        stream: u64,
        lease: LeaseId,
        operation: OperationId,
        action: Digest,
    },
    /// Daemon to server: a lease's result, naming the action its `Start` named.
    Report {
        stream: u64,
        lease: LeaseId,
        action: Digest,
        outcome: Outcome,
    },
    ResultAck {
        lease: LeaseId,
        accepted: bool,
    },
    Cancel {
        stream: u64,
        lease: LeaseId,
    },
    /// Server to daemon: this server holds no such stream (it restarted); open another.
    Reset {
        stream: u64,
    },
}

/// The action digest of action `n`.
#[must_use]
pub fn action(n: u64) -> Digest {
    numbered(0xAC, n)
}

/// The input blob of `action`, which a run of it also names as its `ActionResult`, so
/// an action-cache entry is servable without a second upload.
#[must_use]
pub fn input_of(action: &Digest) -> Digest {
    let mut input = *action;
    input.hash[0] = 0x1B;
    input
}

fn numbered(tag: u8, n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[0] = tag;
    hash[8..16].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n + 1)
}
