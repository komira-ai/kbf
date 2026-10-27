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
use serde::Deserialize;

use crate::desired::{Desired, KBF, MAX_COUNT};

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
    pub after: Vec<HostRole>,
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

    /// The limit on hosts of the role updating at once under `desired`.
    #[must_use]
    pub fn max_updating(&self, role: HostRole, desired: &Desired) -> usize {
        self.max_updating
            .unwrap_or_else(|| desired.max_updating.of(role)) as usize
    }

    /// How many other hosts of the role must be up while one updates under `desired`.
    #[must_use]
    pub fn required_up(&self, role: HostRole, desired: &Desired) -> usize {
        let floor = usize::from(self.never_last);
        (self.min_up.unwrap_or_else(|| desired.min_up.of(role)) as usize).max(floor)
    }
}

/// A service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDef {
    /// Its name.
    pub name: String,
    /// The roles it runs on. A host of any other role is never ordered for it.
    pub roles: BTreeMap<HostRole, RoleDef>,
    /// The services that must have converged before any host of this one is ordered.
    pub after: Vec<String>,
}

impl ServiceDef {
    /// `kbf`: all four roles, limits from the desired state, and frontends that wait for
    /// every other role and are never the last one up.
    #[must_use]
    pub fn kbf() -> Self {
        let mut roles: BTreeMap<HostRole, RoleDef> = HostRole::ALL
            .into_iter()
            .map(|role| (role, RoleDef::plain()))
            .collect();
        roles.insert(
            HostRole::Frontend,
            RoleDef {
                never_last: true,
                after: vec![HostRole::Storage, HostRole::Daemon, HostRole::MacDaemon],
                ..RoleDef::plain()
            },
        );
        Self {
            name: KBF.to_owned(),
            roles,
            after: Vec::new(),
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

fn role_of(name: &str) -> Result<HostRole, ServicesError> {
    name.parse()
        .or_else(|_| invalid(format!("{name} is not a host role")))
}

fn role_def(
    service: &str,
    raw: RawRole,
    roles: &BTreeSet<HostRole>,
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
    let RawService { name, after, roles } = raw;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return invalid(format!(
            "service name {name:?} is not lowercase letters, digits and -"
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
    Ok(ServiceDef { name, roles, after })
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
    /// service has no roles, a role name or count is unusable, an `after` names what does
    /// not exist (or is defined later, which also forbids a cycle).
    pub fn parse(text: &str) -> Result<Self, ServicesError> {
        let file: RawFile =
            toml::from_str(text).map_err(|e| ServicesError::Parse(e.to_string()))?;
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
