//! Which storage nodes applied the registration, as their heartbeats say.
//!
//! **Where the acks live.** Frontend children run side by side on one state store and a
//! proxy spreads a node's heartbeats over them, so an ack kept in one child's memory would
//! show the other child's GET a node it never heard. Each accepted heartbeat therefore
//! writes one row, `objstore/ack/<member id>`, with a time to live of [`ACK_TTL`]: every
//! child reads the same rows. A row that is not renewed lapses, and a lapsed row reads as
//! `pending`, so convergence is never claimed from a report older than the TTL (a node that
//! beats every few seconds renews it long before). A restarted farm with an empty store is
//! `pending` everywhere until heartbeats arrive. A row is replaced only by the same or a
//! newer boot (compare-and-set on the row's version, so two children racing do not let an
//! older boot win), and a registration deletes it: a restarted node has applied nothing yet.
//!
//! **Who is expected.** A member counts when it is `Active` and no `node` deny entry
//! names the host of its address. A draining, joining, catching-up or down member does
//! not, and neither does one that has been denied, whose heartbeats are refused. Of the
//! counted members, one that does not report the fields (a node that predates them sends
//! status 0) or says it cannot use a registration is `unsupported`: it is listed apart,
//! it is not in `expected`, and it keeps the farm from `converged`, since it can never
//! hold the registration.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::{DenyEntry, Map, MemberId, MemberState};
use kbf_proto::kbf::map::v1::{ApplyStatus, HeartbeatRequest};
use kbf_state::{Cond, Store, StoreError, Txn};
use kbf_storage::service::host_of;

/// A node's state of the registration: a closed set, never a node's own words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeStatus {
    /// The node runs on the registration's generation.
    Ok,
    /// The node has not applied it yet, or has not been heard since this frontend started.
    Pending,
    /// The registered keys failed the node's own probe.
    ProbeFailed,
    /// The node runs on a `--cold-store` flag that names another store.
    IdentityMismatch,
    /// The node does not report the fields, or cannot use a registration.
    Unsupported,
}

impl NodeStatus {
    /// The code the operator API shows.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::ProbeFailed => "probe_failed",
            Self::IdentityMismatch => "identity_mismatch",
            Self::Unsupported => "unsupported",
        }
    }
}

/// What one heartbeat said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Report {
    generation: u64,
    status: NodeStatus,
}

impl Report {
    /// Reads the heartbeat's two fields. Status 0 is "does not report", and so is a
    /// number this build does not name: either way the node is shown `unsupported`.
    fn of(beat: &HeartbeatRequest) -> Self {
        let status = match ApplyStatus::try_from(beat.apply_status) {
            Ok(ApplyStatus::Ok) => NodeStatus::Ok,
            Ok(ApplyStatus::Pending) => NodeStatus::Pending,
            Ok(ApplyStatus::ProbeFailed) => NodeStatus::ProbeFailed,
            Ok(ApplyStatus::IdentityMismatch) => NodeStatus::IdentityMismatch,
            Ok(ApplyStatus::Unsupported | ApplyStatus::Unspecified) | Err(_) => {
                NodeStatus::Unsupported
            }
        };
        Self {
            generation: beat.applied_objstore_generation,
            status,
        }
    }
}

/// How long a report counts: a node beats three times in the membership TTL, and this is as
/// long as that TTL's default, so two lost beats are forgiven and a silent node is `pending`.
pub const ACK_TTL: Duration = Duration::from_secs(15);

const PREFIX: &str = "objstore/ack/";

/// A stored report: boot, generation, status number, store-clock seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    boot: u64,
    report: Report,
    at: u64,
}

impl Row {
    fn encode(self) -> Vec<u8> {
        let status = match self.report.status {
            NodeStatus::Ok => 1u64,
            NodeStatus::Pending => 2,
            NodeStatus::ProbeFailed => 3,
            NodeStatus::IdentityMismatch => 4,
            NodeStatus::Unsupported => 5,
        };
        [self.boot, self.report.generation, status, self.at]
            .iter()
            .flat_map(|n| n.to_be_bytes())
            .collect()
    }

    /// A row that is not four numbers or names a status this build does not know reads as
    /// absent (`pending`): a newer frontend's row never makes this one fail.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let n: Vec<u64> = bytes
            .chunks_exact(8)
            .map(|c| u64::from_be_bytes(c.try_into().unwrap_or([0; 8])))
            .collect();
        let [boot, generation, status, at] = n[..] else {
            return None;
        };
        let status = match status {
            1 => NodeStatus::Ok,
            2 => NodeStatus::Pending,
            3 => NodeStatus::ProbeFailed,
            4 => NodeStatus::IdentityMismatch,
            5 => NodeStatus::Unsupported,
            _ => return None,
        };
        Some(Self {
            boot,
            report: Report { generation, status },
            at,
        })
    }
}

