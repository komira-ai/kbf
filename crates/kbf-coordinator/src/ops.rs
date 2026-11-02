//! The ops store: what operators and the coordinator itself decide, kept apart from CI's
//! desired state (`docs/design/host-updates.md`, sections 3.8 and 5).
//!
//! It holds the hosts an operator excluded from a service, the labels an operator put on a
//! host, the per-service marks later changes fill (a halt, the last good version, the bad
//! versions), and a ring of the last changes made. It is one JSON file, `--ops-file`, kept
//! apart from `desired.json` so that an earlier coordinator, which refuses a key it does not
//! know there, still reads the desired state. The file is created by the first change (a
//! farm that never uses it keeps its files as they are) and replaced whole after that, like
//! the desired state: a temporary file, fsync, rename and a directory fsync. What the halt
//! release added to a halt and its acknowledgements is kept in a second file beside it, so that
//! the release before still reads this one ([`crate::ops_file`]).
//!
//! The reader is strict. A key it does not know is a refusal to start, and so is a `schema`
//! newer than this build: a coordinator rolled back below the build that wrote a halt would
//! otherwise ignore the halt and go on rolling. The operator moves the file aside on
//! purpose.
//!
//! A change reaches the file in one of two orders ([`Write`]). A change that *raises*
//! protection (a halt) is in memory first and written after, so a failed write never leaves
//! it unraised. Every other change, a clearing one included, is written first and enters
//! memory only if the file holds it, so a failed write never leaves it cleared.
//!
//! Every change carries an [`AuditEntry`]: the store numbers it, keeps the last
//! [`AUDIT_RING`] in the file and logs it on the target `kbf_coordinator::audit`.
//!
//! The caps on the counts (`MAX_HOSTS`, `MAX_EXCLUDED`) are checked under the same lock as the
//! write ([`OpsStore::try_change`]), on the state the change is applied to.
//!
//! The operator actions ([`operate`]) write exclusions, labels and the clear of a halt; the
//! coordinator itself writes the raising of a halt ([`crate::halt`]) and, in
//! [`crate::reconcile`], the last good version and the marking of a version bad (a raise, so in
//! memory first); CI's override clears that mark when it sets that version by hand. The
//! decision reads a halt and the bad versions ([`crate::halt::decide`]); nothing reads the rest
//! when it decides what a host is told.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::UpdateResult;
use serde::{Deserialize, Serialize};

use crate::desired::StoreError;
use crate::ops_file;
use crate::service::Unauthorized;
use crate::services::Services;

/// The schema of the file this build writes and the newest it reads.
pub const SCHEMA: u32 = 1;

/// How many changes the file remembers.
pub const AUDIT_RING: usize = 64;

/// Who an operator change is logged as: the holder of the operator token.
pub const OPERATOR: &str = "operator";

/// The most hosts with labels the file holds. Raise it here if a farm needs more.
pub const MAX_HOSTS: usize = 4096;

/// The most exclusions the file holds. Raise it here if a farm needs more.
pub const MAX_EXCLUDED: usize = 4096;

/// The most labels one host has.
pub const MAX_LABELS: usize = 8;

/// The most bytes a label's key or value, in `[a-z0-9-]`, has.
pub const MAX_LABEL_BYTES: usize = 32;

/// The most bytes of a reason.
pub const MAX_REASON_BYTES: usize = 256;

/// The labels of one host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOps {
    /// Label key to value, set by an operator only.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// A host an operator took out of a service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusion {
    /// The service.
    pub service: String,
    /// The node id.
    pub node_id: String,
    /// Why, in the operator's words.
    pub reason: String,
}

/// A halt of a service: raised by a host's report ([`crate::halt`]), cleared by the operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Halt {
    /// The host that reported it.
    pub node_id: String,
    /// The check that failed.
    pub check: String,
    /// The version it was updating to.
    pub target: Version,
    /// The `last_update` it reported, whole: what an acknowledgement of it matches. `None` in
    /// a file written before reports were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<UpdateResult>,
}

