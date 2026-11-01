//! The declaration: `/etc/updater.d/<service>.toml`, typed.
//!
//! The field names and the tables are those of the example in docs/design/host-updates.md,
//! section 2.4. Every table refuses a key it does not know, so a misspelt or misplaced key
//! (`read_paths` at the top level, `verify_deadline_secs` under `[hook]`) refuses the file
//! instead of being ignored. [`Declaration::parse`] reads the text and applies the rules that
//! need nothing but the text; the rules that need the host (accounts, files, other
//! declarations) are the loader's (`crate::load`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use update_hook::{Params, Step};

use crate::names::{
    Capability, Family, PathError, SYSTEM_NAMES, UnitError, check_path, check_relative, check_unit,
    env_allowed, is_account_name, is_env_name, is_env_value, is_service_name,
};
use crate::reserved::{self, Kind};

/// The longest a step, a deadline or a registration may be given: one day.
pub const MAX_SECS: u64 = 86_400;
/// The longest promotion ceiling: one week.
pub const MAX_CEILING_SECS: u64 = 604_800;
/// The most times `cleanup` may be re-run.
pub const MAX_CLEANUP_RETRIES: u32 = 10;
/// The most leading arguments a hook may be given before the step.
pub const MAX_HOOK_ARGS: usize = 8;

fn default_hook_args() -> Vec<String> {
    vec!["update-hook".to_owned()]
}

const fn default_ttl() -> u64 {
    300
}

const fn default_retries() -> u32 {
    3
}

/// Whose attestation the artifact must carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    /// `owner/name` of the repository.
    pub repository: String,
    /// The file name of the workflow that signs.
    pub workflow: String,
}

/// The bound of each step, in seconds. All five are required: a step with no bound would run
/// for ever.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    pub prepare: u64,
    pub cutover: u64,
    pub verify: u64,
    pub rollback: u64,
    pub cleanup: u64,
}

impl Timeouts {
    pub fn get(&self, step: Step) -> u64 {
        match step {
            Step::Prepare => self.prepare,
            Step::Cutover => self.cutover,
            Step::Verify => self.verify,
            Step::Rollback => self.rollback,
            Step::Cleanup => self.cleanup,
        }
    }
}

/// The capabilities of each step. A step with no entry falls back to the hook-wide
/// `capabilities`; an entry with an empty list is a step that holds none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepCapabilities {
    pub prepare: Option<BTreeSet<Capability>>,
    pub cutover: Option<BTreeSet<Capability>>,
    pub verify: Option<BTreeSet<Capability>>,
    pub rollback: Option<BTreeSet<Capability>>,
    pub cleanup: Option<BTreeSet<Capability>>,
}

impl StepCapabilities {
    pub fn get(&self, step: Step) -> Option<&BTreeSet<Capability>> {
        match step {
            Step::Prepare => self.prepare.as_ref(),
            Step::Cutover => self.cutover.as_ref(),
            Step::Verify => self.verify.as_ref(),
            Step::Rollback => self.rollback.as_ref(),
            Step::Cleanup => self.cleanup.as_ref(),
        }
    }
}

/// The `[hook]` table: whom the hook runs as and with what rights.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// Not the service's account (its record must not be the service's to write), unless root.
    pub user: String,
    pub group: String,
    /// The hook-wide grant: the fallback of a step with no entry in `step_capabilities`.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    #[serde(default)]
    pub step_capabilities: StepCapabilities,
    #[serde(default)]
    pub read_paths: BTreeSet<String>,
    #[serde(default)]
    pub write_paths: BTreeSet<String>,
    /// The only units the hook may start, stop or mask.
    #[serde(default)]
    pub units: BTreeSet<String>,
    #[serde(default)]
    pub address_families: BTreeSet<Family>,
    /// Exactly these variables; nothing is inherited.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub timeout_secs: Timeouts,
    /// Opaque to the updater: written to a file and passed as `--params <file>`.
    #[serde(default)]
    pub params: toml::Table,
}

impl Hook {
    /// The capabilities the hook holds in `step`.
    pub fn capabilities_for(&self, step: Step) -> &BTreeSet<Capability> {
        self.step_capabilities
            .get(step)
            .unwrap_or(&self.capabilities)
    }
}

