//! The desired state: the one thing the coordinator stores (`docs/design/updates.md`,
//! section 3).
//!
//! CI sets it with one call: the version every host should run, how many hosts of a kind
//! may update at once, and how many storage hosts and frontends must stay up. The store
//! keeps one such state per service name; a service must be `kbf` or one the services file
//! defines (`crate::services`). Hosts are ordered only for `kbf` until their heartbeats
//! name other services. It is one small JSON file, replaced whole by a temporary file,
//! fsync, rename and a directory fsync, so a crash leaves the old state or the new one.
//! While `kbf` is the only service the file is the single state itself (the format earlier
//! builds read and wrote, so a downgrade keeps working); a file with
//! `{"services": {name: state}}` is the map. A file that cannot be read back is a refusal
//! to start: the coordinator does not guess what an operator meant.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kbf_types::HostRole;
use kbf_updater::version::Version;

use serde::{Deserialize, Serialize};

use crate::services::Services;

/// The service every host runs today, and the only one a desired state may name unless a
/// services file defines more.
pub const KBF: &str = "kbf";

/// The most any count may be: a typo's order of magnitude, not a plan.
pub const MAX_COUNT: u32 = 10_000;

/// The host roles of the farm, in the order every output lists them (frontend, storage, daemon,
/// Mac). A role of a service that is not one of these is listed after them, by name.
pub const LEGACY_ROLES: [&str; 4] = ["frontend", "storage", "daemon", "mac-daemon"];

/// The minimums an earlier build named, in the order it wrote them. `kbf` requires both.
pub const KBF_MIN_UP: [&str; 2] = ["storage", "frontend"];

/// The four host roles `kbf` names, in the order the status lists them. The services loader
/// accepts no others.
#[must_use]
pub fn kbf_roles() -> [HostRole; 4] {
    [
        HostRole::FRONTEND,
        HostRole::STORAGE,
        HostRole::DAEMON,
        HostRole::MAC_DAEMON,
    ]
}

/// Whether `name` can name a role: the grammar of a host role ([`HostRole`]'s parse), which
/// is the one place that says.
#[must_use]
pub fn is_role_name(name: &str) -> bool {
    name.parse::<HostRole>().is_ok()
}

/// A number per role name: the limit on hosts updating at once, or the minimum that stays up.
///
/// A role the map does not name has the default of the question asked ([`Self::max_updating_of`],
/// [`Self::min_up_of`]). The defaults are for a role a service definition names: whether a role
/// is run at all is the definition's to say, never this map's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct RoleLimits(BTreeMap<String, u32>);

impl<const N: usize> From<[(&str, u32); N]> for RoleLimits {
    fn from(pairs: [(&str, u32); N]) -> Self {
        Self(pairs.map(|(role, n)| (role.to_owned(), n)).into())
    }
}

impl RoleLimits {
    /// The limits of a state that says none: one frontend and one storage host, two daemons
    /// and two Macs at a time.
    #[must_use]
    pub fn max_updating_default() -> Self {
        Self(
            LEGACY_ROLES
                .into_iter()
                .map(|role| (role.to_owned(), default_max_updating(role)))
                .collect(),
        )
    }

    /// What the map says of `role`, if it names it.
    #[must_use]
    pub fn get(&self, role: &str) -> Option<u32> {
        self.0.get(role).copied()
    }

    /// Sets the number of `role`.
    pub fn set(&mut self, role: &str, n: u32) {
        self.0.insert(role.to_owned(), n);
    }

    /// How many hosts of `role` may update at once: the map's number, else the default (2 for
    /// the daemon roles, 1 for any other).
    #[must_use]
    pub fn max_updating_of(&self, role: &str) -> u32 {
        self.get(role).unwrap_or_else(|| default_max_updating(role))
    }

    /// How many hosts of `role` must stay up: the map's number, else none.
    #[must_use]
    pub fn min_up_of(&self, role: &str) -> u32 {
        self.get(role).unwrap_or(0)
    }

    /// The entries: the names in `legacy` first, in that order, then the others by name.
    fn ordered<'a>(&'a self, legacy: &[&'a str]) -> Vec<(&'a str, u32)> {
        let first = legacy
            .iter()
            .filter_map(|role| self.get(role).map(|n| (*role, n)));
        let rest = self
            .0
            .iter()
            .filter(|(role, _)| !legacy.contains(&role.as_str()))
            .map(|(role, n)| (role.as_str(), *n));
        first.chain(rest).collect()
    }