/// A host and the halt report an operator cleared. It matches that report, whole: the same
/// host reporting another `last_update`, of the same target too, is a different report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaltAck {
    /// The host.
    pub node_id: String,
    /// The version.
    pub target: Version,
    /// The `last_update` the host reported. `None` in a file written before reports were kept:
    /// such an acknowledgement matches no report, and ends on the host's first beat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<UpdateResult>,
}

impl HaltAck {
    /// Whether this acknowledges `report` of the host `node`: the same host and the very same
    /// `last_update`.
    #[must_use]
    pub fn matches(&self, node: &str, report: &UpdateResult) -> bool {
        self.node_id == node && self.report.as_ref() == Some(report)
    }
}

impl From<Halt> for HaltAck {
    fn from(halt: Halt) -> Self {
        Self {
            node_id: halt.node_id,
            target: halt.target,
            report: halt.report,
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// A failure report an operator override acknowledged: the host and the whole report it made,
/// so that a different report of the same host is not hidden by it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureAck {
    /// The host.
    pub node_id: String,
    /// The `last_update` it reported.
    pub report: UpdateResult,
}

/// The marks of one service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOps {
    /// The standing halt, if any.
    #[serde(default)]
    pub halt: Option<Halt>,
    /// The halt reports an operator cleared: one per host, the latest.
    #[serde(default)]
    pub halt_acked: Vec<HaltAck>,
    /// The other halt reports received while the standing halt stood: one per host, the
    /// latest. A clear acknowledges them with the halt's own. Empty unless a halt stands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub halt_seen: Vec<HaltAck>,
    /// How many halts an operator has cleared. Agents that announced `halt-v1` are told it
    /// and lift their hold when it is newer than the one they halted under. Written only
    /// once it is not zero, so a file of a service never cleared reads as it always did.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub halt_generation: u64,
    /// The newest version that converged and baked, as the coordinator recorded it; the
    /// version an automatic rollback goes back to.
    #[serde(default)]
    pub last_good: Option<Version>,
    /// The versions never to order again, until CI sets one by hand.
    #[serde(default)]
    pub bad: Vec<Version>,
    /// The failure reports standing when CI set a bad version by hand: one per host. A report
    /// that is acked, whole, does not count towards an automatic rollback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_acked: Vec<FailureAck>,
    /// The operator's force that stands, if any ([`crate::force`]). It is in force while the
    /// desired version is its version; the next ordinary desired state ends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<Force>,
    /// How many forces an operator has made of the service: the generation of the next one is
    /// one more. Written only once it is not zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub force_generation: u64,
    /// The versions of `bad` that a force left: CI's desired state does not lift their mark,
    /// only an operator's `unmark_bad` does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bad_by_force: Vec<Version>,
}

/// An operator's force of a service to a version, past every health gate ([`crate::force`]).
/// It is kept in the halts file ([`crate::ops_file`]), whose reader skips a key it does not
/// know, so a later release may add one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Force {
    /// The version forced.
    pub version: Version,
    /// Which force of the service it is: one more than the one before.
    pub generation: u64,
    /// Why, in the operator's words.
    pub reason: String,
    /// The number of hosts of each role that may update at once while it stands, as the
    /// request raised it; a role it does not name keeps the service's own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub max_updating: BTreeMap<String, u32>,
}

/// One change, as the ring and the log say it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    /// Its number: one more than the entry before it, for the life of the file.
    pub seq: u64,
    /// Who made it.
    pub by: String,
    /// What it was (`exclude`, `include`, `set_labels`, `clear_halt`, `force`, `unmark_bad`,
    /// `mark_bad`; the coordinator's own: `halt`, `last_good`, `clear_last_good`, `mark_bad`;
    /// CI's: `unmark_bad`, `force_ended`).
    pub action: String,
    /// The service it concerned, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The node it concerned, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// The operator's reason, if one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl AuditEntry {
    /// An entry by `by` for `action`; the store numbers it.
    #[must_use]
    pub fn new(by: &str, action: &str) -> Self {
        Self {
            seq: 0,
            by: by.to_owned(),
            action: action.to_owned(),
            service: None,
            node_id: None,
            reason: None,
        }
    }
}

