//! The stored form of a member row (`member/<id>`): what [`super::Members`] writes at a
//! registration, a heartbeat and an operator's call, and reads back.

use std::time::Duration;

use kbf_map::Member;
use kbf_proto::kbf::map::v1 as pb;
use kbf_state::{StoreTime, Version, Versioned};
use prost::Message as _;

use super::MembershipError;
use super::facts::{NodeFacts, capped};
use super::handoff::Fill;
use crate::storage_health::{Debts, Report, Reported, Writes};
use crate::vitals::{StoredVitals, Vitals};
use crate::worker::named_protocol;

/// A stored member row: the member as the map will hold it, its boot counter, the newest
/// debts report and the protocol version its node registered with.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct Row {
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
    /// The store's clock, in nanoseconds, at which an operator's planned restart of this
    /// member ends (0: none). Past it, a heartbeat of the member returns it to `Active`.
    /// A frontend built before this field existed drops it when it rewrites the row, so
    /// a plan outlives it there: no plan while one serves the same store.
    #[prost(uint64, tag = "5")]
    planned_until_nanos: u64,
    /// What the newest heartbeat said of the handoff (`docs/design/rebalance-handoff.md`):
    /// absent from a row written by a registration, and by a frontend built before it.
    #[prost(message, optional, tag = "6")]
    fill: Option<StoredFill>,
    /// What the node said of the build it runs and its metadata log ([`NodeFacts`]): tags 7
    /// to 9, each with presence, so a row that names none (written before the tags existed,
    /// or by a node that says none of it) reads `None`, never 0. A frontend built before
    /// them drops them when it rewrites the row; the node's next heartbeat to a frontend
    /// that knows them brings them back (`facts.rs`).
    #[prost(uint32, optional, tag = "7")]
    runs_log: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    log_version: Option<u32>,
    #[prost(string, optional, tag = "9")]
    build_commit: Option<String>,
}

/// [`Fill`] as stored.
#[derive(Clone, Copy, PartialEq, prost::Message)]
pub(super) struct StoredFill {
    #[prost(bool, tag = "1")]
    flush_known: bool,
    #[prost(uint64, tag = "2")]
    flush_clear_gen: u64,
    #[prost(uint64, tag = "3")]
    tail_epoch_done: u64,
    #[prost(uint64, tag = "4")]
    fill_fp: u64,
}

impl From<Fill> for StoredFill {
    fn from(fill: Fill) -> Self {
        let (flush_known, flush_clear_gen) = fill.flush_to_wire();
        Self {
            flush_known,
            flush_clear_gen,
            tail_epoch_done: fill.tail_epoch_done,
            fill_fp: fill.fill_fp,
        }
    }
}

impl From<StoredFill> for Fill {
    fn from(stored: StoredFill) -> Self {
        Self::from_wire(
            stored.flush_known,
            stored.flush_clear_gen,
            stored.tail_epoch_done,
            stored.fill_fp,
        )
    }
}

/// [`Report`] as stored.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct StoredReport {
    #[prost(bool, tag = "1")]
    known: bool,
    #[prost(uint64, tag = "2")]
    debts: u64,
    /// The store's clock at receipt, in nanoseconds.
    #[prost(uint64, tag = "3")]
    at_nanos: u64,
    #[prost(bool, tag = "4")]
    writes_known: bool,
    #[prost(uint64, tag = "5")]
    open_writes: u64,
    /// The map generation the node held when it reported.
    #[prost(uint64, tag = "6")]
    generation: u64,
    /// The write tier's health; absent from a row written before the field existed, and
    /// from a node that sent none.
    #[prost(message, optional, tag = "7")]
    vitals: Option<StoredVitals>,
}

impl From<&Report> for StoredReport {
    fn from(report: &Report) -> Self {
        let (known, debts) = report.said.debts.to_wire();
        let (writes_known, open_writes) = report.said.writes.to_wire();
        Self {
            known,
            debts,
            at_nanos: u64::try_from(report.at.as_duration().as_nanos()).unwrap_or(u64::MAX),
            writes_known,
            open_writes,
            generation: report.said.generation,
            vitals: (report.said.vitals != Vitals::NONE).then(|| report.said.vitals.into()),
        }
    }
}

