//! Membership in the state store (`docs/design/architecture-v2.md`, sections 6 and 8.4):
//! who the storage nodes are, whether they are alive, and the map built from them.
//! Only a frontend runs this; storage nodes reach it through [`crate::members_rpc`].
//!
//! **Keys.**
//!
//! | key | value | lives |
//! |---|---|---|
//! | `instance/<instance id>` | the member id | for ever: a node that returns keeps its id |
//! | `member-owner/<id>` | the instance id: written with `instance/<instance id>`, so one id is never given to two instances | for ever |
//! | `member/<id>` | the member (address, zone, weight, admin state), its boot counter, its newest debts report, the protocol version it registered naming and what it said of its build and log | `ttl` after the last heartbeat |
//! | `down/<id>` | nothing | `dwell` from the moment the member went `Down` |
//! | `map` | the current `kbf.map.v1.Map` | for ever; a change is one compare-and-set |
//! | `meta/members_formed` | nothing | for ever: the cluster has had an `Active` member |
//! | `meta/members_forming` | nothing | a minute from the first `Active` member |
//! | `unfilled/<id>` | nothing, or the fill phase of [`Handoff`] (module `handoff`) | for ever, until the member is made `Active` |
//! | `meta/tail_epoch` | the tail epoch, 8 bytes | for ever (written only with [`Members::with_handoff`]) |
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
//! **Node facts.** The row also keeps what the node said of the build it runs and of its
//! metadata log: the newest log version its build runs (informational), the version the log is at,
//! and the commit built ([`NodeFacts`]; row tags 7 to 9, [`Members::node_facts`], `GET
//! /v1/members`). A registration replaces them, a heartbeat updates the log version and keeps
//! the rest, and a node that says nothing reads as `None`, never 0. Nothing decides on them
//! yet. A frontend built before them drops them when it rewrites a row, and they return with
//! the node's next heartbeat to a frontend that knows them (module `facts`).
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
//! **Joiners.** A node the farm has not seen before, registering after the farm's first
//! bootstrap minute, is `Joining` with an `unfilled/<id>` mark and is promoted by
//! [`Members::set_state`] (module `join`), or, with [`Members::with_handoff`], by its own
//! beat once it is filled (modules `fence` and `promote`); the paragraph below is for the
//! others.
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
use kbf_mapsync::{MapSource, RETAINED_MAPS};
use kbf_proto::kbf::map::v1 as pb;
use kbf_state::{Cond, Store, StoreError, StoreTime, Txn, Versioned};
use prost::Message as _;
use tokio::sync::watch;

use crate::storage_health::{
    MemberReport, NotIdle, Planned, Report, Reported, StorageHealth, node_idle, storage_health,
};
use crate::worker::named_protocol;

mod denials;
pub use denials::Denial;
mod assign;
mod blocked;
mod facts;
mod fence;
mod handoff;
mod join;
mod pins;
mod promote;
mod row;
pub use assign::{LastBoot, MAX_SEED};
pub use facts::{MAX_BUILD_COMMIT, NodeFacts};
pub use handoff::{Block, Fill, Handoff, fill_fp};
use pins::pin_key;
use row::{Live, decode_row, encode_row};
mod look;
pub use look::{MemberLook, MembersLook};

/// The value of a `down/<id>` mark written by an operator's planned restart
/// ([`Members::plan_down`]); the sweeper's marks are empty.
const PLANNED: &[u8] = b"planned";
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
    /// the store's verdict, or an operator's planned restart ([`Members::plan_down`]); a
    /// state this build does not name is what a later build wrote, never set here.
    #[error("a member cannot be set {0:?}")]
    NotSettable(MemberState),
    /// Only an `Active` member can be planned down.
    #[error("member {id} is {state:?}: only an Active member can be planned down")]
    NotActive {
        /// The member.
        id: MemberId,
        /// Its state.
        state: MemberState,
    },
    /// The pins are at the cap: a new join or drain is refused until one lapses.
    #[error(
        "{held} maps from before recent changes are pinned (the cap is {cap}): try again after the dwell"
    )]
    PinsFull {
        /// How many pins the store holds.
        held: usize,
        /// The cap.
        cap: usize,
    },
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

