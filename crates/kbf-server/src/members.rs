//! Membership in the state store (`docs/design/architecture-v2.md`, sections 6 and 8.4):
//! who the storage nodes are, whether they are alive, and the map built from them.
//! Only a frontend runs this; storage nodes reach it through [`crate::members_rpc`].
//!
//! **Keys.**
//!
//! | key | value | lives |
//! |---|---|---|
//! | `instance/<instance id>` | the member id | for ever: a node that returns keeps its id |
//! | `member/<id>` | the member (address, zone, weight, admin state), its boot counter, its newest debts report and the protocol version it registered naming | `ttl` after the last heartbeat |
//! | `down/<id>` | nothing | `dwell` from the moment the member went `Down` |
//! | `map` | the current `kbf.map.v1.Map` | for ever; a change is one compare-and-set |
//! | `meta/members_formed` | nothing | for ever: the cluster has had an `Active` member |
//! | `deny/<kind>/<value>` | the reason and the time it was added | for ever, until [`Members::allow`] |
//!
//! and the counters `member-id` and `boot/<id>`. The store assigns both numbers, so no
//! node picks its identity; `(id, boot)` is the prefix of the node's object keys.
//!
//! **Life of a member.** `register` writes `member/<id>` as `Active` when the node says
//! its index is whole ([`Progress::CaughtUp`]: a first start of a cluster, or a restart whose
//! meta log already has its rebuild) and as `CatchingUp` when it is rebuilding. A heartbeat
//! rewrites the row to restart its time to live. When the store's clock expires the row
//! (never a frontend's), the next [`Members::refresh`] marks the member `Down` in the map
//! and writes `down/<id>` with the dwell as its time to live in the same transaction;
//! when that expires too, the next refresh drops the member from the map. A member that
//! registers again meanwhile (a restart: same id, larger boot counter) is `Active` again
//! if its index is whole, `CatchingUp` if not, and its `down/<id>` mark is deleted. The
//! last member never leaves: a map needs one member, so it stays `Down` in the map.
//!
//! **Boot in the map.** Each map entry carries the boot counter of the registration it
//! stands for, so a registration with a new boot is a change of the entry and moves the
//! generation, even for a restart inside the lease that never made the member `Down`. A
//! reader that caches what a node said (it holds a blob) against the generation then drops
//! it when the node restarts. Heartbeats do not change the boot, so they move nothing.
//!
//! **Debts.** A storage heartbeat says how many copies the node owes ([`Debts`]); the row
//! keeps the newest report with the store's clock at its receipt, so it dies with the row
//! and a registration (which rewrites the row) starts without one. A promotion keeps it.
//! [`Members::storage_health`] judges the rows ([`crate::storage_health`]).
//!
//! **Protocol.** The row keeps the protocol version the node named when it registered, for
//! the operator view ([`Members::protocols`], `GET /v1/members`): a heartbeat and a state
//! change keep it, a new registration replaces it, and nothing judges it after the
//! registration. A row written before the field existed names none and reads as a node
//! whose version is not known, never as version 0. The row lives `ttl` after the last
//! heartbeat, so a member that went `Down` has no version either.
//!
//! **The map** is a function of the rows: [`Members::refresh`] reads `map`, `member/` and
//! the `down/` marks, builds the next member list, and if it differs commits generation
//! `g + 1` with the compare-and-set on `map`'s version and a check on every row it read,
//! so a row that changed meanwhile fails the commit rather than being decided on stale
//! reads. A conflict is read again; two frontends racing give one winner and one retry.
//! Anyone may call it; the frontend does so after every change and on a timer.
//!
//! **The floor.** The map carries `min_protocol`, the oldest protocol a registration may name.
//! The first map of an empty store has [`OLDEST_PROTOCOL`] (the oldest this build accepts,
//! not its own newest [`PROTOCOL`]: a fresh store must not lock out the nodes a build still
//! accepts), and every later map has the larger of the stored floor and this build's oldest,
//! so the floor only rises. [`Members::register`] judges a node against that floor, read from
//! the store. Nothing lowers it: a frontend rolled back to a build with a lower oldest keeps
//! the stored floor and refuses the nodes the floor excludes, though that build would accept
//! them (a rollback across a protocol bump is a flag day,
//! `docs/design/frontend-proxy.md` section 9). Only registration judges the floor: storage
//! nodes do not enforce `min_protocol` themselves, and a floor this build lifts reaches the
//! stored map with the next change of members, not on its own.
//!
//! **Promotion.** A node's own request moves it in two ways only. A registration saying the
//! index is whole ([`Progress::CaughtUp`]) writes `Active`, so a rolling restart leaves
//! every member `Active`; a registration saying it is rebuilding ([`Progress::CatchingUp`])
//! writes `CatchingUp`, and its first heartbeat saying the rebuild is done makes it `Active`,
//! in the same compare-and-set as the beat, so a state set meanwhile (`Draining`) is never
//! overwritten. Whatever makes a member `Active` also writes `meta/members_formed` in the
//! same transaction. `Joining` is set by hand ([`Members::set_state`], the hook the join
//! handoff and the operator call); a heartbeat changes no other state: a `Joining` member
//! waits for the handoff, and a member that is `Draining`, `Active` or `Down` stays so
//! whatever it reports.
//!
//! **Who may beat.** The row keeps the member's address, and a heartbeat names the member by
//! its (public, sequential) id and boot, so the caller says whether it is the holder of the
//! address ([`Members::heartbeat`]); the rpc layer answers that with the caller's
//! certificate. The row is read, the holder asked and only then the boot compared.
//!
//! **Who is denied.** The `deny/` rows are the operator's deny list ([`Members::deny`],
//! [`Members::allow`]). The key is the whole entry, `deny/<kind>/<value>` for a
//! [`DenyEntry`] in its canonical spelling (`deny/serial/a1b`, `deny/spki-sha256/<64 hex>`,
//! `deny/node/mac-07`), so an entry is one row whatever it was typed as (a node id in the
//! lower case the operator API writes), and the value is only the reason and the time. [`Members::refresh`] folds the rows, sorted, into the
//! map's `denied` list, and a change of that list alone is a new generation: every storage
//! node is sent the map, and from the first call after it holds that generation refuses
//! the certificates it names (`kbf_storage::service`'s `Admission`). The membership calls
//! read the list themselves ([`crate::members_rpc`]): a `Register` the rows, so an entry
//! counts at once, a `Heartbeat` the map. A node that is refused its beats lapses after
//! the time to live like any silent one.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_map::{DenyEntry, Map, MapError, Member, MemberId, MemberState, Rule};
use kbf_mapsync::MapSource;
use kbf_proto::kbf::map::v1 as pb;
use kbf_state::{Cond, Store, StoreError, StoreTime, Txn, Version, Versioned};
use prost::Message as _;
use tokio::sync::watch;

