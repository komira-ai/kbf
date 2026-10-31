//! Loading the declarations of a host: `/etc/updater.d/*.toml`.
//!
//! The rules, from docs/design/host-updates.md sections 2.4 and 3.3:
//!
//! - the directory and each file are owned by the trusted account (root) and not writable by
//!   the group or others, and a file is a regular file, not a link;
//! - the text is a declaration that every table of which accepts only the keys it knows, and
//!   keeps the rules of `Declaration::check`;
//! - the file is named `<name>.toml`;
//! - the accounts exist, and `hook.user` is not the service's account unless it is root;
//! - no path, once links are resolved, breaks the reserved-path rules;
//! - `state_dir` is not on a path the service's account can write;
//! - no two declarations share a service account (root is exempt: several services run as root),
//!   or a non-root hook account, and no hook runs as another service's account; a unit, a health port or a record directory.
//!
//! **A declaration that fails a rule refuses that one service**, which becomes `unmanaged`
//! and raises an alert; the others load. A conflict has two parties and neither has the better
//! claim, so both are refused.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::accounts::{Accounts, UserIds};
use crate::declaration::{Declaration, Refusal};
use crate::digest::Digest;
use crate::reserved::{self, Kind, is_within};
use crate::trust::{check_dir, read_trusted};

/// The directory the declarations are installed in.
pub const DECLARATION_DIR: &str = "/etc/updater.d";

/// What a load trusts and asks.
pub struct Policy<'a> {
    /// The account that must own the directory and the files: root, 0.
    pub trusted_uid: u32,
    pub accounts: &'a dyn Accounts,
}

/// A declaration that passed every rule.
#[derive(Clone, Debug)]
pub struct Declared {
    pub declaration: Declaration,
    /// The digest of the exact bytes of the file: what `approve` names.
    pub digest: Digest,
    pub file: PathBuf,
    pub service: UserIds,
    /// The ids of the hook's account.
    pub hook: UserIds,
    /// The service account's primary group and the groups that list it.
    pub service_gids: Vec<u32>,
}

impl Declared {
    /// [`recheck_targets`] for this declaration, with the uid of the hook account the loader
    /// resolved: the one way to recheck that cannot be told the wrong answer.
    pub fn recheck(&self) -> Result<(), Refusal> {
        recheck_targets(&self.declaration, self.hook.uid)
    }
}

/// A declaration that was refused, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub file: PathBuf,
    /// The service the file is named for.
    pub service: String,
    pub reason: Refusal,
}

/// What a directory held.
#[derive(Debug, Default)]
pub struct Loaded {
    pub services: BTreeMap<String, Declared>,
    pub refused: Vec<Refused>,
}

/// Whether the account `uid`, with groups `gids`, can write to a thing with this owner and mode.
fn writable_by(meta: &std::fs::Metadata, uid: u32, gids: &[u32]) -> bool {
    let mode = meta.mode();
    [
        (meta.uid() == uid) & (mode & 0o200 != 0),
        gids.contains(&meta.gid()) & (mode & 0o020 != 0),
        mode & 0o002 != 0,
    ]
    .into_iter()
    .any(|writable| writable)
}

/// Checks that the service's account cannot write `state_dir`, nor any directory above it
/// (which could replace it): by owner, by group and by others. Directories that do not exist
/// yet are skipped; the updater creates the record directory. A service that runs as root is
/// not checked, because nothing keeps root from writing anything.
///
/// The loader calls this once; the step runner calls it again at the start of every step.
pub fn check_state_dir(state_dir: &str, service: u32, service_gids: &[u32]) -> Result<(), Refusal> {
    if service == 0 {
        return Ok(());
    }
    for ancestor in Path::new(state_dir).ancestors() {
        if let Ok(meta) = std::fs::metadata(ancestor)
            && writable_by(&meta, service, service_gids)
        {
            return Err(Refusal::StateDirWritable {
                path: ancestor.display().to_string(),
            });
        }
    }
    Ok(())
}

