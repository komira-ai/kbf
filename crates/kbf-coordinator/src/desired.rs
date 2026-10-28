//! The desired state: the one thing the coordinator stores (`docs/design/updates.md`,
//! section 3).
//!
//! CI sets it with one call: the version every host should run, how many hosts of a kind
//! may update at once, and how many storage hosts and frontends must stay up. The store
//! keeps one such state per service name; `kbf` is the only service the coordinator accepts
//! and decides on. It is one small JSON file, replaced whole by a temporary file, fsync,
//! rename and a directory fsync, so a crash leaves the old state or the new one. While `kbf`
//! is the only service the file is the single state itself (the format earlier builds read
//! and wrote, so a downgrade keeps working); a file with `{"services": {name: state}}` is
//! the map. A file that cannot be read back is a refusal to start: the coordinator does not
//! guess what an operator meant.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kbf_types::HostRole;
use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};

/// The service every host runs today, and the only one a desired state may name.
pub const KBF: &str = "kbf";

/// The most any count may be: a typo's order of magnitude, not a plan.
pub const MAX_COUNT: u32 = 10_000;

/// How many hosts of each kind may be updating at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaxUpdating {
    /// Frontends (default 1).
    #[serde(default = "one")]
    pub frontend: u32,
    /// Storage hosts (default 1).
    #[serde(default = "one")]
    pub storage: u32,
    /// Daemon-only Linux hosts (default 2).
    #[serde(default = "two")]
    pub daemon: u32,
    /// Macs that run a daemon (default 2).
    #[serde(default = "two", rename = "mac-daemon")]
    pub mac_daemon: u32,
}

const fn one() -> u32 {
    1
}

const fn two() -> u32 {
    2
}

impl Default for MaxUpdating {
    fn default() -> Self {
        Self {
            frontend: one(),
            storage: one(),
            daemon: two(),
            mac_daemon: two(),
        }
    }
}

impl MaxUpdating {
    /// The limit for hosts of `role`.
    #[must_use]
    pub const fn of(&self, role: HostRole) -> u32 {
        match role {
            HostRole::Frontend => self.frontend,
            HostRole::Storage => self.storage,
            HostRole::Daemon => self.daemon,
            HostRole::MacDaemon => self.mac_daemon,
        }
    }
}

/// How many hosts must be up (live, healthy, not updating) while another updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinUp {
    /// Storage hosts. The operator picks it so that every blob stays readable.
    pub storage: u32,
    /// Frontends. At least one is always required, whatever this says.
    pub frontend: u32,
}

/// The desired state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Desired {
    /// The version every host should run. An older one than the hosts run is a rollback.
    pub version: Version,
    /// The per-role limit on hosts updating at once.
    #[serde(default)]
    pub max_updating: MaxUpdating,
    /// The minimum number of hosts that stay up.
    pub min_up: MinUp,
}

/// Why a desired state is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DesiredError {
    /// Not JSON of the shape (a missing field, an unknown one, a version the rule refuses).
    #[error("the desired state is malformed: {0}")]
    Malformed(String),
    /// A count is over [`MAX_COUNT`].
    #[error("a count is over {MAX_COUNT}")]
    Count,
    /// The `service` is not one the coordinator manages.
    #[error("no such service")]
    UnknownService,
}

impl Desired {
    /// The desired state in `bytes`.
    ///
    /// # Errors
    /// [`DesiredError`].
    pub fn parse(bytes: &[u8]) -> Result<Self, DesiredError> {
        let desired: Self =
            serde_json::from_slice(bytes).map_err(|e| DesiredError::Malformed(e.to_string()))?;
        desired.checked()?;
        Ok(desired)
    }

    /// The service and desired state of a `POST /v1/desired` body: the state's fields and an
    /// optional `service` (absent means [`KBF`]).
    ///
    /// # Errors
    /// [`DesiredError`]; [`DesiredError::UnknownService`] for any service but [`KBF`].
    pub fn parse_request(bytes: &[u8]) -> Result<(String, Self), DesiredError> {
        let malformed = |e: serde_json::Error| DesiredError::Malformed(e.to_string());
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(bytes).map_err(malformed)?;
        let service = match fields.remove("service") {
            None => KBF.to_owned(),
            Some(serde_json::Value::String(name)) => name,
            Some(_) => return Err(DesiredError::Malformed("service is not text".to_owned())),
        };
        if service != KBF {
            return Err(DesiredError::UnknownService);
        }
        let desired: Self = serde_json::from_value(fields.into()).map_err(malformed)?;
        desired.checked()?;
        Ok((service, desired))
    }