/// The acks of the storage nodes, fed by `Membership.Heartbeat`, kept in the state store.
pub struct Acks {
    store: Arc<dyn Store>,
}

impl Acks {
    /// Acks over `store`, the state store every frontend child shares.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    fn key(member: MemberId) -> String {
        format!("{PREFIX}{}", member.0)
    }

    /// Records what `beat`, a heartbeat of `member` at `boot`, reported. A heartbeat of an
    /// older boot than the row's changes nothing. Blocks.
    ///
    /// # Errors
    /// The store failing; a lost race is retried once against the fresh row.
    pub fn record(
        &self,
        member: MemberId,
        boot: u64,
        beat: &HeartbeatRequest,
    ) -> Result<(), StoreError> {
        let key = Self::key(member);
        for _ in 0..4 {
            let read = self.store.get(&key)?;
            let held = read.as_ref().and_then(|v| Row::decode(&v.value));
            if held.is_some_and(|row| row.boot > boot) {
                return Ok(());
            }
            let row = Row {
                boot,
                report: Report::of(beat),
                at: self.store.now().as_duration().as_secs(),
            };
            let cond = read.map_or(Cond::Absent, |v| Cond::Version(v.version));
            let txn = Txn::new().put_ttl(&key, row.encode(), ACK_TTL, cond);
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Drops what `member` reported: it registered again, so it is a new boot that has
    /// applied nothing. Blocks.
    ///
    /// # Errors
    /// The store failing.
    pub fn forget(&self, member: MemberId) -> Result<(), StoreError> {
        self.store
            .txn(Txn::new().delete(Self::key(member), Cond::Any))?
            .map(drop)
            .or(Ok(()))
    }

    /// Where the farm stands on `generation`, over the members of `map`. Blocks.
    ///
    /// # Errors
    /// The store failing.
    pub fn convergence(&self, map: &Map, generation: u64) -> Result<Convergence, StoreError> {
        let now = self.store.now().as_duration().as_secs();
        let rows: std::collections::BTreeMap<String, Row> = self
            .store
            .scan_prefix(PREFIX)?
            .into_iter()
            .filter_map(|(k, v)| Some((k, Row::decode(&v.value)?)))
            .collect();
        let mut nodes = Vec::new();
        let (mut expected, mut applied) = (0, 0);
        let mut unsupported = Vec::new();
        for member in map.members() {
            let row = rows.get(&Self::key(member.id));
            let reported = row.map(|row| row.report);
            let status = match reported {
                None => NodeStatus::Pending,
                Some(Report {
                    status: NodeStatus::Ok,
                    generation: held,
                }) if held != generation => NodeStatus::Pending,
                Some(report) => report.status,
            };
            let counted = member.state == MemberState::Active && !denied(map, &member.addr);
            if counted && status == NodeStatus::Unsupported {
                unsupported.push(member.id);
            } else if counted {
                expected += 1;
                if status == NodeStatus::Ok {
                    applied += 1;
                }
            }
            nodes.push(NodeAck {
                member: member.id,
                state: member.state,
                status,
                applied_generation: reported.map(|r| r.generation),
                last_seen: row.map(|row| Duration::from_secs(now.saturating_sub(row.at))),
            });
        }
        Ok(Convergence {
            generation,
            converged: applied == expected && unsupported.is_empty(),
            expected,
            applied,
            unsupported,
            nodes,
        })
    }
}

/// Whether a `node` deny entry names the host of `addr` (a certificate's names cover the
/// host of the address it registered, so that is the name a deny entry for the node
/// carries; serial and key entries cannot be matched from the map and are enforced at the
/// node's next call instead).
fn denied(map: &Map, addr: &str) -> bool {
    let host = host_of(addr);
    map.denied()
        .iter()
        .any(|entry| matches!(entry, DenyEntry::Node(node) if node.eq_ignore_ascii_case(host)))
}

/// One member's line of the convergence answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAck {
    /// The member.
    pub member: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// What it is doing about the registration.
    pub status: NodeStatus,
    /// The generation it last reported; `None` if it has not since this frontend started.
    pub applied_generation: Option<u64>,
    /// How long ago it reported; `None` if it has not since this frontend started.
    pub last_seen: Option<Duration>,
}

/// Where the farm stands on one registration generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Convergence {
    /// The generation judged.
    pub generation: u64,
    /// Every expected node runs on it and no counted node is unsupported.
    pub converged: bool,
    /// The members counted (see the module docs), unsupported ones apart.
    pub expected: usize,
    /// Of those, the nodes running on `generation`.
    pub applied: usize,
    /// Counted members that cannot hold a registration, named so an operator can upgrade them.
    pub unsupported: Vec<MemberId>,
    /// Every member of the map, by id.
    pub nodes: Vec<NodeAck>,
}
