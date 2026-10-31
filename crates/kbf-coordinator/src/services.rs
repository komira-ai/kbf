//! Services as data: what the coordinator updates, the roles each service runs on, and the
//! order between services (`docs/design/updates.md`, section 3).
//!
//! A [`ServiceDef`] has a name, the roles it runs on (each with its limits and the roles it
//! waits for) and `after`, the services whose live hosts must have converged before any host
//! of this one is ordered. The decision reads these and branches on no service name.
//!
//! `kbf`'s definition is built in ([`ServiceDef::kbf`]) and cannot be redefined. A file named
//! by `--services-file` defines further services. Which of them a CI token may set is not
//! here: the allow-list entry that admits the token says (`services = [...]`, default `kbf`).
//!
//! ```toml
//! [[service]]
//! name = "agent"
//! after = ["server"]             # services defined earlier in the file, or kbf
//! priority = 0                   # default 0; a host is offered the higher number first
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
use kbf_updater::wire::{MAX_SERVICE_NAME, MAX_SERVICES, is_service_name};
use serde::Deserialize;

use crate::desired::{Desired, KBF, KBF_MIN_UP, LEGACY_ROLES, MAX_COUNT};

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

    /// `kbf`: all four roles, limits from the desired state, and frontends that wait for
    /// every other role and are never the last one up.
    #[must_use]
    pub fn kbf() -> Self {
        let mut roles: BTreeMap<String, RoleDef> = LEGACY_ROLES
            .into_iter()
            .map(|role| (role.to_owned(), RoleDef::plain()))
            .collect();
        roles.insert(
            "frontend".to_owned(),
            RoleDef {
                never_last: true,
                after: ["storage", "daemon", "mac-daemon"]
                    .map(str::to_owned)
                    .into(),
                ..RoleDef::plain()
            },
        );
        Self {
            name: KBF.to_owned(),
            roles,
            after: Vec::new(),
            require_min_up: KBF_MIN_UP.map(str::to_owned).into(),
            priority: 0,
        }
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
    name.parse::<HostRole>()
        .map(|role| role.name().to_owned())
        .or_else(|_| invalid(format!("{name} is not a host role")))
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
    } = raw;
    if !is_service_name(&name) {
        return invalid(format!(
            "service name {name:?} is not 1 to {MAX_SERVICE_NAME} bytes of lowercase letters, digits and -"
        ));
    }
    if name == KBF || defined.contains_key(&name) {
        return invalid(format!("{name} is defined twice (kbf is built in)"));
    }
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
    let names = roles
        .keys()
        .map(|r| role_of(r))
        .collect::<Result<BTreeSet<_>, _>>()?;
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
    })
}

/// Every service the coordinator manages, and the ones CI's token may set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Services {
    defs: BTreeMap<String, ServiceDef>,
}

impl Default for Services {
    /// `kbf` only, and CI may set only `kbf`.
    fn default() -> Self {
        Self {
            defs: BTreeMap::from([(KBF.to_owned(), ServiceDef::kbf())]),
        }
    }
}

impl Services {
    /// The services of the file text `text`, with `kbf` built in.
    ///
    /// # Errors
    /// [`ServicesError`]: the text is not TOML of the shape, a name is bad or repeated, a
    /// service has no roles, there are more than [`MAX_SERVICES`] services, a role name or
    /// count is unusable, or an `after` names what does not exist (or is defined later, which
    /// also forbids a cycle).
    pub fn parse(text: &str) -> Result<Self, ServicesError> {
        let file: RawFile =
            toml::from_str(text).map_err(|e| ServicesError::Parse(e.to_string()))?;
        if file.service.len() > MAX_SERVICES {
            return invalid(format!("more than {MAX_SERVICES} services"));
        }
        let mut services = Self::default();
        for raw in file.service {
            let def = service_def(raw, &services.defs)?;
            services.defs.insert(def.name.clone(), def);
        }
        Ok(services)
    }

    /// The services in the file at `path`.
    ///
    /// # Errors
    /// [`ServicesError`].
    pub fn from_file(path: &Path) -> Result<Self, ServicesError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ServicesError::Read(format!("{}: {e}", path.display())))?;
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
