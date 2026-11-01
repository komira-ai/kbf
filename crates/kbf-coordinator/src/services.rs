//! Services as data: what the coordinator updates, the roles each service runs on, and the
//! order between services (`docs/design/updates.md`, section 3).
//!
//! A [`ServiceDef`] has a name, the roles it runs on (each with its limits and the roles it
//! waits for) and `after`, the services whose live hosts must have converged before any host
//! of this one is ordered. The decision reads these and branches on no service name.
//!
//! `kbf`'s definition is shipped data in this same format (`kbf.services.toml`, read by
//! [`ServiceDef::kbf`]). A file named by `--services-file` defines further services, and may
//! begin with an entry named `kbf` to replace the shipped definition: that entry must be the
//! first, wait for no service and have a role, and a desired state of `kbf` still has to name
//! `min_up` for storage and frontend, whatever roles the replacement has. Which services a CI
//! token may set is not here: the allow-list entry that admits the token says
//! (`services = [...]`, default `kbf`).
//!
//! ```toml
//! [[service]]
//! name = "agent"
//! after = ["server"]             # services defined earlier in the file, or kbf
//! priority = 0                   # default 0; a host is offered the higher number first
//! install_when_absent = false    # default false; order a host whose copy never registered
//!
//! [service.selector]             # optional; absent: a host belongs if it announces the service
//! roles = ["daemon"]             # default: every role of the service
//! labels = { zone = "a" }        # operator-set labels the host must carry
//! bake_beats = 0                 # default 0, at most 100: healthy beats before a slot frees
//! canary = { count = 1 }         # optional: only this many hosts go until they have baked
//! rollback = "manual"            # or "automatic": the coordinator sets the last good version
//!                                # when a release fails on enough hosts (default manual)
//! rollback_after_failed = { percent = 25, min_hosts = 2 }  # the larger of the two (default)
//! min_version = "0.0.18"         # optional: the first version with update hooks; setting the
//!                                # desired state to it forgets the last good version
//! defer_backoff = { initial_secs = 300, factor = 2, max_secs = 3600 }   # the defaults
//!
//! [service.roles.daemon]
//! max_updating = 2               # optional; absent: the desired state's number
//! min_up = 0                     # optional; absent: the desired state's number
//! never_last = false             # default false
//! after = ["storage"]            # roles of this service this role waits for
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use kbf_types::HostRole;
use kbf_updater::secret::read_secret;
use kbf_updater::version::Version;
use kbf_updater::wire::{MAX_SERVICE_NAME, MAX_SERVICES, is_service_name};
use serde::Deserialize;

use crate::bake::MAX_BAKE_BEATS;
use crate::canary::Canary;
use crate::defer::DeferBackoff;
use crate::desired::{Desired, KBF, KBF_MIN_UP, LEGACY_ROLES, MAX_COUNT, kbf_roles};
use crate::membership::{RawSelector, Selector};
use crate::reconcile::{FailThreshold, Rollback};

/// The most bytes of a services file.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// kbf's shipped definition, in the format of a services file.
const KBF_SERVICES: &str = include_str!("kbf.services.toml");

/// What one role of a service is allowed and required to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleDef {
    /// How many hosts of the role may update at once. `None`: the desired state's number.
    pub max_updating: Option<u32>,
    /// How many other hosts of the role must be up while one updates. `None`: the desired
    /// state's number.
    pub min_up: Option<u32>,
    /// Never order the last host of the role that is up, whatever `min_up` says.
    pub never_last: bool,
    /// The roles of the same service whose live hosts must all run the desired version, and
    /// not be updating, before a host of this role is ordered.
    pub after: Vec<String>,
}

impl RoleDef {
    /// A role with no limits of its own and no wait.
    #[must_use]
    pub const fn plain() -> Self {
        Self {
            max_updating: None,
            min_up: None,
            never_last: false,
            after: Vec::new(),
        }
    }

    /// The limit on hosts of the role `role` updating at once under `desired`: the
    /// definition's number, else the desired state's, else the default (2 for the daemon
    /// roles, 1 for any other). A role the service does not name is never ordered, whatever
    /// these say ([`crate::decide`]).
    #[must_use]
    pub fn max_updating(&self, role: &str, desired: &Desired) -> usize {
        self.max_updating
            .unwrap_or_else(|| desired.max_updating.max_updating_of(role)) as usize
    }