use crate::storage_health::{Debts, MemberReport, Report, StorageHealth, storage_health};
use crate::worker::named_protocol;

const MAP_KEY: &str = "map";
const MEMBERS: &str = "member/";
const ID_COUNTER: &str = "member-id";
/// Written with every `Active` member and never deleted: the cluster has had an
/// `Active` member.
const FORMED: &str = "meta/members_formed";
/// Reads and commits tried before a call gives up on writers it keeps racing.
const ATTEMPTS: usize = 16;
/// The prefix of the deny list's rows.
const DENY: &str = "deny/";
/// The most entries the deny list holds: it is carried in every map a node is sent, so it
/// is bounded. [`Members::deny`] refuses a new entry past it.
pub const MAX_DENIED: usize = 4096;
/// The longest value of a deny entry, in bytes: a DNS name is at most 253.
const MAX_VALUE: usize = 253;
/// The longest reason a deny entry is stored with, in bytes.
const MAX_REASON: usize = 512;

/// The newest protocol version this build speaks: the worker protocol's, so the two
/// cannot drift (`kbf --version --json` reports one number for the whole binary).
pub const PROTOCOL: u32 = crate::worker::PROTOCOL_VERSION;

/// The oldest protocol version this build accepts from a storage node: the worker
/// protocol's, for the same reason. It is the floor (the map's `min_protocol`) of the
/// first map a build makes.
pub const OLDEST_PROTOCOL: u32 = crate::worker::OLDEST_PROTOCOL_VERSION;

/// How long rows live and how many owners a digest has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MembershipConfig {
    /// A member row lives this long after its last heartbeat; then the member is `Down`.
    pub ttl: Duration,
    /// A `Down` member stays in the map this long before it leaves.
    pub dwell: Duration,
    /// The replication factor of the map (clamped to the member count).
    pub replication: u8,
}

impl Default for MembershipConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(15),
            dwell: Duration::from_secs(300),
            replication: 2,
        }
    }
}

impl MembershipConfig {
    /// How often a node heartbeats and a frontend refreshes: a third of the time to
    /// live, so two beats can be lost before the row expires.
    #[must_use]
    pub fn beat(&self) -> Duration {
        self.ttl / 3
    }

    /// How old a debts report may be and still count: two beats, so one lost heartbeat is
    /// forgiven and two are not.
    #[must_use]
    pub fn freshness(&self) -> Duration {
        self.beat() * 2
    }
}

/// Whether a node is still rebuilding its index, as it says when it registers and at
/// every heartbeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// Rebuilding: it enters the map as `CatchingUp`, and stays so.
    CatchingUp,
    /// Its index is whole: a `CatchingUp` member becomes `Active`.
    CaughtUp,
}

impl Progress {
    /// The wire's `catching_up` field.
    #[must_use]
    pub const fn from_wire(catching_up: bool) -> Self {
        if catching_up {
            Self::CatchingUp
        } else {
            Self::CaughtUp
        }
    }
}

/// What a node says about itself when it registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registrant {
    /// Stable across restarts: the key of its member id.
    pub instance_id: String,
    /// The address callers dial.
    pub addr: String,
    /// The zone label; may be empty.
    pub zone: String,
    /// Its relative capacity, at least 1.
    pub weight: u32,
    /// The newest protocol version it speaks.
    pub protocol: u32,
    /// Whether it is rebuilding its index.
    pub progress: Progress,
}