/// The claim on a member id: `member-owner/<id>` holds the instance that has it, written
/// with the instance's own row so no two instances are ever given one id.
fn owner_key(id: MemberId) -> String {
    format!("member-owner/{}", id.0)
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
        current.map_or(Rule::Rendezvous, Map::rule),
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
enum Touch<'a> {
    /// A heartbeat of this boot, what it reports, what it says it owes, what it says of
    /// the handoff and what it says of its build and log: a whole time to live more, and the
    /// debts report replaced ([`NodeFacts::after_beat`] says what becomes of the facts).
    Beat(u64, Progress, Reported, Fill, &'a NodeFacts),
    /// An administrator's state change: the time to live it had.
    State(MemberState),
    /// An administrator's planned restart of an `Active` member: `Down` in the row, with
    /// the dwell mark, in one transaction.
    PlanDown(Duration),
    /// An administrator's undoing of a planned restart: `Active` again, only for a member
    /// that is `Down` on a live row; any other state is left as it is.
    UndoPlan,
}

/// The maps a process has seen: the newest, and the generations before it.
#[derive(Default)]
struct Seen {
    latest: Option<Arc<Map>>,
    /// Newest first, all older than `latest`, at most [`RETAINED_MAPS`].
    history: Vec<Arc<Map>>,
}

/// Membership over a state store.
pub struct Members {
    store: Arc<dyn Store>,
    config: MembershipConfig,
    /// The newest map this process has seen, whoever bumped it, and those before it.
    seen: Mutex<Seen>,
    /// Its generation (0 before any): [`MapSource::changes`].
    generation: watch::Sender<u64>,
    /// The oldest protocol this build accepts: the least floor of any map it makes.
    oldest: u32,
    /// Automatic promotion of joiners ([`Members::with_handoff`]); `None`: by an operator only.
    handoff: Option<Handoff>,
    /// The `deny/` rows the newest refresh that could read them found, and when (on the
    /// process clock): what the worker listener checks callers against without a store
    /// read of its own ([`Members::denied_snapshot`]).
    denied_seen: Mutex<Option<(Arc<Vec<DenyEntry>>, tokio::time::Instant)>>,
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
            handoff: None,
            denied_seen: Mutex::new(None),
        }
    }

    /// These members with automatic promotion of joiners on ([`Handoff`]; `--auto-promote`):
    /// a joiner is made `Active` by its own beat when it is filled (module `promote`), and
    /// [`Members::refresh`] runs the fence that says so (module `fence`). An operator's
    /// `set_state(Active)` stays the override.
    #[must_use]
    pub fn with_handoff(mut self, handoff: Handoff) -> Self {
        self.handoff = Some(handoff);
        self
    }

    /// Notes `map` as seen: if it is newer than the last, keeps it (and the one it
    /// replaced) and wakes the [`MapSource::changes`] receivers. `older` are maps the
    /// store holds from before `map` (the pins, or the map a bump replaced): those this
    /// process does not have join its history, which stays newest first and at most
    /// [`RETAINED_MAPS`] long.
    fn publish(&self, map: &Map, older: Vec<Map>) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen
            .latest
            .as_ref()
            .is_none_or(|l| l.generation() < map.generation())
        {
            if let Some(replaced) = seen.latest.replace(Arc::new(map.clone())) {
                seen.history.insert(0, replaced);
            }
            self.generation.send_replace(map.generation());
        }
        let newest = seen.latest.as_ref().map_or(0, |l| l.generation());
        for map in older {
            let generation = map.generation();
            if generation < newest && seen.history.iter().all(|m| m.generation() != generation) {
                seen.history.push(Arc::new(map));
            }
        }
        seen.history
            .sort_by_key(|m| std::cmp::Reverse(m.generation()));
        seen.history.truncate(RETAINED_MAPS);
    }

    /// The newest map this process has seen and the generations before it, newest first
    /// (a blob written before a change is found in them), at most [`RETAINED_MAPS`]. A
    /// restarted process has the maps the pins hold after its first read of the store
    /// ([`Members::current_map`]). `None` before any map. Reads no store:
    /// [`Members::current_map`] does, and publishes what it finds.
    #[must_use]
    pub fn view(&self) -> Option<(Arc<Map>, Vec<Arc<Map>>)> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let latest = seen.latest.clone()?;
        Some((latest, seen.history.clone()))
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

    /// The deny list as the newest [`Members::refresh`] that could read the `deny/` rows
    /// found it, and when that was: no store read. `None` before the first such refresh. A
    /// refresh that fails to read the rows (the store failing, or a corrupt `deny/` row)
    /// leaves the earlier snapshot, which therefore only ages.
    #[must_use]
    pub fn denied_snapshot(&self) -> Option<(Arc<Vec<DenyEntry>>, tokio::time::Instant)> {
        self.denied_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The settings of automatic promotion, if it is on ([`Members::with_handoff`]).
    #[must_use]
    pub fn handoff(&self) -> Option<Handoff> {
        self.handoff
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &MembershipConfig {
        &self.config
    }

    /// Registers a storage node: assigns its id (the same for the same instance) and a
    /// larger boot counter, writes its row as `Active` (`CatchingUp` if it is rebuilding
    /// its index, `Joining` while its `unfilled/` mark is there: see the module docs and
    /// `join`), and rebuilds the map.
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
    /// [`MembershipError::Contended`]; [`MembershipError::PinsFull`] for a new instance that
    /// would join a farm holding data while the pins are at the cap (it gets no identity).
    pub fn register(&self, who: &Registrant) -> Result<Registration, MembershipError> {
        self.register_after(who, None)
    }

    /// [`Members::register`] for a node that says which member and boot its metadata log
    /// last wrote as (`last`; `None` for a node that has no log history or an older node
    /// that does not say). The store assigns numbers the log will accept: see `assign`.
    ///
    /// # Errors
    /// As [`Members::register`].
    pub fn register_after(
        &self,
        who: &Registrant,
        last: Option<LastBoot>,
    ) -> Result<Registration, MembershipError> {
        self.register_reporting(who, last, &NodeFacts::default())
    }

    /// [`Members::register_after`] for a node that also says what it runs and what its
    /// metadata log is at (`facts`): the row keeps them, in place of any an earlier boot
    /// wrote ([`NodeFacts`]).
    ///
    /// # Errors
    /// As [`Members::register`].
    pub fn register_reporting(
        &self,
        who: &Registrant,
        last: Option<LastBoot>,
        facts: &NodeFacts,
    ) -> Result<Registration, MembershipError> {
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
        // A new instance on a farm that holds data is a joiner ([`Members::assign`]): refused,
        // before it gets an identity or a mark, when the pins are full.
        if self.store.get(&instance_key(&who.instance_id))?.is_none() && self.holds_data()? {
            self.pins_full()?;
        }
        let (id, boot) = self.assign(&who.instance_id, last)?;
        let member = Member {
            id,
            // The spelling enrollment stores, so the update look can join a host to it.
            addr: kbf_enroll_registry::canonical_addr(&who.addr),
            zone: who.zone.clone(),
            weight: who.weight,
            state: MemberState::Active,
            // Published in the map, so a restart inside the lease changes the member (and the
            // generation) even when nothing else about it did.
            boot,
        };
        self.write_row(
            member,
            who.progress == Progress::CatchingUp,
            boot,
            named_protocol(who.protocol),
            facts,
        )?;
        let map = self.refresh()?.ok_or(MembershipError::NoMap)?;
        Ok(Registration { id, boot, map })
    }

    /// Stores `member`'s row with a whole time to live, unless a newer boot is there, and
    /// clears its `down/` mark. The row names the `protocol` and the `facts` this
    /// registration did, in place of any an earlier boot wrote. The state is
    /// [`Members::admit`]'s (read again at every attempt, and the write checks the mark it
    /// read); an `Active` row also writes `meta/members_formed`, in the same transaction.
    fn write_row(
        &self,
        mut member: Member,
        rebuilding: bool,
        boot: u64,
        protocol: Option<u32>,
        facts: &NodeFacts,
    ) -> Result<(), MembershipError> {
        let key = member_key(member.id);
        for _ in 0..ATTEMPTS {
            let (state, mark) = self.admit(member.id, rebuilding)?;
            member.state = state;
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
            let mut txn = Txn::new().check(join::unfilled_key(member.id), mark);
            if member.state == MemberState::Active {
                txn = self.formed(txn)?;
            }
            let txn = txn
                .put_ttl(
                    &key,
                    encode_row(&member, boot, protocol, None, None, Fill::default(), facts),
                    self.config.ttl,
                    cond,
                )
                .delete(down_key(member.id), Cond::Any);
            if self.store.txn(txn)?.is_ok() {
                if member.state == MemberState::Joining {
                    tracing::warn!(
                        member = member.id.0,
                        addr = %member.addr,
                        "a joiner: it is not primary until promoted by the operator (POST /v1/members, state active)"
                    );
                }
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
        said: Reported,
        holder: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<u64, MembershipError> {
        self.heartbeat_with(id, boot, progress, said, Fill::default(), holder)
    }

    /// [`Members::heartbeat`] for a node that also says how far its flush and its tail have
    /// got (`fill`). A joiner's beat that finds it filled promotes it ([`Handoff`]). The beat
    /// says nothing of the node's build or log, as an older node's does not: the row's log
    /// version becomes `None` ([`Members::heartbeat_reporting`] says it).
    ///
    /// # Errors
    /// As [`Members::heartbeat`].
    pub fn heartbeat_with(
        &self,
        id: MemberId,
        boot: u64,
        progress: Progress,
        said: Reported,
        fill: Fill,
        holder: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<u64, MembershipError> {
        let said_nothing = NodeFacts::default();
        self.heartbeat_reporting(id, boot, progress, said, (fill, &said_nothing), holder)
    }

    /// [`Members::heartbeat_with`] for a node that also says what it runs and what its
    /// metadata log is at now (`facts`, with `fill`): the row takes the log version, and
    /// the other two facts if it holds none ([`NodeFacts`]).
    ///
    /// # Errors
    /// As [`Members::heartbeat`].
    pub fn heartbeat_reporting(
        &self,
        id: MemberId,
        boot: u64,
        progress: Progress,
        said: Reported,
        (fill, facts): (Fill, &NodeFacts),
        holder: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<u64, MembershipError> {
        let beat = Touch::Beat(boot, progress, said, fill, facts);
        if self.touch(id, beat, holder)? {
            return Ok(self.refresh()?.map_or(0, |map| map.generation()));
        }
        Ok(self.current_map()?.map_or(0, |map| map.generation()))
    }

    /// Sets a live member's state (the promotion hook: `Joining`, `CatchingUp`, `Active`,
    /// `Draining`) and rebuilds the map. The row keeps the time to live it had.
    ///
    /// # Errors
    /// [`MembershipError::NotSettable`] for `Down` and for a state this build does not name
    /// ([`MemberState::Unknown`]); [`MembershipError::NotRegistered`] if the member has no
    /// live row; the store failing.
    pub fn set_state(&self, id: MemberId, state: MemberState) -> Result<Map, MembershipError> {
        if matches!(state, MemberState::Down | MemberState::Unknown(_)) {
            return Err(MembershipError::NotSettable(state));
        }
        if state == MemberState::Draining {
            let draining = self
                .store
                .get(&member_key(id))?
                .map(|stored| decode_row(&stored))
                .transpose()?
                .is_some_and(|live| live.member.state == MemberState::Draining);
            // As for a join, only a farm that holds data has a handoff to bound.
            if !draining && self.holds_data()? {
                self.pins_full()?;
            }
        }
        self.touch(id, Touch::State(state), None)?;
        self.refresh()?.ok_or(MembershipError::NoMap)
    }

    /// Takes a live `Active` member out of the write path for a planned restart of at most
    /// `for_`, and rebuilds the map: it is `Down` in the map at once, so every router, chain
    /// and repair worker that holds the new map skips it for writes and reads its blobs from
    /// the other owner, and a map an older build holds reads it like any `Down` member. The
    /// `down/` mark that holds its slot for the dwell is written in the same transaction as
    /// the row, with a value that says an operator planned it ([`Members::planned`]).
    ///
    /// The member keeps heartbeating until it stops and a heartbeat does not undo this, until
    /// `for_` has passed: the first heartbeat after that returns it to `Active` through the
    /// way a promotion goes (a controller that died, or a restart that never came, does not
    /// strand a healthy node). It is undone sooner by [`Members::set_state`] to `Active`, or
    /// by the member registering again (the restart did happen: `Active`, or `CatchingUp`
    /// while it rebuilds, the mark deleted); if it stops and does not return, its row
    /// expires and the dwell, counted from this call, decides when it leaves the map.
    ///
    /// **Mixed frontends.** The end of the plan lives in the member's row (tag 5), which a
    /// frontend built before it does not know: it keeps the state `Down` when it rewrites the
    /// row (a heartbeat, a promotion) but drops the end, so the plan would never run out.
    /// Plan nothing down while a frontend of an older build serves the same state store.
    ///
    /// Asking again for a member already planned down is the same planned restart with its
    /// end moved to `for_` from now: no new generation.
    ///
    /// # Errors
    /// [`MembershipError::NotRegistered`] if the member has no live row;
    /// [`MembershipError::NotActive`] unless it is `Active` or already planned down; the
    /// store failing.
    pub fn plan_down(&self, id: MemberId, for_: Duration) -> Result<Map, MembershipError> {
        self.touch(id, Touch::PlanDown(for_), None)?;
        self.refresh()?.ok_or(MembershipError::NoMap)
    }

    /// Ends a planned restart of `id` at once: a live member that is `Down` on its row (only a
    /// plan makes that) is `Active` again, its `down/` mark deleted, and the map rebuilt.
    /// Whether it changed anything: a member that is not planned down (it returned by its own
    /// heartbeat or by registering again, or is `CatchingUp` or `Draining`) is left as it is,
    /// so the undoing of a restart that happened never promotes a node that is rebuilding.
    ///
    /// # Errors
    /// [`MembershipError::NotRegistered`] if the member has no live row (it stopped: its mark
    /// is deleted when it registers again, or expires with the dwell); the store failing.
    pub fn undo_plan(&self, id: MemberId) -> Result<bool, MembershipError> {
        let changed = self.touch(id, Touch::UndoPlan, None)?;
        if changed {
            self.refresh()?;
        }
        Ok(changed)
    }

    /// The planned restart of `id`, if there is one: see [`Planned`].
    ///
    /// # Errors
    /// The store failing; a stored row not decoding.
    pub fn planned(&self, id: MemberId) -> Result<Planned, MembershipError> {
        let row = match self.store.get(&member_key(id))? {
            Some(stored) => Some(decode_row(&stored)?),
            None => None,
        };
        self.planned_of(id, row.as_ref())
    }

    /// Whether `id` is `Down` because an operator planned it and the plan has not run out.
    ///
    /// # Errors
    /// The store failing; a stored row not decoding.
    pub fn planned_down(&self, id: MemberId) -> Result<bool, MembershipError> {
        Ok(self.planned(id)?.in_force(self.store.now()))
    }

    /// `row`'s plan, or the mark's when the member has no live row (it stopped).
    fn planned_of(&self, id: MemberId, row: Option<&Live>) -> Result<Planned, MembershipError> {
        // A live row carries its own plan, or none; the mark speaks for a member that stopped.
        if let Some(live) = row {
            return Ok(live.planned_until.map_or(Planned::No, Planned::Until));
        }
        let marked = self
            .store
            .get(&down_key(id))?
            .is_some_and(|mark| mark.value == PLANNED);
        Ok(if marked {
            Planned::Stopped
        } else {
            Planned::No
        })
    }

    /// Rewrites `id`'s row as `how` says; whether its state changed. `holder` is
    /// [`Members::heartbeat`]'s.
    fn touch(
        &self,
        id: MemberId,
        how: Touch<'_>,
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
            let mut step = promote::Step::none();
            let ttl = match how {
                Touch::Beat(boot, _, _, _, _) if boot != live.boot => {
                    return Err(MembershipError::Superseded {
                        id,
                        boot,
                        current: live.boot,
                    });
                }
                Touch::Beat(_, progress, said, fill, facts) => {
                    live.report = Some(Report {
                        said,
                        at: self.store.now(),
                    });
                    live.fill = fill;
                    live.facts = live.facts.after_beat(facts);
                    if before == MemberState::Joining {
                        step = self.beat_step(&live, progress)?;
                        if step.promote {
                            live.member.state = MemberState::Active;
                        }
                    }
                    if before == MemberState::CatchingUp && progress == Progress::CaughtUp {
                        live.member.state = MemberState::Active;
                    }
                    // A planned restart that ran out: the member is still beating, so it
                    // never restarted (or the operator's controller is gone); it returns
                    // by the way a promotion goes, not by waiting to be noticed.
                    if before == MemberState::Down
                        && live
                            .planned_until
                            .is_some_and(|until| !Planned::Until(until).in_force(self.store.now()))
                    {
                        live.member.state = MemberState::Active;
                        live.planned_until = None;
                    }
                    self.config.ttl
                }
                Touch::State(state) => {
                    live.member.state = state;
                    live.planned_until = None;
                    stored
                        .expires_at
                        .map_or(self.config.ttl, |at| at.since(self.store.now()))
                }
                // `Down` on a live row is a plan (nothing else makes one), so a repeat is
                // taken; so is one whose end an older frontend dropped.
                Touch::PlanDown(_)
                    if before != MemberState::Active && before != MemberState::Down =>
                {
                    return Err(MembershipError::NotActive { id, state: before });
                }
                // Asked again (or by a second controller): the same planned restart, its
                // end moved to a whole `for_` from now.
                // Not planned down (it returned by itself, or never went): nothing to undo, and
                // a member that is rebuilding or draining is not promoted by this.
                Touch::UndoPlan if before != MemberState::Down => return Ok(false),
                Touch::UndoPlan => {
                    live.member.state = MemberState::Active;
                    live.planned_until = None;
                    stored
                        .expires_at
                        .map_or(self.config.ttl, |at| at.since(self.store.now()))
                }
                Touch::PlanDown(for_) => {
                    live.member.state = MemberState::Down;
                    live.planned_until = Some(self.store.now().saturating_add(for_));
                    stored
                        .expires_at
                        .map_or(self.config.ttl, |at| at.since(self.store.now()))
                }
            };
            let mut put = Txn::new().put_ttl(
                &key,
                encode_row(
                    &live.member,
                    live.boot,
                    live.protocol,
                    live.report.as_ref(),
                    live.planned_until,
                    live.fill,
                    &live.facts,
                ),
                ttl,
                Cond::Version(stored.version),
            );
            put = step.apply(put, id);
            if live.member.state == MemberState::Active && before != MemberState::Active {
                put = self.formed(put)?;
            }
            // Every time an operator's call leaves the member `Active`, not only on the
            // transition: a mark a frontend that predates it left is cleaned by the next.
            if matches!(how, Touch::State(_)) && live.member.state == MemberState::Active {
                put = put.delete(join::unfilled_key(id), Cond::Any);
            }
            if matches!(how, Touch::PlanDown(_)) {
                put = put.put_ttl(down_key(id), PLANNED, self.config.dwell, Cond::Any);
            } else if before == MemberState::Down && live.member.state != MemberState::Down {
                put = put.delete(down_key(id), Cond::Any);
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
        if let Some(m) = &map {
            self.publish(m, self.pins()?);
        }
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
    /// added or removed is a change). With [`Members::with_handoff`] it then moves the
    /// marks of the joiners along ([`Members::advance`]): a failure of that is logged, never
    /// returned, since the map is what the caller asked for and the next call tries again.
    ///
    /// # Errors
    /// The store failing; [`MembershipError::Contended`].
    pub fn refresh(&self) -> Result<Option<Map>, MembershipError> {
        let map = self.refresh_map()?;
        if let Err(error) = self.advance() {
            tracing::warn!(%error, "advancing the joiners' handoff failed; tried again at the next refresh");
        }
        Ok(map)
    }

    /// The map's part of [`Members::refresh`].
    fn refresh_map(&self) -> Result<Option<Map>, MembershipError> {
        for _ in 0..ATTEMPTS {
            let stored = self.store.get(MAP_KEY)?;
            let current = stored.as_ref().map(decode_map).transpose()?;
            let rows = self.read_rows()?;
            let marks = self.read_marks(current.as_ref())?;
            // A deny written after this read is in the next refresh's map (the sweeper's,
            // within a beat): a row that is not there yet cannot be a precondition.
            let denied = self.denied()?;
            *self
                .denied_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some((Arc::new(denied.clone()), tokio::time::Instant::now()));
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
                if let Some(m) = &current {
                    self.publish(m, self.pins()?);
                }
                return Ok(current);
            };
            let cond = stored
                .as_ref()
                .map_or(Cond::Absent, |s| Cond::Version(s.version));
            let wire = pb::Map::from(&plan.map).encode_to_vec();
            let mut txn = Txn::new().put(MAP_KEY, wire, cond);
            // The map this bump replaces is pinned in the same transaction, for the dwell,
            // and the pin that falls out of the cap goes with it.
            if let Some(replaced) = stored.as_ref().zip(current.as_ref()) {
                let generation = replaced.1.generation();
                txn = txn.put_ttl(
                    pin_key(generation),
                    replaced.0.value.clone(),
                    self.config.dwell,
                    Cond::Any,
                );
                if let Some(gone) = generation.checked_sub(RETAINED_MAPS as u64) {
                    txn = txn.delete(pin_key(gone), Cond::Any);
                }
            }
            for id in &plan.newly_down {
                txn = txn.put_ttl(down_key(*id), Vec::new(), self.config.dwell, Cond::Any);
            }
            for (id, cond) in &plan.guards {
                txn = txn.check(member_key(*id), *cond);
            }
            if self.store.txn(txn)?.is_ok() {
                self.publish(&plan.map, current.into_iter().collect());
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
        let (members, now) = self.member_reports()?;
        Ok(storage_health(&members, now, self.config.freshness()))
    }

    /// Whether storage node `node` may be stopped now ([`node_idle`]): `at_generation` is
    /// the map that stopped routing writes to it, which its last report must have seen.
    ///
    /// # Errors
    /// [`NotIdle`] naming the first reason; a membership that cannot be read is one.
    pub fn node_idle(&self, node: MemberId, at_generation: u64) -> Result<(), NotIdle> {
        let (members, now) = self
            .member_reports()
            .map_err(|e| NotIdle::Unreadable(e.to_string()))?;
        node_idle(&members, now, self.config.freshness(), node, at_generation)
            .into_iter()
            .next()
            .map_or(Ok(()), Err)
    }

    /// The members as the judgements read them, and the store's clock after the reads, so
    /// a report is judged no fresher than the clock then was.
    fn member_reports(&self) -> Result<(Vec<MemberReport>, StoreTime), MembershipError> {
        let mut members: BTreeMap<MemberId, MemberReport> = BTreeMap::new();
        for member in self.current_map()?.as_ref().map_or(&[][..], Map::members) {
            members.insert(
                member.id,
                MemberReport {
                    id: member.id,
                    state: member.state,
                    boot: member.boot,
                    planned: Planned::No,
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
                    boot: live.boot,
                    planned: Planned::No,
                    report: live.report,
                },
            );
        }
        let rows = self.read_rows()?;
        for member in members.values_mut() {
            if member.state == MemberState::Down {
                member.planned = self.planned_of(member.id, rows.get(&member.id))?;
            }
        }
        let now = self.store.now();
        Ok((members.into_values().collect(), now))
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

    /// What each member with a live row said of the build it runs and of its metadata log
    /// ([`NodeFacts`]; a fact the node did not say is `None`, never 0). A member the map
    /// holds whose row has lapsed (`Down`) is not here: its row, which held the facts,
    /// expired with the heartbeats, so a reader treats it as not known.
    ///
    /// # Errors
    /// The store failing; a stored row not decoding.
    pub fn node_facts(&self) -> Result<BTreeMap<MemberId, NodeFacts>, MembershipError> {
        Ok(self
            .read_rows()?
            .into_iter()
            .map(|(id, live)| (id, live.facts))
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