/// Everything the file holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsState {
    schema: u32,
    /// Per node id.
    #[serde(default)]
    pub hosts: BTreeMap<String, HostOps>,
    /// The hosts taken out of a service.
    #[serde(default)]
    pub excluded: Vec<Exclusion>,
    /// Per service name.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceOps>,
    /// The last [`AUDIT_RING`] changes, oldest first.
    #[serde(default)]
    pub audit: Vec<AuditEntry>,
}

impl Default for OpsState {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            hosts: BTreeMap::new(),
            excluded: Vec::new(),
            services: BTreeMap::new(),
            audit: Vec::new(),
        }
    }
}

impl OpsState {
    /// Nothing has been changed: the state of a coordinator with no ops file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Why the ops file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    /// A file operation failed.
    #[error(transparent)]
    Io(#[from] StoreError),
    /// The file is not an ops file this build accepts.
    #[error("{path}: {why}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The file was written by a newer build.
    #[error(
        "{path}: schema {found} is newer than the {SCHEMA} this build reads; starting would \
         ignore what it holds (a halt, an exclusion), so move it aside to start without it"
    )]
    Newer {
        /// The file.
        path: PathBuf,
        /// Its schema.
        found: u64,
    },
}

/// Which order a change reaches the file and memory in.
#[derive(Clone, Copy, Debug)]
pub enum Write {
    /// In memory first, written after: for a change that raises protection (a halt). A
    /// failed write leaves it raised, and the next write carries it.
    AfterMemory,
    /// Written first, in memory only if the file holds it: for every other change, a
    /// clearing one included. A failed write leaves the old state.
    BeforeMemory,
}

/// A change [`OpsStore::interleave`] makes at the start of the next change.
#[cfg(test)]
type Interleaved = Box<dyn FnOnce(&mut OpsState) + Send>;

/// What puts a state into the file.
pub type Writer = Box<dyn Fn(&OpsState) -> Result<(), OpsError> + Send + Sync>;

/// The ops state, in memory and in its file.
pub struct OpsStore {
    current: Mutex<OpsState>,
    /// Held across a whole change, so that writers wait for each other and not readers:
    /// `current` is only locked to read or to swap, never across the file's fsyncs.
    writing: Mutex<()>,
    write: Writer,
    /// A change another writer makes between a caller's read and its change, applied once at
    /// the start of the next change, under the writers' lock: what a race would do there.
    #[cfg(test)]
    interleave: Mutex<Option<Interleaved>>,
}

fn parse(path: &Path, bytes: &[u8]) -> Result<OpsState, OpsError> {
    let invalid = |why: String| OpsError::Invalid {
        path: path.to_owned(),
        why,
    };
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
    let newer = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .filter(|&n| n > u64::from(SCHEMA));
    if let Some(found) = newer {
        return Err(OpsError::Newer {
            path: path.to_owned(),
            found,
        });
    }
    let state: OpsState = serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
    if state.schema != SCHEMA {
        return Err(invalid(format!(
            "schema {} is not the {SCHEMA} this build reads",
            state.schema
        )));
    }
    Ok(state)
}