/// The value of a `deny/` row: why the entry was added and when (the store's clock, in
/// milliseconds since its origin).
#[derive(Clone, PartialEq, prost::Message)]
struct DenyRow {
    #[prost(string, tag = "1")]
    reason: String,
    #[prost(uint64, tag = "2")]
    at_ms: u64,
}

/// What a registration returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// The member id the store assigned (the same one for the same instance).
    pub id: MemberId,
    /// The boot counter: larger at every registration of this member.
    pub boot: u64,
    /// The map after this registration.
    pub map: Map,
}

/// Why a membership call failed.
#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    /// The state store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The caller is not at the member's address: its certificate names no host of it
    /// (see [`Members::heartbeat`]). The message gives no more than the member id.
    #[error("member {0} is not at an address the caller's certificate names")]
    NotHolder(MemberId),
    /// The caller's certificate is on the deny list (the entry that names it).
    #[error("denied: {0}")]
    Denied(DenyEntry),
    /// The registration is malformed.
    #[error("invalid registration: {0}")]
    Invalid(&'static str),
    /// The node speaks a protocol older than the map's minimum.
    #[error("protocol {got} is below the minimum {min}")]
    Protocol {
        /// What the node speaks.
        got: u32,
        /// What the map requires.
        min: u32,
    },
    /// The member has no row: it expired, or never registered. Register again.
    #[error("member {0} has no live row: register again")]
    NotRegistered(MemberId),
    /// A newer registration of the same member exists.
    #[error("member {id} boot {boot} was superseded by boot {current}")]
    Superseded {
        /// The member.
        id: MemberId,
        /// The boot counter the caller holds.
        boot: u64,
        /// The one on the row.
        current: u64,
    },
    /// Only `Joining`, `CatchingUp`, `Active` and `Draining` are set by hand; `Down` is
    /// the store's verdict.
    #[error("a member cannot be set {0:?}")]
    NotSettable(MemberState),
    /// Another writer kept winning the compare-and-set.
    #[error("gave up after {ATTEMPTS} attempts against other writers")]
    Contended,
    /// No member is live, so there is no map to answer with.
    #[error("no member is live")]
    NoMap,
    /// A stored value does not decode.
    #[error("stored membership is unreadable: {0}")]
    Corrupt(String),
    /// The members the rows give do not make a map.
    #[error("the members do not make a map: {0}")]
    Map(#[from] MapError),
}

/// A stored member row: the member as the map will hold it, its boot counter, the newest
/// debts report and the protocol version its node registered with.
#[derive(Clone, PartialEq, prost::Message)]
struct Row {
    #[prost(message, optional, tag = "1")]
    member: Option<pb::Member>,
    #[prost(uint64, tag = "2")]
    boot: u64,
    #[prost(message, optional, tag = "3")]
    report: Option<StoredReport>,
    /// The `protocol` of the registration that wrote the row. 0 is absent: a row written
    /// before this field existed (or by a registration that named none) reads as a node
    /// whose version is not known ([`named_protocol`]).
    #[prost(uint32, tag = "4")]
    protocol: u32,
}

/// [`Report`] as stored.
#[derive(Clone, PartialEq, prost::Message)]
struct StoredReport {
    #[prost(bool, tag = "1")]
    known: bool,
    #[prost(uint64, tag = "2")]
    debts: u64,
    /// The store's clock at receipt, in nanoseconds.
    #[prost(uint64, tag = "3")]
    at_nanos: u64,
}

impl From<&Report> for StoredReport {
    fn from(report: &Report) -> Self {
        let (known, debts) = report.debts.to_wire();
        Self {
            known,
            debts,
            at_nanos: u64::try_from(report.at.as_duration().as_nanos()).unwrap_or(u64::MAX),
        }
    }
}

impl From<&StoredReport> for Report {
    fn from(stored: &StoredReport) -> Self {
        Self {
            debts: Debts::from_wire(stored.known, stored.debts),
            at: StoreTime::from_duration(Duration::from_nanos(stored.at_nanos)),
        }
    }
}

/// A decoded row and the version it was read at.
struct Live {
    member: Member,
    boot: u64,
    report: Option<Report>,
    /// What its registration named; `None` for a row that names none.
    protocol: Option<u32>,
    version: Version,
}

fn decode_row(stored: &Versioned) -> Result<Live, MembershipError> {
    let row = Row::decode(stored.value.as_slice())
        .map_err(|e| MembershipError::Corrupt(format!("member row: {e}")))?;
    let mut member = Member::try_from(row.member.unwrap_or_default())
        .map_err(|e| MembershipError::Corrupt(format!("member row: {e}")))?;
    // The row's counter is the authority (a row written before the map named boots has
    // none inside the member).
    member.boot = row.boot;
    Ok(Live {
        member,
        boot: row.boot,
        report: row.report.as_ref().map(Report::from),
        protocol: named_protocol(row.protocol),
        version: stored.version,
    })
}

/// The value of the row of `member` at `boot`, for a node that registered naming
/// `protocol`, with `report` as its newest debts report.
fn encode_row(
    member: &Member,
    boot: u64,
    protocol: Option<u32>,
    report: Option<&Report>,
) -> Vec<u8> {
    Row {
        member: Some(pb::Member::from(member)),
        boot,
        report: report.map(StoredReport::from),
        protocol: protocol.unwrap_or(0),
    }
    .encode_to_vec()
}

fn decode_map(stored: &Versioned) -> Result<Map, MembershipError> {
    let wire = pb::Map::decode(stored.value.as_slice())
        .map_err(|e| MembershipError::Corrupt(format!("map: {e}")))?;
    Map::try_from(wire).map_err(|e| MembershipError::Corrupt(format!("map: {e}")))
}

fn member_key(id: MemberId) -> String {
    format!("{MEMBERS}{}", id.0)
}

fn down_key(id: MemberId) -> String {
    format!("down/{}", id.0)
}

fn boot_counter(id: MemberId) -> String {
    format!("boot/{}", id.0)
}

fn instance_key(instance: &str) -> String {
    format!("instance/{instance}")
}

/// The row of a deny entry: `kind value` is the entry's canonical text, and the row is
/// `deny/kind/value`. A node id may hold a `/`, a kind never does, so the first one splits.
fn deny_key(entry: &DenyEntry) -> String {
    format!("{DENY}{}", entry.to_string().replacen(' ', "/", 1))
}

/// The entry a `deny/` row's key spells.
fn denied_entry(key: &str) -> Result<DenyEntry, MembershipError> {
    key.strip_prefix(DENY)
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(kind, value)| format!("{kind} {value}").parse().ok())
        .ok_or_else(|| MembershipError::Corrupt(format!("deny row {key:?} is not an entry")))
}