/// The reserved-path rules again, on what each entry points to when it exists.
fn check_targets(declaration: &Declaration, root_hook: bool) -> Result<(), Refusal> {
    let hook = &declaration.hook;
    let entries = [(Kind::State, "state_dir", &declaration.state_dir)]
        .into_iter()
        .chain(
            hook.read_paths
                .iter()
                .map(|p| (Kind::Read, "hook.read_paths", p)),
        )
        .chain(
            hook.write_paths
                .iter()
                .map(|p| (Kind::Write, "hook.write_paths", p)),
        );
    for (kind, field, entry) in entries {
        // A hook that runs as root follows links as root: nothing above a path it reads or writes may
        // be swapped by anyone else.
        let guarded = root_hook & (kind != Kind::State);
        let own = |path: &str| {
            (root_hook & (kind == Kind::Write))
                .then(|| reserved::check_service_write(path, &declaration.name).err())
                .flatten()
                .map(|why| Refusal::Reserved {
                    field,
                    path: path.to_owned(),
                    why,
                })
        };
        let swappable = |path: &str| {
            guarded
                .then(|| swappable_above(path))
                .flatten()
                .map(|ancestor| Refusal::Ancestor {
                    field,
                    path: path.to_owned(),
                    ancestor,
                })
        };
        if let Some(refusal) = own(entry).or_else(|| swappable(entry)) {
            return Err(refusal);
        }
        if let Ok(target) = std::fs::canonicalize(entry) {
            let target = target.to_string_lossy().into_owned();
            reserved::check(kind, &target).map_err(|why| Refusal::Reserved {
                field,
                path: format!("{entry} (which is {target})"),
                why,
            })?;
            if let Some(refusal) = own(&target).or_else(|| swappable(&target)) {
                return Err(refusal);
            }
        }
    }
    Ok(())
}

/// The nearest directory above `path` that an account other than root owns or can write (the
/// group or others): the one that could replace `path` with a link. Directories that do not
/// exist yet are skipped.
fn swappable_above(path: &str) -> Option<String> {
    Path::new(path).ancestors().skip(1).find_map(|above| {
        let meta = std::fs::metadata(above).ok()?;
        ((meta.uid() != 0) | (meta.mode() & 0o022 != 0)).then(|| above.display().to_string())
    })
}

/// The reserved-path rules on what each entry points to now, for the step runner to call before
/// every launch: a link can be re-pointed after the load.
pub fn recheck_targets(declaration: &Declaration, hook_uid: u32) -> Result<(), Refusal> {
    check_targets(declaration, hook_uid == 0)
}

fn account_error(what: &str, name: &str) -> Refusal {
    Refusal::Account(format!("{what} {name:?} is not an account of this host"))
}

fn load_file(path: &Path, stem: &str, policy: &Policy<'_>) -> Result<Declared, Refusal> {
    let bytes = read_trusted(path, policy.trusted_uid).map_err(|e| Refusal::File(e.to_string()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Refusal::Syntax("not UTF-8".to_owned()))?;
    let declaration = Declaration::parse(text)?;
    if declaration.name != stem {
        return Err(Refusal::Value {
            field: "name".to_owned(),
            why: format!("{:?} is in a file named for {stem:?}", declaration.name),
        });
    }
    let accounts = policy.accounts;
    let service = accounts
        .user(&declaration.uid)
        .ok_or_else(|| account_error("uid", &declaration.uid))?;
    let hook_user = accounts
        .user(&declaration.hook.user)
        .ok_or_else(|| account_error("hook.user", &declaration.hook.user))?;
    accounts
        .group(&declaration.hook.group)
        .ok_or_else(|| account_error("hook.group", &declaration.hook.group))?;
    if (hook_user.uid != 0) & (hook_user.uid == service.uid) {
        return Err(Refusal::Account(
            "hook.user is the service's own account, which can then write the hook's record"
                .to_owned(),
        ));
    }
    let root_hook = hook_user.uid == 0;
    // The text rule on strings is by name; an account of uid 0 under another name is held to it too.
    if root_hook & declaration.hook.params.values().any(toml::Value::is_str) {
        return Err(Refusal::Value {
            field: "hook.params".to_owned(),
            why: "a hook that runs as root takes numbers and booleans, not strings".to_owned(),
        });
    }
    check_targets(&declaration, root_hook)?;
    let mut service_gids = accounts.member_of(&declaration.uid);
    service_gids.push(service.gid);
    check_state_dir(&declaration.state_dir, service.uid, &service_gids)?;
    Ok(Declared {
        digest: Digest::of(&bytes),
        file: path.to_owned(),
        declaration,
        service,
        hook: hook_user,
        service_gids,
    })
}

fn nested(a: &str, b: &str) -> bool {
    is_within(a, b) | is_within(b, a)
}

/// Whether `a` can write into the record directory of `b`, or a directory around it.
fn writes_into(a: &Declaration, b: &Declaration) -> bool {
    a.hook.write_paths.iter().any(|w| nested(w, &b.state_dir))
}

/// Whether the hook of `a` runs as the service account of `b`, root excepted.
fn owns(a: &Declared, b: &Declared) -> bool {
    (a.hook.uid == b.service.uid) & (a.hook.uid != 0)
}

/// What the two declarations fight over, if anything. Symmetric.
fn conflict(a: &Declared, b: &Declared) -> Option<&'static str> {
    let (x, y) = (&a.declaration, &b.declaration);
    [
        (
            "the service's account",
            (a.service.uid == b.service.uid) & (a.service.uid != 0),
        ),
        ("a unit", !x.hook.units.is_disjoint(&y.hook.units)),
        (
            "a health port",
            !x.health_ports.is_disjoint(&y.health_ports),
        ),
        ("the record directory", nested(&x.state_dir, &y.state_dir)),
        (
            "a write path into the record directory",
            writes_into(x, y) | writes_into(y, x),
        ),
        (
            "a hook account that is another service's account",
            owns(a, b) | owns(b, a),
        ),
        (
            "a hook account",
            (a.hook.uid == b.hook.uid) & (a.hook.uid != 0),
        ),
    ]
    .into_iter()
    .find_map(|(what, hit)| hit.then_some(what))
}