impl OpsStore {
    /// The store of the file at `path`: its state if it exists, empty if not. Nothing is
    /// created until the first change.
    ///
    /// The halts file beside it ([`crate::ops_file`]) is read and joined to it, and every
    /// change writes both.
    ///
    /// # Errors
    /// [`OpsError`]: the file exists and cannot be read, holds a key this build does not
    /// know, or has another schema; or a halts file a change staged cannot be settled, or the
    /// halts file exists and cannot be read or parsed.
    pub fn open(path: &Path) -> Result<Self, OpsError> {
        let state = match fs::read(path) {
            Ok(bytes) => parse(path, &bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => OpsState::default(),
            Err(source) => {
                return Err(StoreError::Io {
                    path: path.to_owned(),
                    source,
                }
                .into());
            }
        };
        let (state, _skipped) = ops_file::joined(path, state)?;
        let path = path.to_owned();
        Ok(Self::with_writer(
            state,
            Box::new(move |state| ops_file::write(&path, state)),
        ))
    }

    /// A store over `state` that puts every change through `write` instead of a file.
    #[must_use]
    pub fn with_writer(state: OpsState, write: Writer) -> Self {
        Self {
            current: Mutex::new(state),
            writing: Mutex::new(()),
            write,
            #[cfg(test)]
            interleave: Mutex::new(None),
        }
    }

    /// A store that keeps nothing: changes stand in memory only. For a coordinator built
    /// without a file.
    #[must_use]
    pub fn volatile() -> Self {
        Self::with_writer(OpsState::default(), Box::new(|_| Ok(())))
    }

    /// The state now.
    #[must_use]
    pub fn state(&self) -> OpsState {
        self.lock().clone()
    }

    /// What `read` makes of the state now, without copying it: for a decision that looks at
    /// a little of it on every heartbeat.
    pub fn read<T>(&self, read: impl FnOnce(&OpsState) -> T) -> T {
        read(&self.lock())
    }

    /// The marks of `service` now: none if it has none.
    #[must_use]
    pub fn marks(&self, service: &str) -> ServiceOps {
        self.lock()
            .services
            .get(service)
            .cloned()
            .unwrap_or_default()
    }

    fn lock(&self) -> MutexGuard<'_, OpsState> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes `edit` to the state, numbers `entry`, appends it to the ring, logs it, and
    /// reaches the file and memory in the order `write` says. Returns the entry as numbered.
    ///
    /// # Errors
    /// [`OpsError::Io`]: the file could not be replaced. After [`Write::BeforeMemory`] the
    /// old state stands; after [`Write::AfterMemory`] the change stands in memory.
    pub fn change(
        &self,
        write: Write,
        entry: AuditEntry,
        edit: impl FnOnce(&mut OpsState),
    ) -> Result<AuditEntry, OpsError> {
        self.try_change(write, entry, |state| {
            edit(state);
            Ok::<(), OpsError>(())
        })
    }

    /// [`Self::change`] with an edit that may refuse. The edit runs under the lock that
    /// serialises writes, on the state the change is applied to, so a limit it checks (a cap
    /// on a count) holds however many callers there are. When it refuses, its error is
    /// returned and nothing else happens: the state, the ring and its numbering and the file
    /// are as they were, and nothing is logged.
    ///
    /// # Errors
    /// The edit's own error, or [`OpsError`] as for [`Self::change`].
    pub fn try_change<E: From<OpsError>>(
        &self,
        write: Write,
        entry: AuditEntry,
        edit: impl FnOnce(&mut OpsState) -> Result<(), E>,
    ) -> Result<AuditEntry, E> {
        self.decide_change(write, |state| edit(state).map(|()| entry))
    }

    /// [`Self::try_change`] with an edit that also says what the change is: it reads the state
    /// under the lock that serialises writes and answers the entry to audit, so that a change
    /// whose kind or subject depends on the state (raising a halt or keeping a report of one,
    /// clearing the halt that stands) is decided on the state it is applied to, not on one
    /// read before another writer changed it.
    ///
    /// # Errors
    /// The edit's own error, or [`OpsError`] as for [`Self::change`].
    pub(crate) fn decide_change<E: From<OpsError>>(
        &self,
        write: Write,
        edit: impl FnOnce(&mut OpsState) -> Result<AuditEntry, E>,
    ) -> Result<AuditEntry, E> {
        self.decide_changes(write, |state| edit(state).map(|entry| (entry, Vec::new())))
    }

    /// [`Self::decide_change`] for a change of several parts, each audited: the edit answers
    /// the change's own entry and the entries of its other parts, which are numbered after it
    /// in their order. The parts reach the file and memory as one change, so none of them is
    /// made without the others. Answers the change's own entry, as numbered.
    ///
    /// # Errors
    /// The edit's own error, or [`OpsError`] as for [`Self::change`].
    pub(crate) fn decide_changes<E: From<OpsError>>(
        &self,
        write: Write,
        edit: impl FnOnce(&mut OpsState) -> Result<(AuditEntry, Vec<AuditEntry>), E>,
    ) -> Result<AuditEntry, E> {
        let _writer = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        self.interleaved();
        let mut next = self.state();
        let (head, parts) = edit(&mut next)?;
        let mut entries = vec![head];
        entries.extend(parts);
        for entry in &mut entries {
            entry.seq = next.audit.last().map_or(1, |last| last.seq + 1);
            next.audit.push(entry.clone());
        }
        let surplus = next.audit.len().saturating_sub(AUDIT_RING);
        next.audit.drain(..surplus);
        match write {
            Write::AfterMemory => {
                *self.lock() = next.clone();
                entries.iter().for_each(announce);
                (self.write)(&next)?;
            }
            Write::BeforeMemory => {
                (self.write)(&next)?;
                *self.lock() = next;
                entries.iter().for_each(announce);
            }
        }
        Ok(entries.swap_remove(0))
    }
}

#[cfg(test)]
impl OpsStore {
    /// Makes `change` at the start of the next change, under the writers' lock, as another
    /// writer that got there between that caller's read and its change would.
    pub(crate) fn interleave(&self, change: impl FnOnce(&mut OpsState) + Send + 'static) {
        *self
            .interleave
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(change));
    }