/// What one refresh decided: the next map, who just went down, and what it read.
struct Plan {
    map: Map,
    newly_down: Vec<MemberId>,
    guards: Vec<(MemberId, Cond)>,
}

/// The floor (`min_protocol`) of the map a change of `current` makes, for a build that
/// accepts nodes from protocol `oldest` on: `oldest` for the first map; after it the stored
/// floor, lifted to `oldest` if that is higher. It never falls, so a build with a lower
/// `oldest` (a rolled-back frontend) keeps the floor the map has. Pure.
fn next_floor(current: Option<&Map>, oldest: u32) -> u32 {
    current.map_or(oldest, |map| map.min_protocol().max(oldest))
}

/// The next map from the current one, the live rows, the live `down/` marks and the deny
/// list the `deny/` rows give (`denied`: every entry once, sorted), carrying `floor`
/// ([`next_floor`]); `None` when it would not change (or there is no member to put in one). A
/// change of `denied` alone is a change, and a `floor` that differs from the map's is not:
/// it is lifted by the next change. Pure.
fn plan(
    current: Option<&Map>,
    rows: &BTreeMap<MemberId, Live>,
    marks: &BTreeSet<MemberId>,
    denied: &[DenyEntry],
    floor: u32,
    config: &MembershipConfig,
) -> Result<Option<Plan>, MapError> {
    let mut members: BTreeMap<MemberId, Member> = rows
        .iter()
        .map(|(id, live)| (*id, live.member.clone()))
        .collect();
    let mut guards: Vec<(MemberId, Cond)> = rows
        .iter()
        .map(|(id, live)| (*id, Cond::Version(live.version)))
        .collect();
    let mut newly_down = Vec::new();
    for old in current.map_or(&[][..], Map::members) {
        if members.contains_key(&old.id) {
            continue;
        }
        guards.push((old.id, Cond::Absent));
        if old.state != MemberState::Down {
            newly_down.push(old.id);
            let down = Member {
                state: MemberState::Down,
                ..old.clone()
            };
            members.insert(old.id, down);
        } else if marks.contains(&old.id) {
            members.insert(old.id, old.clone());
        }
    }
    if members.is_empty() {
        return Ok(None);
    }
    let members: Vec<Member> = members.into_values().collect();
    let count = u8::try_from(members.len()).unwrap_or(u8::MAX);
    let r = config.replication.min(count);
    if current.is_some_and(|c| (c.members(), c.r(), c.denied()) == (members.as_slice(), r, denied))
    {
        return Ok(None);
    }
    let map = Map::new(
        current.map_or(1, |c| c.generation() + 1),
        members,
        Rule::Rendezvous,
        r,
        floor,
        denied.to_vec(),
    )?;
    Ok(Some(Plan {
        map,
        newly_down,
        guards,
    }))
}

/// How a row is rewritten in place.
#[derive(Clone, Copy)]
enum Touch {
    /// A heartbeat of this boot, what it reports and what it says it owes: a whole time to
    /// live more, and the debts report replaced.
    Beat(u64, Progress, Debts),
    /// An administrator's state change: the time to live it had.
    State(MemberState),
}

/// The maps a process has seen: the newest, and the generation it replaced.
#[derive(Default)]
struct Seen {
    latest: Option<Arc<Map>>,
    previous: Option<Arc<Map>>,
}

/// Membership over a state store.
pub struct Members {
    store: Arc<dyn Store>,
    config: MembershipConfig,
    /// The newest map this process has seen, whoever bumped it, and the one before it.
    seen: Mutex<Seen>,
    /// Its generation (0 before any): [`MapSource::changes`].
    generation: watch::Sender<u64>,
    /// The oldest protocol this build accepts: the least floor of any map it makes.
    oldest: u32,
}

