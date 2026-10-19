//! What rule R1 judges (`docs/design/frontend-proxy.md` section 9): the protocol each daemon
//! and storage member the active child knows has spoken, and the nodes that are enrolled.
//! A [`FleetSnapshot`] holds those three reads; [`R1Gate`](super::compat::R1Gate) judges it.
//!
//! Nothing here opens a socket. A [`FleetReader`] (U8b) makes the three requests of the
//! active child's operator port and hands each answer, status and body, to the
//! constructors below, which are the one place the wire is interpreted. A read that
//! fails, or answers something R1 cannot use, is an `Err` in the snapshot: the gate
//! refuses on it, so a child that cannot be asked is never taken for an empty fleet.

use std::collections::BTreeSet;

use hyper::StatusCode;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// A node state that is not enrolled any more. Any other word, one this build does not
/// know included, counts as enrolled: the gate errs towards refusing.
const REVOKED: &str = "revoked";

/// A read that failed or answered something R1 cannot use.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Unreadable(pub String);

/// The daemons (`GET /v1/nodes`) or the storage members (`GET /v1/members`) the child lists,
/// and what its envelope says of their protocols.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Population {
    /// The `node_id` of each daemon, or the `addr` of each member in lower case: the names
    /// an enrolled node is looked up by.
    pub names: BTreeSet<String>,
    /// The envelope's `oldest_protocol`: the least protocol among the entries that name
    /// one, `None` if none does.
    pub oldest: Option<u32>,
    /// The envelope's `protocol_unknown`: the entries that name none. They are not in
    /// `oldest`.
    pub unknown: u32,
}

/// The nodes `GET /v1/enroll/nodes` lists as enrolled, by what they are looked up by in the
/// two views: a daemon by its node id, a storage node by its advertise address in lower
/// case. A node with both roles is in both sets. Empty when the child has no enrollment
/// (a 404), so a daemon with a hand-issued certificate is not seen: R1 cannot refuse for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Enrolled {
    pub daemons: BTreeSet<String>,
    pub storage: BTreeSet<String>,
}

/// The three reads R1 needs, each of which can have failed on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FleetSnapshot {
    pub nodes: Result<Population, Unreadable>,
    pub members: Result<Population, Unreadable>,
    pub enrolled: Result<Enrolled, Unreadable>,
}

/// Reads the fleet from the child that is active now, never from a candidate.
///
/// The actor calls it once per update, when the staged release is judged, and waits for it.
/// What an implementation must do, none of which the trait can check:
///
/// - **Name the active child on every call.** The active child's port changes after every
///   swap, so an address fixed when the reader is built reads a child that is gone. Like
///   [`Forwarder`](super::traits::Forwarder), the reader finds the child at each read, and
///   its test reads after a swap.
/// - **Bound the whole read.** `read` is synchronous and runs on the actor's task, which
///   every API call waits on, abort included, so a slow child stalls them all. One
///   deadline for the three requests together, not a timeout each (three add up), and a
///   blocking client: `block_on` inside a runtime task panics.
/// - **Gate `GET /v1/enroll/nodes` like a write**: a loopback peer, the operator bearer
///   token, `Content-Type: application/json`, no `Origin` (`docs/api.md`). The two views
///   are open.
pub trait FleetReader: Send {
    fn read(&self) -> FleetSnapshot;
}

/// `oldest_protocol` and `protocol_unknown`, which both views carry. A body without them is
/// from a child older than the fields, and says nothing of any protocol.
#[derive(Deserialize)]
struct Summary {
    // `Option::deserialize` makes the key required: a missing `Option` key is `None` otherwise,
    // and `None` is "no node names a protocol".
    #[serde(deserialize_with = "Option::deserialize")]
    oldest_protocol: Option<u32>,
    protocol_unknown: u32,
}

#[derive(Deserialize)]
struct NodesBody {
    nodes: Vec<NodeEntry>,
    #[serde(flatten)]
    summary: Summary,
}

#[derive(Deserialize)]
struct NodeEntry {
    node_id: String,
}

#[derive(Deserialize)]
struct MembersBody {
    members: Vec<MemberEntry>,
    #[serde(flatten)]
    summary: Summary,
}

#[derive(Deserialize)]
struct MemberEntry {
    addr: String,
}

#[derive(Deserialize)]
struct RegistryBody {
    nodes: Vec<RegistryNode>,
}

#[derive(Deserialize)]
struct RegistryNode {
    id: String,
    roles: Vec<String>,
    state: String,
    advertise: String,
}

fn decode<T: DeserializeOwned>(
    what: &str,
    status: StatusCode,
    body: &[u8],
) -> Result<T, Unreadable> {
    if status != StatusCode::OK {
        return Err(Unreadable(format!("{what} answered {status}")));
    }
    serde_json::from_slice(body).map_err(|e| Unreadable(format!("{what}: {e}")))
}

impl Population {
    /// From the answer to `GET /v1/nodes`.
    pub fn from_nodes(status: StatusCode, body: &[u8]) -> Result<Population, Unreadable> {
        let wire: NodesBody = decode("GET /v1/nodes", status, body)?;
        Ok(Population {
            names: wire.nodes.into_iter().map(|n| n.node_id).collect(),
            oldest: wire.summary.oldest_protocol,
            unknown: wire.summary.protocol_unknown,
        })
    }

    /// From the answer to `GET /v1/members`.
    pub fn from_members(status: StatusCode, body: &[u8]) -> Result<Population, Unreadable> {
        let wire: MembersBody = decode("GET /v1/members", status, body)?;
        Ok(Population {
            names: wire
                .members
                .into_iter()
                .map(|m| m.addr.to_ascii_lowercase())
                .collect(),
            oldest: wire.summary.oldest_protocol,
            unknown: wire.summary.protocol_unknown,
        })
    }
}

impl Enrolled {
    /// From the answer to `GET /v1/enroll/nodes`. A 404 is a child with no enrollment, which
    /// enrolls no one; any other status but 200 is unreadable.
    pub fn from_registry(status: StatusCode, body: &[u8]) -> Result<Enrolled, Unreadable> {
        if status == StatusCode::NOT_FOUND {
            return Ok(Enrolled::default());
        }
        let wire: RegistryBody = decode("GET /v1/enroll/nodes", status, body)?;
        let mut out = Enrolled::default();
        for node in wire.nodes.into_iter().filter(|n| n.state != REVOKED) {
            if node.roles.iter().any(|r| r == "storage") {
                out.storage.insert(node.advertise.to_ascii_lowercase());
            }
            if node.roles.iter().any(|r| r == "daemon") {
                out.daemons.insert(node.id);
            }
        }
        Ok(out)
    }
}