    fn interleaved(&self) {
        let change = self
            .interleave
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(change) = change {
            change(&mut self.lock());
        }
    }
}

fn announce(entry: &AuditEntry) {
    tracing::info!(
        target: "kbf_coordinator::audit",
        seq = entry.seq,
        by = %entry.by,
        action = %entry.action,
        service = %entry.service.as_deref().unwrap_or(""),
        node_id = %entry.node_id.as_deref().unwrap_or(""),
        reason = %entry.reason.as_deref().unwrap_or(""),
        "operator change"
    );
}

/// An operator action: `{"action": "<name>", ...}`, the name from a closed set.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    /// Takes a host out of a service: its beats are not counted and it is not ordered.
    Exclude {
        service: String,
        node_id: String,
        reason: String,
    },
    /// Puts it back.
    Include { service: String, node_id: String },
    /// Replaces the labels of a host (none removes them all).
    SetLabels {
        node_id: String,
        #[serde(deserialize_with = "labels_once")]
        labels: BTreeMap<String, String>,
    },
    /// Lifts the halt of a service: orders resume, and the report that raised it is not
    /// raised again.
    ClearHalt { service: String, reason: String },
    /// Rolls a service to a version past every health gate ([`crate::force`]).
    Force {
        service: String,
        version: Version,
        reason: String,
        /// Per role, how many hosts may update at once while the force stands.
        #[serde(default, deserialize_with = "limits_once")]
        max_updating: BTreeMap<String, u32>,
    },
    /// Lifts the mark that a version is bad, a force's mark too ([`crate::force`]).
    UnmarkBad {
        service: String,
        version: Version,
        reason: String,
    },
}

/// A map of labels in which no key is given twice.
fn labels_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    deserializer.deserialize_map(Once::<String>(std::marker::PhantomData))
}

/// A map of limits in which no role is given twice.
fn limits_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, u32>, D::Error> {
    deserializer.deserialize_map(Once::<u32>(std::marker::PhantomData))
}

/// A map of text keys to `V` in which no key is given twice.
struct Once<V>(std::marker::PhantomData<V>);

impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Once<V> {
    type Value = BTreeMap<String, V>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object with no key given twice")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, V>()? {
            if entries.insert(key, value).is_some() {
                return Err(serde::de::Error::custom("a key is given twice"));
            }
        }
        Ok(entries)
    }
}