/// A declaration. The top-level keys come first in the file: in TOML a key after a table header
/// belongs to that table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub name: String,
    /// The hook executable, a path within the verified release.
    pub binary: String,
    /// Leading arguments before the step; `["update-hook"]` when absent.
    #[serde(default = "default_hook_args")]
    pub hook_args: Vec<String>,
    pub signer: Signer,
    /// The account the service runs as and registers as.
    pub uid: String,
    #[serde(default = "default_ttl")]
    pub registration_ttl_secs: u64,
    /// Loopback ports a registration may name; none for a service with no listener.
    #[serde(default)]
    pub health_ports: BTreeSet<u16>,
    /// After `cutover` succeeds, `verify` must succeed by then or the updater runs `rollback`.
    pub verify_deadline_secs: u64,
    /// The only ceiling on a pending promotion; none means none.
    pub promotion_ceiling_secs: Option<u64>,
    #[serde(default = "default_retries")]
    pub cleanup_retries: u32,
    /// The hook's record directory.
    pub state_dir: String,
    /// A boot-time entry point, run at the updater's start.
    pub boot: Option<String>,
    /// The first release that carries a hook.
    pub min_version: Option<String>,
    pub hook: Hook,
}

/// Why a declaration, or a set of them, was refused. A refusal makes one service `unmanaged`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("not a declaration: {0}")]
    Syntax(String),
    #[error("{field}: {why}")]
    Value { field: String, why: String },
    #[error("{field}: path {path:?} {why}")]
    Path {
        field: &'static str,
        path: String,
        why: PathError,
    },
    #[error("{field}: path {path:?} {why}")]
    Reserved {
        field: &'static str,
        path: String,
        why: reserved::Violation,
    },
    #[error("hook.units: {unit:?} {why}")]
    Unit { unit: String, why: UnitError },
    #[error(
        "verify_deadline_secs {deadline} is less than the verify and rollback bounds together ({needed})"
    )]
    Deadline { deadline: u64, needed: u64 },
    #[error("the file is not a regular file owned by the trusted account: {0}")]
    File(String),
    #[error("the directory {0} is not owned by the trusted account or is writable by others")]
    DirectoryUnsafe(String),
    #[error("{0}")]
    Account(String),
    #[error("the state directory {path:?} can be written by the service's account")]
    StateDirWritable { path: String },
    #[error(
        "{field}: path {path:?} is below {ancestor:?}, which an account other than root owns or can write, so it can be swapped for a link"
    )]
    Ancestor {
        field: &'static str,
        path: String,
        ancestor: String,
    },
    #[error("conflicts with {with} over {what}")]
    Conflict { what: &'static str, with: String },
}

fn value(field: &str, why: impl Into<String>) -> Refusal {
    Refusal::Value {
        field: field.to_owned(),
        why: why.into(),
    }
}

fn in_range(field: &str, number: u64, max: u64) -> Result<(), Refusal> {
    if (1..=max).contains(&number) {
        Ok(())
    } else {
        Err(value(field, format!("{number} is not within 1 to {max}")))
    }
}

fn plain_word(field: &str, word: &str) -> Result<(), Refusal> {
    let ok = (1..=64).contains(&word.len())
        & word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._=:/@+-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(value(
            field,
            format!("{word:?} is not 1 to 64 letters, digits and `._=:/@+-`"),
        ))
    }
}

fn account(field: &str, name: &str) -> Result<(), Refusal> {
    if is_account_name(name) {
        Ok(())
    } else {
        Err(value(field, format!("{name:?} is not an account name")))
    }
}

fn path(kind: Kind, field: &'static str, entry: &str) -> Result<(), Refusal> {
    check_path(entry).map_err(|why| Refusal::Path {
        field,
        path: entry.to_owned(),
        why,
    })?;
    reserved::check(kind, entry).map_err(|why| Refusal::Reserved {
        field,
        path: entry.to_owned(),
        why,
    })
}

impl Declaration {
    /// Reads the text of a declaration and applies every rule that needs nothing but the text.
    pub fn parse(text: &str) -> Result<Declaration, Refusal> {
        let declaration: Declaration =
            toml::from_str(text).map_err(|e| Refusal::Syntax(e.message().to_owned()))?;
        declaration.check()?;
        Ok(declaration)
    }