    /// How many other hosts of the role must be up while one updates under `desired`.
    #[must_use]
    pub fn required_up(&self, role: &str, desired: &Desired) -> usize {
        let floor = usize::from(self.never_last);
        (self
            .min_up
            .unwrap_or_else(|| desired.min_up.min_up_of(role)) as usize)
            .max(floor)
    }
}

/// A service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDef {
    /// Its name.
    pub name: String,
    /// The roles it runs on. A host of any other role is never ordered for it.
    pub roles: BTreeMap<String, RoleDef>,
    /// The services that must have converged before any host of this one is ordered.
    pub after: Vec<String>,
    /// The `min_up` keys a desired state of the service must name (`kbf`: storage and
    /// frontend); a body without one is refused. Any other service may omit `min_up`.
    pub require_min_up: Vec<String>,
    /// Which service a host is offered first when more than one is behind: the higher
    /// number first, and services with the same number in the tie order of
    /// [`crate::service::Coordinator::heartbeat`]. It orders offers only: a service that
    /// must wait (`after`, its slots) yields to the next one whatever its priority.
    pub priority: i32,
    /// Which hosts should run the service (`None`: the hosts that announce it).
    pub selector: Option<Selector>,
    /// Whether a host on which the service is declared and has never registered is ordered.
    pub install_when_absent: bool,
    /// How many healthy beats a host at the desired version must send before it frees the
    /// slot it held while updating (default 0: it frees it at once).
    pub bake_beats: u32,
    /// The hosts that go alone first, if the service has a canary.
    pub canary: Option<Canary>,
    /// Whether the coordinator rolls the service back by itself.
    pub rollback: Rollback,
    /// How many hosts must fail before it does.
    pub rollback_after_failed: FailThreshold,
    /// The first version that has update hooks: no rollback goes below it.
    pub min_version: Option<Version>,
    /// How long a host that deferred an update is left alone ([`crate::defer`]).
    pub defer_backoff: DeferBackoff,
}

impl ServiceDef {
    /// A service named `name` with the roles `roles`, each by name.
    #[cfg(test)]
    pub(crate) fn new(name: &str, roles: &[(&str, RoleDef)]) -> Self {
        Self {
            name: name.to_owned(),
            roles: roles
                .iter()
                .map(|(role, def)| ((*role).to_owned(), def.clone()))
                .collect(),
            after: Vec::new(),
            require_min_up: Vec::new(),
            priority: 0,
            selector: None,
            install_when_absent: false,
            bake_beats: 0,
            canary: None,
            rollback: Rollback::Manual,
            rollback_after_failed: FailThreshold::default(),
            min_version: None,
            defer_backoff: DeferBackoff::default(),
        }
    }

    /// The names of its roles: the four legacy roles first, in their order (frontend, storage,
    /// daemon, mac-daemon), then any other by name. Every list of roles is in this order.
    #[must_use]
    pub fn role_names(&self) -> Vec<&str> {
        let legacy = LEGACY_ROLES
            .into_iter()
            .filter(|role| self.roles.contains_key(*role));
        let rest = self
            .roles
            .keys()
            .map(String::as_str)
            .filter(|role| !LEGACY_ROLES.contains(role));
        legacy.chain(rest).collect()
    }

    /// `kbf`'s shipped definition (`kbf.services.toml`): all four roles, limits from the
    /// desired state, and frontends that wait for every other role and are never the last
    /// one up.
    #[must_use]
    pub fn kbf() -> Self {
        let file: RawFile = toml::from_str(KBF_SERVICES)
            .expect("kbf.services.toml is TOML of the services file shape (a test reads it)");
        let raw = file
            .service
            .into_iter()
            .next()
            .expect("kbf.services.toml defines kbf (a test reads it)");
        kbf_def(raw).expect("kbf.services.toml is a valid definition (a test reads it)")
    }
}

/// Why a services file is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServicesError {
    /// The file cannot be read.
    #[error("read {0}")]
    Read(String),
    /// The text is not TOML of the shape.
    #[error("services: {0}")]
    Parse(String),
    /// A value is unusable.
    #[error("services: {0}")]
    Invalid(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    service: Vec<RawService>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    name: String,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    roles: BTreeMap<String, RawRole>,
    #[serde(default)]
    priority: i32,
    selector: Option<RawSelector>,
    #[serde(default)]
    install_when_absent: bool,
    #[serde(default)]
    bake_beats: u32,
    canary: Option<RawCanary>,
    #[serde(default)]
    rollback: Rollback,
    #[serde(default)]
    rollback_after_failed: FailThreshold,
    min_version: Option<Version>,
    #[serde(default)]
    defer_backoff: DeferBackoff,
}