/// Why an operator action was not done.
#[derive(Debug, thiserror::Error)]
pub enum OperateError {
    /// The caller has no valid operator token.
    #[error(transparent)]
    Unauthorized(#[from] Unauthorized),
    /// The body is no action this coordinator accepts; the text says which part.
    #[error("{0}")]
    Invalid(&'static str),
    /// The file already holds the most of something it keeps; the constant that sets the
    /// most is named.
    #[error(
        "the ops file holds at most {max} {what}: remove some, or raise the constant in ops.rs"
    )]
    Capped {
        /// What there is too much of, with the constant.
        what: &'static str,
        /// The most.
        max: usize,
    },
    /// The ops file could not be written.
    #[error(transparent)]
    Store(#[from] OpsError),
    /// A force names a version older than the service's `min_version`, the first release with
    /// the host-updates hook: such a release is put on a host by its first install, by hand.
    #[error(
        "{version} is older than {floor}, the service's min_version: the first release with the \
         host-updates hook. A force does not go below it; install the older release on each host \
         by hand (docs/deploy/kbf-updater.md, \"Below min_version\")"
    )]
    BelowMinVersion {
        /// The version asked for.
        version: Version,
        /// The service's `min_version`.
        floor: Version,
    },
    /// The desired state of a force could not be written: the ops file holds the force's
    /// marks, and the force is asked again.
    #[error("the desired state could not be written: {0}")]
    Desired(#[from] crate::desired::StoreError),
}

pub(crate) fn node(text: &str) -> Result<String, OperateError> {
    text.parse::<NodeId>()
        .map(|id| id.to_string())
        .map_err(|_| OperateError::Invalid("node_id is not a node id"))
}

pub(crate) fn service<'a>(services: &Services, name: &'a str) -> Result<&'a str, OperateError> {
    services
        .get(name)
        .map(|_| name)
        .ok_or(OperateError::Invalid(
            "service is not one this coordinator manages",
        ))
}

pub(crate) fn reason(text: &str) -> Result<String, OperateError> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_REASON_BYTES || text.chars().any(char::is_control) {
        return Err(OperateError::Invalid(
            "reason must be 1 to 256 bytes with no control character",
        ));
    }
    Ok(text.to_owned())
}