impl Members {
    /// Membership kept in `store`, for a build that accepts storage nodes from
    /// [`OLDEST_PROTOCOL`] on.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, config: MembershipConfig) -> Self {
        Self::with_oldest_protocol(store, config, OLDEST_PROTOCOL)
    }

    /// Membership for a build that accepts storage nodes from protocol `oldest` on. Every
    /// protocol constant is 1 today, so this is how a test stands in for a later build. Only
    /// the oldest end of the range judges anything: a node that speaks newer than the
    /// frontend speaks down to it, so the newest end has no role here.
    #[must_use]
    pub fn with_oldest_protocol(
        store: Arc<dyn Store>,
        config: MembershipConfig,
        oldest: u32,
    ) -> Self {
        Self {
            store,
            config,
            seen: Mutex::default(),
            generation: watch::Sender::new(0),
            oldest,
        }
    }

    /// Notes `map` as seen: if it is newer than the last, keeps it and wakes the
    /// [`MapSource::changes`] receivers.
    fn publish(&self, map: &Map) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen
            .latest
            .as_ref()
            .is_none_or(|l| l.generation() < map.generation())
        {
            seen.previous = seen.latest.replace(Arc::new(map.clone()));
            self.generation.send_replace(map.generation());
        }
    }

    /// The newest map this process has seen and the one it replaced, which a call routed
    /// by the newer finds a blob written before the change in. `None` before any map.
    /// Reads no store: [`Members::current_map`] does, and publishes what it finds.
    #[must_use]
    pub fn view(&self) -> Option<(Arc<Map>, Option<Arc<Map>>)> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let latest = seen.latest.clone()?;
        Some((latest, seen.previous.clone()))
    }

    /// The map the push protocol reads: its current value (`None` until the first member
    /// registers) and a receiver of its generation, which changes on every bump this
    /// process commits or observes (a bump by another frontend is seen at the next
    /// [`Members::refresh`], which the frontend runs every beat). Seeded from the store,
    /// so a restarted frontend has the map its members left.
    ///
    /// # Errors
    /// The store failing.
    pub fn map_source(self: &Arc<Self>) -> Result<Arc<dyn MapSource>, MembershipError> {
        self.current_map()?;
        Ok(Arc::new(MemberMap {
            members: Arc::clone(self),
        }))
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &MembershipConfig {
        &self.config
    }

    /// Registers a storage node: assigns its id (the same for the same instance) and a
    /// larger boot counter, writes its row as `Active` (`CatchingUp` if it is rebuilding
    /// its index, see the module docs), and rebuilds the map.
    ///
    /// The node is judged against the floor of the map this registration makes
    /// ([`next_floor`]: the stored map's, lifted to this build's oldest), so what the store
    /// holds decides, not the build's constants. The map is read first, then the node's
    /// protocol compared, and only then is an identity assigned: a refused node leaves no
    /// row. The read is [`Members::current_map`], so it also publishes the stored map to
    /// [`Members::view`] and [`MapSource::changes`], refused or not, and a registration costs
    /// one more store read than it did. The read and the commit are not one transaction: a
    /// floor another frontend raises between them is not applied to this call.
    ///
    /// # Errors
    /// [`MembershipError::Invalid`] for a bad request; the store failing (the map is read
    /// before the protocol is judged, so a store that fails is this and not
    /// [`MembershipError::Protocol`]); [`MembershipError::Protocol`] for a node below the
    /// floor; [`MembershipError::Superseded`] if a newer registration of the instance won;
    /// [`MembershipError::Contended`].
    pub fn register(&self, who: &Registrant) -> Result<Registration, MembershipError> {
        if who.instance_id.is_empty() || who.addr.is_empty() || who.weight == 0 {
            return Err(MembershipError::Invalid(
                "an instance id, an address and a weight of at least 1 are required",
            ));
        }
        let floor = next_floor(self.current_map()?.as_ref(), self.oldest);
        if who.protocol < floor {
            return Err(MembershipError::Protocol {
                got: who.protocol,
                min: floor,
            });
        }
        let (id, boot) = self.assign(&who.instance_id)?;
        let member = Member {
            id,
            addr: who.addr.clone(),
            zone: who.zone.clone(),
            weight: who.weight,
            state: match who.progress {
                Progress::CatchingUp => MemberState::CatchingUp,
                Progress::CaughtUp => MemberState::Active,
            },
            // Published in the map, so a restart inside the lease changes the member (and the
            // generation) even when nothing else about it did.
            boot,
        };
        self.write_row(&member, boot, named_protocol(who.protocol))?;
        let map = self.refresh()?.ok_or(MembershipError::NoMap)?;
        Ok(Registration { id, boot, map })
    }

    /// The id of `instance` (assigned on first sight) and a new boot counter for it.
    fn assign(&self, instance: &str) -> Result<(MemberId, u64), MembershipError> {
        let key = instance_key(instance);
        for _ in 0..ATTEMPTS {
            let (id, txn) = match self.store.get(&key)? {
                Some(known) => {
                    let id = MemberId(decode_id(&known.value)?);
                    let txn = Txn::new().check(&key, Cond::Version(known.version));
                    (id, txn.next(boot_counter(id)))
                }
                None => {
                    // A counter has no precondition, so it cannot conflict.
                    let taken = self.store.txn(Txn::new().next(ID_COUNTER))?;
                    let id = MemberId(taken.map_err(|_| MembershipError::Contended)?.counters[0]);
                    let txn = Txn::new().put(&key, id.0.to_be_bytes(), Cond::Absent);
                    (id, txn.next(boot_counter(id)))
                }
            };
            if let Ok(commit) = self.store.txn(txn)? {
                return Ok((id, commit.counters[0]));
            }
        }
        Err(MembershipError::Contended)
    }

    /// Stores `member`'s row with a whole time to live, unless a newer boot is there, and
    /// clears its `down/` mark. The row names the `protocol` this registration did, in
    /// place of any an earlier boot wrote. An `Active` row also writes
    /// `meta/members_formed`, in the same transaction.
    fn write_row(
        &self,
        member: &Member,
        boot: u64,
        protocol: Option<u32>,
    ) -> Result<(), MembershipError> {
        let key = member_key(member.id);
        for _ in 0..ATTEMPTS {
            let cond = match self.store.get(&key)? {
                Some(stored) => {
                    let current = decode_row(&stored)?.boot;
                    if current > boot {
                        return Err(MembershipError::Superseded {
                            id: member.id,
                            boot,
                            current,
                        });
                    }
                    Cond::Version(stored.version)
                }
                None => Cond::Absent,
            };
            let mut txn = Txn::new();
            if member.state == MemberState::Active {
                txn = txn.put(FORMED, Vec::new(), Cond::Any);
            }
            let txn = txn
                .put_ttl(
                    &key,
                    encode_row(member, boot, protocol, None),
                    self.config.ttl,
                    cond,
                )
                .delete(down_key(member.id), Cond::Any);
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
        Err(MembershipError::Contended)
    }

    /// Restarts the time to live of `id`'s row, keeps what the node says it owes (with the
    /// store's clock) and, if the member is `CatchingUp` and `progress` says it is done,
    /// makes it `Active` and rebuilds the map. Returns the generation of the current map.
    ///
    /// `holder` says whether the caller may speak for a member at an address (its
    /// certificate names the address's host): it is asked of the address on the row,
    /// before the boot is compared and in the same pass as the rewrite, so a caller that
    /// is not the holder learns nothing of the row's boot or state, not even that its boot
    /// is wrong (it can tell a row that lapsed, `NotRegistered`, from a live one it does
    /// not hold, `NotHolder`, and member ids and states are in the map every node gets
    /// anyway), and promotes nothing. `None` asks no one.
    ///
    /// # Errors
    /// [`MembershipError::NotRegistered`] if the row expired (register again);
    /// [`MembershipError::NotHolder`] if `holder` refuses the row's address;
    /// [`MembershipError::Superseded`] if `boot` is not the row's; the store failing.
    pub fn heartbeat(
        &self,
        id: MemberId,
        boot: u64,
        progress: Progress,
        debts: Debts,
        holder: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<u64, MembershipError> {
        if self.touch(id, Touch::Beat(boot, progress, debts), holder)? {
            return Ok(self.refresh()?.map_or(0, |map| map.generation()));
        }
        Ok(self.current_map()?.map_or(0, |map| map.generation()))
    }

    /// Sets a live member's state (the promotion hook: `Joining`, `CatchingUp`, `Active`,
    /// `Draining`) and rebuilds the map. The row keeps the time to live it had.
    ///
    /// # Errors
    /// [`MembershipError::NotSettable`] for `Down`; [`MembershipError::NotRegistered`] if
    /// the member has no live row; the store failing.
    pub fn set_state(&self, id: MemberId, state: MemberState) -> Result<Map, MembershipError> {
        if state == MemberState::Down {
            return Err(MembershipError::NotSettable(state));
        }
        self.touch(id, Touch::State(state), None)?;
        self.refresh()?.ok_or(MembershipError::NoMap)
    }

    /// Rewrites `id`'s row as `how` says; whether its state changed. `holder` is
    /// [`Members::heartbeat`]'s.
    fn touch(
        &self,
        id: MemberId,
        how: Touch,
        holder: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<bool, MembershipError> {
        let key = member_key(id);
        for _ in 0..ATTEMPTS {
            let stored = self
                .store
                .get(&key)?
                .ok_or(MembershipError::NotRegistered(id))?;
            let mut live = decode_row(&stored)?;
            // Before anything the caller said is compared with the row.
            if holder.is_some_and(|holds| !holds(&live.member.addr)) {
                return Err(MembershipError::NotHolder(id));
            }
            let before = live.member.state;
            let ttl = match how {
                Touch::Beat(boot, _, _) if boot != live.boot => {
                    return Err(MembershipError::Superseded {
                        id,
                        boot,
                        current: live.boot,
                    });
                }
                Touch::Beat(_, progress, debts) => {
                    live.report = Some(Report {
                        debts,
                        at: self.store.now(),
                    });
                    if before == MemberState::CatchingUp && progress == Progress::CaughtUp {
                        live.member.state = MemberState::Active;
                    }
                    self.config.ttl
                }
                Touch::State(state) => {
                    live.member.state = state;
                    stored
                        .expires_at
                        .map_or(self.config.ttl, |at| at.since(self.store.now()))
                }
            };
            let mut put = Txn::new().put_ttl(
                &key,
                encode_row(&live.member, live.boot, live.protocol, live.report.as_ref()),
                ttl,
                Cond::Version(stored.version),
            );
            if live.member.state == MemberState::Active && before != MemberState::Active {
                put = put.put(FORMED, Vec::new(), Cond::Any);
            }
            if self.store.txn(put)?.is_ok() {
                return Ok(live.member.state != before);
            }
        }
        Err(MembershipError::Contended)
    }

    /// Adds `entry` to the deny list, with the operator's `reason`. It reaches the map at the
    /// next [`Members::refresh`] (the operator call runs one at once). An entry that is
    /// listed already is left as it is, with its first reason and time. The entry is stored
    /// as given: a node id is compared with a certificate's DNS names up to ASCII case, but
    /// `Mac-07` and `mac-07` are two rows (the operator API lowercases a node id, so it
    /// writes one).
    ///
    /// # Errors
    /// [`MembershipError::Invalid`] for a value over 253 bytes or with a control character
    /// in it, a node that is an IP address or has a colon (it would name no certificate:
    /// only DNS names are compared), a reason over 512 bytes, or a new entry when
    /// [`MAX_DENIED`] are listed; the store failing.
    pub fn deny(&self, entry: &DenyEntry, reason: &str) -> Result<(), MembershipError> {
        let (DenyEntry::Serial(value) | DenyEntry::SpkiSha256(value) | DenyEntry::Node(value)) =
            entry;
        if value.len() > MAX_VALUE {
            return Err(MembershipError::Invalid(
                "a deny entry's value is at most 253 bytes",
            ));
        }
        if value.chars().any(char::is_control) {
            return Err(MembershipError::Invalid(
                "a deny entry's value has no control character",
            ));
        }
        if let DenyEntry::Node(node) = entry
            && (node.contains(':') || node.parse::<IpAddr>().is_ok())
        {
            return Err(MembershipError::Invalid(
                "a node entry is a DNS name, not an IP address or a host:port (a certificate's IP \
                 names are not compared with it): list the certificate by serial or spki-sha256",
            ));
        }
        if reason.len() > MAX_REASON {
            return Err(MembershipError::Invalid(
                "a deny entry's reason is at most 512 bytes",
            ));
        }
        let listed = self.denied()?;
        if listed.contains(entry) {
            return Ok(());
        }
        if listed.len() >= MAX_DENIED {
            return Err(MembershipError::Invalid(
                "the deny list is full: allow an entry before denying another",
            ));
        }
        let row = DenyRow {
            reason: reason.to_owned(),
            at_ms: u64::try_from(self.store.now().as_duration().as_millis()).unwrap_or(u64::MAX),
        };
        // A conflict is another writer's row for this very entry: denied, as asked.
        let _won =
            self.store
                .txn(Txn::new().put(deny_key(entry), row.encode_to_vec(), Cond::Absent))?;
        Ok(())
    }

    /// Removes `entry` from the deny list, from the next [`Members::refresh`] on. Whether it
    /// was listed.
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Contended`].
    pub fn allow(&self, entry: &DenyEntry) -> Result<bool, MembershipError> {
        let key = deny_key(entry);
        for _ in 0..ATTEMPTS {
            let Some(stored) = self.store.get(&key)? else {
                return Ok(false);
            };
            let txn = Txn::new().delete(&key, Cond::Version(stored.version));
            if self.store.txn(txn)?.is_ok() {
                return Ok(true);
            }
        }
        Err(MembershipError::Contended)
    }

    /// The deny list as the rows give it now: every entry once, in the order `DenyEntry`
    /// sorts (serials, then key hashes, then node ids). Not the map's, which lags the rows
    /// by up to a refresh.
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Corrupt`] for a `deny/` row that is no entry
    /// (the list is not guessed at: an entry dropped would let its caller back in).
    pub fn denied(&self) -> Result<Vec<DenyEntry>, MembershipError> {
        let mut entries = BTreeSet::new();
        for (key, _) in self.store.scan_prefix(DENY)? {
            entries.insert(denied_entry(&key)?);
        }
        Ok(entries.into_iter().collect())
    }

    /// The current map, or `None` before the first member registered. This is what a
    /// map push (`Welcome.map`, `MapUpdate`) and a `STALE_MAP` answer read.
    ///
    /// # Errors
    /// The store failing; the stored map not decoding.
    pub fn current_map(&self) -> Result<Option<Map>, MembershipError> {
        let stored = self.store.get(MAP_KEY)?;
        let map = stored.as_ref().map(decode_map).transpose()?;
        map.iter().for_each(|m| self.publish(m));
        Ok(map)
    }

    /// Frees the rows the store's clock has expired (member, dwell and session rows
    /// among them) and returns how many. Reads hide an expired row whether or not this
    /// has run; it only keeps the file from growing.
    ///
    /// # Errors
    /// The store failing.
    pub fn reap(&self) -> Result<usize, MembershipError> {
        Ok(self.store.reap_expired()?)
    }

    /// Brings the map up to date with the rows: members that expired go `Down`, `Down`
    /// members whose dwell ran out leave, new and changed rows enter, and the map's deny
    /// list becomes the `deny/` rows ([`Members::denied`]). Returns the map after, and
    /// bumps the generation by one compare-and-set only if something changed (an entry
    /// added or removed is a change).
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Contended`].
    pub fn refresh(&self) -> Result<Option<Map>, MembershipError> {
        for _ in 0..ATTEMPTS {
            let stored = self.store.get(MAP_KEY)?;
            let current = stored.as_ref().map(decode_map).transpose()?;
            let rows = self.read_rows()?;
            let marks = self.read_marks(current.as_ref())?;
            // A deny written after this read is in the next refresh's map (the sweeper's,
            // within a beat): a row that is not there yet cannot be a precondition.
            let denied = self.denied()?;
            let floor = next_floor(current.as_ref(), self.oldest);
            let Some(plan) = plan(
                current.as_ref(),
                &rows,
                &marks,
                &denied,
                floor,
                &self.config,
            )?
            else {
                current.iter().for_each(|m| self.publish(m));
                return Ok(current);
            };
            let cond = stored.map_or(Cond::Absent, |s| Cond::Version(s.version));
            let wire = pb::Map::from(&plan.map).encode_to_vec();
            let mut txn = Txn::new().put(MAP_KEY, wire, cond);
            for id in &plan.newly_down {
                txn = txn.put_ttl(down_key(*id), Vec::new(), self.config.dwell, Cond::Any);
            }
            for (id, cond) in &plan.guards {
                txn = txn.check(member_key(*id), *cond);
            }
            if self.store.txn(txn)?.is_ok() {
                self.publish(&plan.map);
                return Ok(Some(plan.map));
            }
        }
        Err(MembershipError::Contended)
    }

    /// The judgement of [`crate::storage_health::storage_health`] over the members the map
    /// holds and the rows that are live: a member in the map with no row (`Down`, or a row
    /// that expired a moment ago) counts as it is in the map, with no report; a row the map
    /// does not hold yet counts as the row says.
    ///
    /// # Errors
    /// The store failing; a stored value not decoding.
    pub fn storage_health(&self) -> Result<StorageHealth, MembershipError> {
        let mut members: BTreeMap<MemberId, MemberReport> = BTreeMap::new();
        for member in self.current_map()?.as_ref().map_or(&[][..], Map::members) {
            members.insert(
                member.id,
                MemberReport {
                    id: member.id,
                    state: member.state,
                    report: None,
                },
            );
        }
        for (id, live) in self.read_rows()? {
            members.insert(
                id,
                MemberReport {
                    id,
                    state: live.member.state,
                    report: live.report,
                },
            );
        }
        // After the reads, so a report is judged no fresher than the clock then was.
        let now = self.store.now();
        let members: Vec<MemberReport> = members.into_values().collect();
        Ok(storage_health(&members, now, self.config.freshness()))
    }

    /// The protocol version each member with a live row registered naming (`None`: the row
    /// names none, as a row written before the field existed). A member the map holds
    /// whose row has lapsed (`Down`) is not here: its row, which held the version, expired
    /// with the heartbeats, so a reader treats it as not known.
    ///
    /// # Errors
    /// The store failing; a stored row not decoding.
    pub fn protocols(&self) -> Result<BTreeMap<MemberId, Option<u32>>, MembershipError> {
        Ok(self
            .read_rows()?
            .into_iter()
            .map(|(id, live)| (id, live.protocol))
            .collect())
    }

    fn read_rows(&self) -> Result<BTreeMap<MemberId, Live>, MembershipError> {
        let mut rows = BTreeMap::new();
        for (_, stored) in self.store.scan_prefix(MEMBERS)? {
            let live = decode_row(&stored)?;
            rows.insert(live.member.id, live);
        }
        Ok(rows)
    }

    /// The `Down` members of `current` whose dwell has not run out.
    fn read_marks(&self, current: Option<&Map>) -> Result<BTreeSet<MemberId>, MembershipError> {
        let mut marks = BTreeSet::new();
        for m in current.map_or(&[][..], Map::members) {
            if m.state == MemberState::Down && self.store.get(&down_key(m.id))?.is_some() {
                marks.insert(m.id);
            }
        }
        Ok(marks)
    }
}

fn decode_id(value: &[u8]) -> Result<u64, MembershipError> {
    <[u8; 8]>::try_from(value)
        .map(u64::from_be_bytes)
        .map_err(|_| MembershipError::Corrupt("instance entry is not a member id".to_owned()))
}

/// [`Members`] as a [`MapSource`].
struct MemberMap {
    members: Arc<Members>,
}

impl MapSource for MemberMap {
    fn current(&self) -> Option<Arc<Map>> {
        self.members.view().map(|(latest, _)| latest)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.members.generation.subscribe()
    }
}