/// The count form of the canary. The roles and labels form is refused as an unknown field
/// until it is implemented.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCanary {
    count: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    max_updating: Option<u32>,
    min_up: Option<u32>,
    #[serde(default)]
    never_last: bool,
    #[serde(default)]
    after: Vec<String>,
}

fn invalid<T>(why: String) -> Result<T, ServicesError> {
    Err(ServicesError::Invalid(why))
}

/// The one place that says which role names a services file may use.
fn role_of(name: &str) -> Result<String, ServicesError> {
    // The loader names the four roles `kbf` knows and no others, however well formed.
    name.parse::<HostRole>()
        .ok()
        .filter(|role| kbf_roles().contains(role))
        .map_or_else(
            || invalid(format!("{name} is not a host role")),
            |role| Ok(role.name().to_owned()),
        )
}

fn role_def(
    service: &str,
    raw: RawRole,
    roles: &BTreeSet<String>,
) -> Result<RoleDef, ServicesError> {
    if [raw.max_updating, raw.min_up]
        .into_iter()
        .flatten()
        .any(|n| n > MAX_COUNT)
    {
        return invalid(format!("{service}: a count is over {MAX_COUNT}"));
    }
    let after = raw
        .after
        .iter()
        .map(|name| {
            let role = role_of(name)?;
            if roles.contains(&role) {
                Ok(role)
            } else {
                invalid(format!(
                    "{service}: after names {name}, a role it does not have"
                ))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(RoleDef {
        max_updating: raw.max_updating,
        min_up: raw.min_up,
        never_last: raw.never_last,
        after,
    })
}

fn service_def(
    raw: RawService,
    defined: &BTreeMap<String, ServiceDef>,
) -> Result<ServiceDef, ServicesError> {
    let RawService {
        name,
        after,
        roles,
        priority,
        selector,
        install_when_absent,
        bake_beats,
        canary,
        rollback,
        rollback_after_failed,
        min_version,
        defer_backoff,
    } = raw;
    if !is_service_name(&name) {
        return invalid(format!(
            "service name {name:?} is not 1 to {MAX_SERVICE_NAME} bytes of lowercase letters, digits and -"
        ));
    }
    if defined.contains_key(&name) {
        return invalid(format!("{name} is defined twice"));
    }
    defer_backoff
        .check()
        .or_else(|why| invalid(format!("{name}: defer_backoff: {why}")))?;
    if roles.is_empty() {
        return invalid(format!("{name} has no roles"));
    }
    if let Some(unknown) = after
        .iter()
        .find(|a| *a != KBF && !defined.contains_key(*a))
    {
        return invalid(format!(
            "{name}: after names {unknown}, which is not kbf or defined earlier in the file"
        ));
    }
    if bake_beats > MAX_BAKE_BEATS {
        return invalid(format!("{name}: bake_beats is over {MAX_BAKE_BEATS}"));
    }
    if canary
        .as_ref()
        .is_some_and(|c| c.count == 0 || c.count > MAX_COUNT)
    {
        return invalid(format!("{name}: canary count is not 1 to {MAX_COUNT}"));
    }
    if !(1..=100).contains(&rollback_after_failed.percent) {
        return invalid(format!(
            "{name}: rollback_after_failed percent is not 1 to 100"
        ));
    }
    if !(1..=MAX_COUNT).contains(&rollback_after_failed.min_hosts) {
        return invalid(format!(
            "{name}: rollback_after_failed min_hosts is not 1 to {MAX_COUNT}"
        ));
    }
    let names = roles
        .keys()
        .map(|r| role_of(r))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let selector = selector
        .map(|raw| Selector::new(raw, &names))
        .transpose()
        .or_else(|why| invalid(format!("{name}: selector {why}")))?;
    let roles = roles
        .into_iter()
        .map(|(r, raw)| Ok((role_of(&r)?, role_def(&name, raw, &names)?)))
        .collect::<Result<_, ServicesError>>()?;
    Ok(ServiceDef {
        name,
        roles,
        after,
        require_min_up: Vec::new(),
        priority,
        selector,
        install_when_absent,
        bake_beats,
        canary: canary.map(|c| Canary { count: c.count }),
        rollback,
        rollback_after_failed,
        min_version,
        defer_backoff,
    })
}

/// The definition of `kbf` in `raw`: a service that waits for no service, and whose desired
/// state must name `min_up` for [`KBF_MIN_UP`] (kbf's contract with CI and deploy-farm; not
/// a thing a file sets).
fn kbf_def(raw: RawService) -> Result<ServiceDef, ServicesError> {
    if !raw.after.is_empty() {
        return invalid(format!("{KBF} waits for no service"));
    }
    let mut def = service_def(raw, &BTreeMap::new())?;
    def.require_min_up = KBF_MIN_UP.map(str::to_owned).into();
    Ok(def)
}

/// Every service the coordinator manages, and the ones CI's token may set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Services {
    defs: BTreeMap<String, ServiceDef>,
}

impl Default for Services {
    /// `kbf`, as shipped, and nothing else.
    fn default() -> Self {
        Self {
            defs: BTreeMap::from([(KBF.to_owned(), ServiceDef::kbf())]),
        }
    }
}

impl Services {
    /// These services and `def`, for a test that needs a role the loader refuses.
    #[cfg(test)]
    pub(crate) fn with(mut self, def: ServiceDef) -> Self {
        self.defs.insert(def.name.clone(), def);
        self
    }

    /// The services of the file text `text`, with `kbf` as shipped unless the first entry of
    /// the text defines it.
    ///
    /// # Errors
    /// [`ServicesError`]: the text is not TOML of the shape, a name is bad or repeated (`kbf`
    /// only as the first entry, once), a service has no roles, `kbf` has an `after`, there
    /// are more than [`MAX_SERVICES`] services, a role name or count is unusable, or an
    /// `after` names what does not exist (or is defined later, which also forbids a cycle).
    pub fn parse(text: &str) -> Result<Self, ServicesError> {
        let file: RawFile =
            toml::from_str(text).map_err(|e| ServicesError::Parse(e.to_string()))?;
        if file.service.len() > MAX_SERVICES {
            return invalid(format!("more than {MAX_SERVICES} services"));
        }
        let mut entries = file.service.into_iter().peekable();
        let replacement = entries.next_if(|raw| raw.name == KBF);
        let replaced = replacement.is_some();
        let kbf = match replacement {
            Some(raw) => kbf_def(raw)?,
            None => ServiceDef::kbf(),
        };
        let mut services = Self {
            defs: BTreeMap::from([(KBF.to_owned(), kbf)]),
        };
        for raw in entries {
            if raw.name == KBF {
                return invalid(if replaced {
                    "kbf is defined twice".to_owned()
                } else {
                    "kbf must be the first entry of the file (it is defined after another service)"
                        .to_owned()
                });
            }
            let def = service_def(raw, &services.defs)?;
            services.defs.insert(def.name.clone(), def);
        }
        Ok(services)
    }

    /// The services in the file at `path`. The file is read under the rule of the operator's
    /// token file: a regular file, not a symlink, owned by this process's user, mode `0600`
    /// or `0400`. Whoever can write it controls kbf's rollout limits (a replaced `kbf` can
    /// drop every gate), so it is held to the same rule as a secret.
    ///
    /// # Errors
    /// [`ServicesError`]: the file fails the rule, cannot be read, is over [`MAX_FILE_BYTES`],
    /// or its text is refused by [`Self::parse`].
    pub fn from_file(path: &Path) -> Result<Self, ServicesError> {
        let shown = path.display().to_string();
        let bytes = read_secret(path, &shown, MAX_FILE_BYTES).map_err(ServicesError::Read)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(ServicesError::Read(format!(
                "{shown}: it is over {MAX_FILE_BYTES} bytes"
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ServicesError::Read(format!("{shown}: it is not UTF-8")))?;
        Self::parse(&text)
    }

    /// The services `defs` and nothing else (not even `kbf`).
    #[cfg(test)]
    pub(crate) fn from_defs(defs: Vec<ServiceDef>) -> Self {
        Self {
            defs: defs
                .into_iter()
                .map(|def| (def.name.clone(), def))
                .collect(),
        }
    }

    /// Every service the coordinator manages, by name.
    pub fn defs(&self) -> impl Iterator<Item = &ServiceDef> {
        self.defs.values()
    }

    /// The names of every service the coordinator manages, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.defs.keys().map(String::as_str)
    }

    /// The service `name`, if the coordinator manages it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ServiceDef> {
        self.defs.get(name)
    }
}