pub(crate) fn label(text: &str) -> bool {
    (1..=MAX_LABEL_BYTES).contains(&text.len())
        && text
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// The operator action in `body`.
///
/// # Errors
/// [`OperateError::Invalid`]: the body is no action.
pub(crate) fn parse_action(body: &[u8]) -> Result<Action, OperateError> {
    serde_json::from_slice(body)
        .map_err(|_| OperateError::Invalid("the body is not an operator action"))
}

/// Does the operator action in `body`, as `by`, in `store`, for an action the ops store makes
/// alone (a force and an unmark need the coordinator: [`crate::force`]). The caller has
/// already checked who `by` is; the body is validated here before anything changes.
///
/// # Errors
/// [`OperateError::Invalid`]: the body is no action, or names an unknown service, a bad node
/// id, a bad reason or bad labels, or is an action the ops store does not make alone.
/// [`OperateError::Store`]: the file could not be written.
pub fn operate(
    store: &OpsStore,
    services: &Services,
    by: &str,
    body: &[u8],
) -> Result<AuditEntry, OperateError> {
    apply(store, services, by, parse_action(body)?)
}

/// Does `action`, as [`operate`].
pub(crate) fn apply(
    store: &OpsStore,
    services: &Services,
    by: &str,
    action: Action,
) -> Result<AuditEntry, OperateError> {
    Ok(match action {
        Action::Exclude {
            service: name,
            node_id,
            reason: why,
        } => {
            let exclusion = Exclusion {
                service: service(services, &name)?.to_owned(),
                node_id: node(&node_id)?,
                reason: reason(&why)?,
            };
            let mut entry = AuditEntry::new(by, "exclude");
            entry.service = Some(exclusion.service.clone());
            entry.node_id = Some(exclusion.node_id.clone());
            entry.reason = Some(exclusion.reason.clone());
            store.try_change(Write::BeforeMemory, entry, |state| {
                let before = state.excluded.len();
                state.excluded.retain(|e| {
                    (e.service.as_str(), e.node_id.as_str())
                        != (exclusion.service.as_str(), exclusion.node_id.as_str())
                });
                let known = state.excluded.len() < before;
                if !known && before >= MAX_EXCLUDED {
                    return Err(OperateError::Capped {
                        what: "exclusions (MAX_EXCLUDED)",
                        max: MAX_EXCLUDED,
                    });
                }
                state.excluded.push(exclusion);
                Ok(())
            })?
        }
        Action::Include {
            service: name,
            node_id,
        } => {
            let (name, id) = (service(services, &name)?.to_owned(), node(&node_id)?);
            let mut entry = AuditEntry::new(by, "include");
            entry.service = Some(name.clone());
            entry.node_id = Some(id.clone());
            store.change(Write::BeforeMemory, entry, |state| {
                state.excluded.retain(|e| {
                    (e.service.as_str(), e.node_id.as_str()) != (name.as_str(), id.as_str())
                });
            })?
        }
        Action::SetLabels { node_id, labels } => {
            let id = node(&node_id)?;
            if labels.len() > MAX_LABELS || !labels.iter().all(|(k, v)| label(k) && label(v)) {
                return Err(OperateError::Invalid(
                    "labels are at most 8, each key and value 1 to 32 bytes of [a-z0-9-]",
                ));
            }
            let mut entry = AuditEntry::new(by, "set_labels");
            entry.node_id = Some(id.clone());
            store.try_change(Write::BeforeMemory, entry, |state| {
                if labels.is_empty() {
                    state.hosts.remove(&id);
                } else {
                    let grows = !state.hosts.contains_key(&id);
                    if grows && state.hosts.len() >= MAX_HOSTS {
                        return Err(OperateError::Capped {
                            what: "hosts with labels (MAX_HOSTS)",
                            max: MAX_HOSTS,
                        });
                    }
                    state.hosts.insert(id, HostOps { labels });
                }
                Ok(())
            })?
        }
        Action::ClearHalt {
            service: name,
            reason: why,
        } => {
            let name = service(services, &name)?.to_owned();
            let why = reason(&why)?;
            // Written first: a clear that cannot be written leaves the halt standing. The halt
            // is the one standing when the change is made, under the writers' lock: one read
            // before could have been cleared by another clear since, and a halt raised after
            // that one would be cleared with the old report's acknowledgement.
            store.decide_change::<OperateError>(Write::BeforeMemory, |state| {
                let marks = state.services.entry(name.clone()).or_default();
                if marks.halt.is_none() {
                    return Err(OperateError::Invalid("the service is not halted"));
                }
                Ok(clear_halt(marks, by, &name, why))
            })?
        }
        Action::Force { .. } | Action::UnmarkBad { .. } => {
            return Err(OperateError::Invalid(
                "a force or an unmark is made by the coordinator, not the ops store alone",
            ));
        }
    })
}

/// Clears the halt of `service` in `marks`, as `by`, for `why`: the one path of a clear, the
/// operator's `clear_halt` and a force's alike. The reports of the halt and of every other host
/// that reported while it stood are acknowledged (one per host, the latest) so that a host,
/// which keeps reporting until it updates again, does not raise the halt again: a clear clears
/// the service. The clear generation goes up by one, which lifts the hold of every agent that
/// halted under an older one, whether or not a halt stands here (a force clears a service with
/// none, so that an agent's own hold does not refuse the forced order). The caller writes it
/// before it is in memory.
pub(crate) fn clear_halt(
    marks: &mut ServiceOps,
    by: &str,
    service: &str,
    why: String,
) -> AuditEntry {
    let mut entry = AuditEntry::new(by, "clear_halt");
    entry.service = Some(service.to_owned());
    entry.reason = Some(why);
    let halt = marks.halt.take();
    entry.node_id = halt.as_ref().map(|halt| halt.node_id.clone());
    let seen = std::mem::take(&mut marks.halt_seen);
    let reports = halt.into_iter().map(HaltAck::from).chain(seen);
    for report in reports {
        marks.halt_acked.retain(|ack| ack.node_id != report.node_id);
        marks.halt_acked.push(report);
    }
    marks.halt_generation = marks.halt_generation.saturating_add(1);
    entry
}