impl From<&StoredReport> for Report {
    fn from(stored: &StoredReport) -> Self {
        Self {
            said: Reported {
                debts: Debts::from_wire(stored.known, stored.debts),
                writes: Writes::from_wire(stored.writes_known, stored.open_writes),
                generation: stored.generation,
                vitals: stored.vitals.map_or(Vitals::NONE, Vitals::from),
            },
            at: StoreTime::from_duration(Duration::from_nanos(stored.at_nanos)),
        }
    }
}

/// A decoded row and the version it was read at.
pub(super) struct Live {
    pub(super) member: Member,
    pub(super) boot: u64,
    pub(super) report: Option<Report>,
    /// What its registration named; `None` for a row that names none.
    pub(super) protocol: Option<u32>,
    pub(super) planned_until: Option<StoreTime>,
    /// What its newest heartbeat said of the handoff ([`Fill::default`]: nothing).
    pub(super) fill: Fill,
    /// What its registration and heartbeats said of its build and log ([`NodeFacts`]); all
    /// `None` for a row that names none.
    pub(super) facts: NodeFacts,
    pub(super) version: Version,
}

/// The row a stored value holds. A state in it that this build does not name reads as
/// [`kbf_map::MemberState::Unknown`] with the number it came with, so the row reads, the
/// member is neither given work nor counted as active, and a row written back unchanged keeps
/// the number.
///
/// # Errors
/// [`MembershipError::Corrupt`] if the value is not a row message or holds no member.
pub(super) fn decode_row(stored: &Versioned) -> Result<Live, MembershipError> {
    let row = Row::decode(stored.value.as_slice())
        .map_err(|e| MembershipError::Corrupt(format!("member row: {e}")))?;
    let wire = row
        .member
        .ok_or_else(|| MembershipError::Corrupt("member row: no member".to_owned()))?;
    let mut member = Member::from(wire);
    // The row's counter is the authority (a row written before the map named boots has
    // none inside the member).
    member.boot = row.boot;
    Ok(Live {
        member,
        boot: row.boot,
        report: row.report.as_ref().map(Report::from),
        protocol: named_protocol(row.protocol),
        planned_until: (row.planned_until_nanos > 0)
            .then(|| StoreTime::from_duration(Duration::from_nanos(row.planned_until_nanos))),
        fill: row.fill.map_or_else(Fill::default, Fill::from),
        facts: NodeFacts {
            runs_log: row.runs_log,
            log_version: row.log_version,
            build_commit: row.build_commit,
        },
        version: stored.version,
    })
}

/// The value of the row of `member` at `boot`, for a node that registered naming
/// `protocol`, with `report` as its newest debts report and `planned_until` the end of an
/// operator's planned restart, `fill` what its newest heartbeat said of the handoff and
/// `facts` what the node said of its build and log (its `build_commit` cut to
/// [`super::facts::MAX_BUILD_COMMIT`] bytes).
pub(super) fn encode_row(
    member: &Member,
    boot: u64,
    protocol: Option<u32>,
    report: Option<&Report>,
    planned_until: Option<StoreTime>,
    fill: Fill,
    facts: &NodeFacts,
) -> Vec<u8> {
    Row {
        member: Some(pb::Member::from(member)),
        boot,
        report: report.map(StoredReport::from),
        protocol: protocol.unwrap_or(0),
        planned_until_nanos: planned_until.map_or(0, |at| {
            u64::try_from(at.as_duration().as_nanos())
                .unwrap_or(u64::MAX)
                .max(1)
        }),
        // A row that has heard nothing stores nothing, as before the field existed.
        fill: (fill != Fill::default()).then(|| StoredFill::from(fill)),
        runs_log: facts.runs_log,
        log_version: facts.log_version,
        build_commit: facts.build_commit.as_deref().map(capped),
    }
    .encode_to_vec()
}