    /// Drops the limits for roles that are neither in `legacy` nor accepted by `keep`, and
    /// returns their names.
    fn retain(&mut self, legacy: &[&str], keep: impl Fn(&str) -> bool) -> Vec<String> {
        let mut dropped = Vec::new();
        self.0.retain(|role, _| {
            let kept = legacy.contains(&role.as_str()) || keep(role);
            if !kept {
                dropped.push(role.clone());
            }
            kept
        });
        dropped
    }
}

const fn default_max_updating(role: &str) -> u32 {
    match role.as_bytes() {
        b"daemon" | b"mac-daemon" => 2,
        _ => 1,
    }
}

/// The limits in the order the old struct wrote them.
fn write_max_updating<S: serde::Serializer>(limits: &RoleLimits, s: S) -> Result<S::Ok, S::Error> {
    s.collect_map(limits.ordered(&LEGACY_ROLES))
}

/// The minimums in the order the old struct wrote them. The old struct requires `storage` and
/// `frontend`, so a state that names neither writes them as 0, which is what none means.
fn write_min_up<S: serde::Serializer>(limits: &RoleLimits, s: S) -> Result<S::Ok, S::Error> {
    let mut written = limits.clone();
    for role in KBF_MIN_UP {
        written.0.entry(role.to_owned()).or_insert(0);
    }
    s.collect_map(written.ordered(&KBF_MIN_UP))
}

/// The limits as written, with the four legacy roles filled in with their defaults.
fn read_max_updating<'de, D: serde::Deserializer<'de>>(d: D) -> Result<RoleLimits, D::Error> {
    let mut limits = RoleLimits::deserialize(d)?;
    for (role, n) in RoleLimits::max_updating_default().0 {
        limits.0.entry(role).or_insert(n);
    }
    Ok(limits)
}

/// The desired state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Desired {
    /// The version every host should run. An older one than the hosts run is a rollback.
    pub version: Version,
    /// The per-role limit on hosts updating at once.
    #[serde(
        default = "RoleLimits::max_updating_default",
        deserialize_with = "read_max_updating",
        serialize_with = "write_max_updating"
    )]
    pub max_updating: RoleLimits,
    /// The minimum number of hosts that stay up.
    #[serde(serialize_with = "write_min_up")]
    pub min_up: RoleLimits,
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

/// The minimums `kbf` requires a state to name.
fn kbf_required() -> Vec<String> {
    KBF_MIN_UP.map(str::to_owned).into()
}

impl Desired {
    /// The desired state of `kbf` in `bytes`: the single state of the state file.
    ///
    /// # Errors
    /// [`DesiredError`].
    pub fn parse(bytes: &[u8]) -> Result<Self, DesiredError> {
        let desired: Self =
            serde_json::from_slice(bytes).map_err(|e| DesiredError::Malformed(e.to_string()))?;
        desired.checked(&kbf_required())?;
        Ok(desired)
    }

    /// The service and desired state of a `POST /v1/desired` body: the state's fields and an
    /// optional `service` (absent means [`KBF`]), for a coordinator that manages `kbf` alone.
    ///
    /// # Errors
    /// [`DesiredError`]; [`DesiredError::UnknownService`] for any service but [`KBF`].
    pub fn parse_request(bytes: &[u8]) -> Result<(String, Self), DesiredError> {
        Self::parse_request_for(bytes, &Services::default())
    }

    /// Like [`Self::parse_request`], for the services of `services`.
    ///
    /// The body is the one every service gets: a service takes the `min_up` keys its definition
    /// requires (`kbf`: `storage` and `frontend`) and may omit the map when it requires none. A
    /// limit for a role the service does not have is dropped, so that one body does for all
    /// services; the four legacy roles' keys stay as sent, the form earlier builds read.
    ///
    /// # Errors
    /// [`DesiredError`]; [`DesiredError::UnknownService`] for a service `services` lacks.
    pub fn parse_request_for(
        bytes: &[u8],
        services: &Services,
    ) -> Result<(String, Self), DesiredError> {
        Self::parse_request_noting(bytes, services).map(|(service, desired, _)| (service, desired))
    }