/// Loads every `*.toml` file of `dir`. Only a directory that cannot be listed is an error;
/// everything wrong with a declaration is a [`Refused`].
pub fn load_dir(dir: &Path, policy: &Policy<'_>) -> Result<Loaded, io::Error> {
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".toml"))
            .map(str::to_owned);
        files.extend(stem.map(|stem| (path, stem)));
    }
    files.sort();
    let dir_problem = check_dir(dir, policy.trusted_uid).err();
    let mut loaded = Loaded::default();
    let mut good = Vec::new();
    for (path, stem) in files {
        let outcome = match &dir_problem {
            Some(problem) => Err(Refusal::DirectoryUnsafe(format!(
                "{}: {problem}",
                dir.display()
            ))),
            None => load_file(&path, &stem, policy),
        };
        match outcome {
            Ok(declared) => good.push(declared),
            Err(reason) => loaded.refused.push(Refused {
                file: path,
                service: stem,
                reason,
            }),
        }
    }
    for declared in &good {
        let other = good
            .iter()
            .filter(|b| b.declaration.name != declared.declaration.name)
            .find_map(|b| conflict(declared, b).map(|what| (what, b.declaration.name.clone())))
            .or_else(|| trespass(declared, &good));
        match other {
            Some((what, with)) => loaded.refused.push(Refused {
                file: declared.file.clone(),
                service: declared.declaration.name.clone(),
                reason: Refusal::Conflict { what, with },
            }),
            None => {
                loaded
                    .services
                    .insert(declared.declaration.name.clone(), declared.clone());
            }
        }
    }
    loaded.refused.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(loaded)
}

/// A write path of a hook that runs as root in the directory of another declared service: the
/// first component under the write root belongs to the declared name it begins with, the longest
/// one, so `tierstore` may write `/var/lib/tierstore-state` and not `/var/lib/tierstore-b/x` when
/// `tierstore-b` is a service of this host. Returns what the two fight over and the owner.
fn trespass(declared: &Declared, good: &[Declared]) -> Option<(&'static str, String)> {
    if declared.hook.uid != 0 {
        return None;
    }
    let own = &declared.declaration.name;
    declared
        .declaration
        .hook
        .write_paths
        .iter()
        .find_map(|path| {
            let first = reserved::first_below_a_write_root(path)?;
            let owner = good
                .iter()
                .map(|g| g.declaration.name.as_str())
                .filter(|name| reserved::belongs_to(first, name))
                .max_by_key(|name| name.len())?;
            (owner != own).then(|| {
                (
                    "a write path in the directory of another service",
                    owner.to_owned(),
                )
            })
        })
}