    /// The rules of the text alone: grammars, bounds, the reserved paths, the deadline.
    pub fn check(&self) -> Result<(), Refusal> {
        if !is_service_name(&self.name) {
            return Err(value(
                "name",
                format!("{:?} is not a service name", self.name),
            ));
        }
        if SYSTEM_NAMES.contains(&self.name.as_str()) {
            return Err(value(
                "name",
                format!("{:?} is the name of a directory of the system", self.name),
            ));
        }
        check_relative(&self.binary).map_err(|why| value("binary", format!("{why}")))?;
        if self.hook_args.len() > MAX_HOOK_ARGS {
            return Err(value(
                "hook_args",
                format!("more than {MAX_HOOK_ARGS} arguments"),
            ));
        }
        for arg in &self.hook_args {
            plain_word("hook_args", arg)?;
        }
        plain_word("signer.repository", &self.signer.repository)?;
        plain_word("signer.workflow", &self.signer.workflow)?;
        account("uid", &self.uid)?;
        account("hook.user", &self.hook.user)?;
        account("hook.group", &self.hook.group)?;
        in_range(
            "registration_ttl_secs",
            self.registration_ttl_secs,
            MAX_SECS,
        )?;
        if self.health_ports.contains(&0) {
            return Err(value("health_ports", "port 0 is not a port"));
        }
        if self.cleanup_retries > MAX_CLEANUP_RETRIES {
            return Err(value(
                "cleanup_retries",
                format!(
                    "{} is more than {MAX_CLEANUP_RETRIES}",
                    self.cleanup_retries
                ),
            ));
        }
        for (field, number) in [
            ("hook.timeout_secs.prepare", self.hook.timeout_secs.prepare),
            ("hook.timeout_secs.cutover", self.hook.timeout_secs.cutover),
            ("hook.timeout_secs.verify", self.hook.timeout_secs.verify),
            (
                "hook.timeout_secs.rollback",
                self.hook.timeout_secs.rollback,
            ),
            ("hook.timeout_secs.cleanup", self.hook.timeout_secs.cleanup),
            ("verify_deadline_secs", self.verify_deadline_secs),
        ] {
            in_range(field, number, MAX_SECS)?;
        }
        let needed = self.hook.timeout_secs.verify + self.hook.timeout_secs.rollback;
        if self.verify_deadline_secs < needed {
            return Err(Refusal::Deadline {
                deadline: self.verify_deadline_secs,
                needed,
            });
        }
        self.promotion_ceiling_secs.iter().try_for_each(|ceiling| {
            in_range("promotion_ceiling_secs", *ceiling, MAX_CEILING_SECS)
        })?;
        self.boot
            .iter()
            .try_for_each(|text| plain_word("boot", text))?;
        self.min_version
            .iter()
            .try_for_each(|text| plain_word("min_version", text))?;
        path(Kind::State, "state_dir", &self.state_dir)?;
        for entry in &self.hook.read_paths {
            path(Kind::Read, "hook.read_paths", entry)?;
        }
        for entry in &self.hook.write_paths {
            path(Kind::Write, "hook.write_paths", entry)?;
        }
        for unit in &self.hook.units {
            check_unit(&self.name, unit).map_err(|why| Refusal::Unit {
                unit: unit.clone(),
                why,
            })?;
        }
        for (key, text) in &self.hook.environment {
            if !(is_env_name(key) & is_env_value(text) & env_allowed(key, text)) {
                return Err(value(
                    "hook.environment",
                    format!("{key:?} is not a variable name with a plain value"),
                ));
            }
        }
        // The table is opaque, but it is scalars, and the `updater.` keys are the updater's.
        // The design allows a string only for a key a schema lists, and none exists: a hook that
        // runs as root takes numbers and booleans.
        if self.hook.user == "root" && self.hook.params.values().any(toml::Value::is_str) {
            return Err(value(
                "hook.params",
                "a hook that runs as root takes numbers and booleans, not strings",
            ));
        }
        Params::parse_declared(&self.hook.params.to_string())
            .map_err(|e| value("hook.params", e.to_string()))?;
        Ok(())
    }
}