    /// [`Self::parse_request_for`], also returning the names of the roles whose limits were
    /// dropped because the service does not have them (sorted, each once), which it logs as a
    /// warning.
    ///
    /// # Errors
    /// [`DesiredError`], as [`Self::parse_request_for`].
    pub fn parse_request_noting(
        bytes: &[u8],
        services: &Services,
    ) -> Result<(String, Self, Vec<String>), DesiredError> {
        let malformed = |e: serde_json::Error| DesiredError::Malformed(e.to_string());
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(bytes).map_err(malformed)?;
        let service = match fields.remove("service") {
            None => KBF.to_owned(),
            Some(serde_json::Value::String(name)) => name,
            Some(_) => return Err(DesiredError::Malformed("service is not text".to_owned())),
        };
        let def = services.get(&service).ok_or(DesiredError::UnknownService)?;
        if def.require_min_up.is_empty() && !fields.contains_key("min_up") {
            fields.insert("min_up".to_owned(), serde_json::json!({}));
        }
        let mut desired: Self = serde_json::from_value(fields.into()).map_err(malformed)?;
        desired.checked(&def.require_min_up)?;
        desired.refuse_unnamed(services)?;
        let has = |role: &str| def.roles.contains_key(role);
        let mut dropped = desired.max_updating.retain(&LEGACY_ROLES, has);
        dropped.extend(desired.min_up.retain(&KBF_MIN_UP, has));
        dropped.sort();
        dropped.dedup();
        if !dropped.is_empty() {
            tracing::warn!(%service, roles = ?dropped, "dropped limits for roles the service does not have");
        }
        Ok((service, desired, dropped))
    }

    /// Refuses a limits key that is neither a legacy name nor a role of any service, so that a
    /// typo is an error and not a limit silently lost; one body still serves every service.
    fn refuse_unnamed(&self, services: &Services) -> Result<(), DesiredError> {
        let maps = [
            ("max_updating", &self.max_updating, &LEGACY_ROLES[..]),
            ("min_up", &self.min_up, &KBF_MIN_UP[..]),
        ];
        for (map, limits, legacy) in maps {
            let unnamed = limits.0.keys().find(|role| {
                !legacy.contains(&role.as_str())
                    && !services.defs().any(|def| def.roles.contains_key(*role))
            });
            if let Some(role) = unnamed {
                return Err(DesiredError::Malformed(format!(
                    "{map}: {role} is no role of any service"
                )));
            }
        }
        Ok(())
    }

    /// Whether the state is acceptable: every limit key a role name, every count within
    /// [`MAX_COUNT`], and every minimum in `require` named.
    fn checked(&self, require: &[String]) -> Result<(), DesiredError> {
        let limits = [&self.max_updating, &self.min_up];
        for role in limits.iter().flat_map(|l| l.0.keys()) {
            if !is_role_name(role) {
                return Err(DesiredError::Malformed(format!(
                    "{role:?} is not a role name"
                )));
            }
        }
        if let Some(role) = require.iter().find(|r| !self.min_up.0.contains_key(*r)) {
            return Err(DesiredError::Malformed(format!("min_up lacks {role}")));
        }
        if limits
            .iter()
            .flat_map(|l| l.0.values())
            .any(|&n| n > MAX_COUNT)
        {
            return Err(DesiredError::Count);
        }
        Ok(())
    }
}

/// The state file in its map form.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MapFile {
    services: BTreeMap<String, Desired>,
}

/// The services in `bytes`: a map file, or the single state of earlier builds as `kbf`'s.
fn parse_file(bytes: &[u8]) -> Result<BTreeMap<String, Desired>, DesiredError> {
    let is_map = serde_json::from_slice::<serde_json::Value>(bytes)
        .is_ok_and(|v| v.get("services").is_some());
    if !is_map {
        return Ok(BTreeMap::from([(KBF.to_owned(), Desired::parse(bytes)?)]));
    }
    let file: MapFile =
        serde_json::from_slice(bytes).map_err(|e| DesiredError::Malformed(e.to_string()))?;
    for (name, desired) in &file.services {
        let require = if name == KBF {
            kbf_required()
        } else {
            Vec::new()
        };
        desired.checked(&require)?;
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
            _ => serde_json::to_vec_pretty(&MapFile {
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
