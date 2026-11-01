//! The ops store: what operators and the coordinator itself decide, kept apart from CI's
//! desired state (`docs/design/host-updates.md`, sections 3.8 and 5).
//!
//! It holds the hosts an operator excluded from a service, the labels an operator put on a
//! host, the per-service marks later changes fill (a halt, the last good version, the bad
//! versions), and a ring of the last changes made. It is one JSON file, `--ops-file`, kept
//! apart from `desired.json` so that an earlier coordinator, which refuses a key it does not
//! know there, still reads the desired state. The file is created by the first change (a
//! farm that never uses it keeps its files as they are) and replaced whole after that, like
//! the desired state: a temporary file, fsync, rename and a directory fsync.
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
//! The operator actions ([`operate`]) are the only writers in this PR; nothing reads the
//! store when it decides what a host is told.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use kbf_types::NodeId;
use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};

use crate::desired::{StoreError, io_at, write_whole};
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

/// A halt of a service: set by a later change, cleared by the operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Halt {
    /// The host that reported it.
    pub node_id: String,
    /// The check that failed.
    pub check: String,
    /// The version it was updating to.
    pub target: Version,
}

/// A host and the version whose halt report an operator cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaltAck {
    /// The host.
    pub node_id: String,
    /// The version.
    pub target: Version,
}

/// The marks of one service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOps {
    /// The standing halt, if any.
    #[serde(default)]
    pub halt: Option<Halt>,
    /// The halt reports an operator cleared.
    #[serde(default)]
    pub halt_acked: Vec<HaltAck>,
    /// The newest version that converged and baked.
    #[serde(default)]
    pub last_good: Option<Version>,
    /// The versions never to order again.
    #[serde(default)]
    pub bad: Vec<Version>,
}

/// One change, as the ring and the log say it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    /// Its number: one more than the entry before it, for the life of the file.
    pub seq: u64,
    /// Who made it.
    pub by: String,
    /// What it was (`exclude`, `include`, `set_labels`).
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

/// What puts a state into the file.
pub type Writer = Box<dyn Fn(&OpsState) -> Result<(), OpsError> + Send + Sync>;

/// The ops state, in memory and in its file.
pub struct OpsStore {
    current: Mutex<OpsState>,
    /// Held across a whole change, so that writers wait for each other and not readers:
    /// `current` is only locked to read or to swap, never across the file's fsyncs.
    writing: Mutex<()>,
    write: Writer,
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
    /// # Errors
    /// [`OpsError`]: the file exists and cannot be read, holds a key this build does not
    /// know, or has another schema.
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
        let path = path.to_owned();
        Ok(Self::with_writer(
            state,
            Box::new(move |state| {
                let mut text = serde_json::to_vec_pretty(state)
                    .map_err(io::Error::from)
                    .map_err(io_at(&path))?;
                text.push(b'\n');
                Ok(write_whole(&path, &text)?)
            }),
        ))
    }

    /// A store over `state` that puts every change through `write` instead of a file.
    #[must_use]
    pub fn with_writer(state: OpsState, write: Writer) -> Self {
        Self {
            current: Mutex::new(state),
            writing: Mutex::new(()),
            write,
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
        mut entry: AuditEntry,
        edit: impl FnOnce(&mut OpsState) -> Result<(), E>,
    ) -> Result<AuditEntry, E> {
        let _writer = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = self.state();
        edit(&mut next)?;
        entry.seq = next.audit.last().map_or(1, |last| last.seq + 1);
        next.audit.push(entry.clone());
        let surplus = next.audit.len().saturating_sub(AUDIT_RING);
        next.audit.drain(..surplus);
        match write {
            Write::AfterMemory => {
                *self.lock() = next.clone();
                announce(&entry);
                (self.write)(&next)?;
            }
            Write::BeforeMemory => {
                (self.write)(&next)?;
                *self.lock() = next;
                announce(&entry);
            }
        }
        Ok(entry)
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
enum Action {
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
}

/// A map of labels in which no key is given twice.
fn labels_once<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Once;

    impl<'de> serde::de::Visitor<'de> for Once {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object of labels")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut labels = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if labels.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("a label key is given twice"));
                }
            }
            Ok(labels)
        }
    }

    deserializer.deserialize_map(Once)
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
}

fn node(text: &str) -> Result<String, OperateError> {
    text.parse::<NodeId>()
        .map(|id| id.to_string())
        .map_err(|_| OperateError::Invalid("node_id is not a node id"))
}

fn service<'a>(services: &Services, name: &'a str) -> Result<&'a str, OperateError> {
    services
        .get(name)
        .map(|_| name)
        .ok_or(OperateError::Invalid(
            "service is not one this coordinator manages",
        ))
}

fn reason(text: &str) -> Result<String, OperateError> {
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

/// Does the operator action in `body`, as `by`, in `store`. The caller has already checked
/// who `by` is; the body is validated here before anything changes.
///
/// # Errors
/// [`OperateError::Invalid`]: the body is no action, or names an unknown service, a bad node
/// id, a bad reason or bad labels. [`OperateError::Store`]: the file could not be written.
pub fn operate(
    store: &OpsStore,
    services: &Services,
    by: &str,
    body: &[u8],
) -> Result<AuditEntry, OperateError> {
    let action: Action = serde_json::from_slice(body)
        .map_err(|_| OperateError::Invalid("the body is not an operator action"))?;
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
    })
}