    fn checked(&self) -> Result<(), DesiredError> {
        let desired = self;
        let counts = [
            desired.max_updating.frontend,
            desired.max_updating.storage,
            desired.max_updating.daemon,
            desired.max_updating.mac_daemon,
            desired.min_up.storage,
            desired.min_up.frontend,
        ];
        if counts.iter().any(|&n| n > MAX_COUNT) {
            return Err(DesiredError::Count);
        }
        Ok(())
    }
}

/// The state file in its map form.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Services {
    services: BTreeMap<String, Desired>,
}

/// The services in `bytes`: a map file, or the single state of earlier builds as `kbf`'s.
fn parse_file(bytes: &[u8]) -> Result<BTreeMap<String, Desired>, DesiredError> {
    let is_map = serde_json::from_slice::<serde_json::Value>(bytes)
        .is_ok_and(|v| v.get("services").is_some());
    if !is_map {
        return Ok(BTreeMap::from([(KBF.to_owned(), Desired::parse(bytes)?)]));
    }
    let file: Services =
        serde_json::from_slice(bytes).map_err(|e| DesiredError::Malformed(e.to_string()))?;
    for desired in file.services.values() {
        desired.checked()?;
    }
    Ok(file.services)
}

/// Why the state file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A file operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The file holds no desired state this build accepts.
    #[error("{path}: {source}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: DesiredError,
    },
}

/// The desired state, in memory and in its file.
#[derive(Debug)]
pub struct DesiredStore {
    path: PathBuf,
    current: Mutex<BTreeMap<String, Desired>>,
    /// Held across a whole replacement, so that writers wait for each other and not readers:
    /// `current` is only locked to read or to swap, never across the file's fsyncs.
    writing: Mutex<()>,
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The directory a file is in: `.` for a bare file name.
pub(crate) fn dir_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

impl DesiredStore {
    /// The store of the file at `path`: its state if it exists, none if not.
    ///
    /// # Errors
    /// [`StoreError`]: the file exists and cannot be read, or holds no desired state.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let current = match fs::read(path) {
            Ok(bytes) => parse_file(&bytes).map_err(|source| StoreError::Invalid {
                path: path.to_owned(),
                source,
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(io_at(path)(e)),
        };
        Ok(Self {
            path: path.to_owned(),
            current: Mutex::new(current),
            writing: Mutex::new(()),
        })
    }

    /// The desired state of `service` now, if one was ever set.
    #[must_use]
    pub fn get(&self, service: &str) -> Option<Desired> {
        self.lock().get(service).cloned()
    }

    /// The desired state of every service, by name.
    #[must_use]
    pub fn all(&self) -> BTreeMap<String, Desired> {
        self.lock().clone()
    }

    /// Replaces the desired state of `service` (the others stay), in the file first and in
    /// memory only if the file holds it.
    ///
    /// # Errors
    /// [`StoreError::Io`]: the file could not be replaced; the old state stands.
    pub fn set(&self, service: &str, desired: Desired) -> Result<(), StoreError> {
        let _writer = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = self.all();
        next.insert(service.to_owned(), desired);
        self.write(&next)?;
        *self.lock() = next;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Desired>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self, services: &BTreeMap<String, Desired>) -> Result<(), StoreError> {
        let mut temp = self.path.clone().into_os_string();
        temp.push(".new");
        let temp = PathBuf::from(temp);
        // Only `kbf` is the single state of earlier builds, which refuse the map form.
        let encoded = match services.iter().collect::<Vec<_>>().as_slice() {
            [(name, only)] if name.as_str() == KBF => serde_json::to_vec_pretty(only),
            _ => serde_json::to_vec_pretty(&Services {
                services: services.clone(),
            }),
        };
        let mut text = encoded.map_err(io::Error::from).map_err(io_at(&temp))?;
        text.push(b'\n');
        // A leftover of a crash is replaced, never followed: the temporary file is created
        // new, owner-only.
        match fs::remove_file(&temp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_at(&temp)(e)),
            _ => {}
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(io_at(&temp))?;
        file.write_all(&text).map_err(io_at(&temp))?;
        file.sync_all().map_err(io_at(&temp))?;
        fs::rename(&temp, &self.path).map_err(io_at(&self.path))?;
        let dir = dir_of(&self.path);
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io_at(dir))
    }
}
