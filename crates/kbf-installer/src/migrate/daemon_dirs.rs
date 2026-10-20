//! The two keys that make a node take orders: `[daemon] update-dir` and `orders-dir` of the
//! host's `kbf.toml`.
//!
//! A node started without `--update-dir` updates by hand: it reports that and ignores every
//! order the frontend sends (`kbf-node`'s update link). `setup` writes both keys for a host
//! whose daemon takes orders ([`crate::setup::plan::takes_orders`]); a host that was adopted
//! has a `kbf.toml` the old tools wrote, so the migration adds them, once, to the file that is
//! there. Nothing else in the file moves: the keys are put in as lines (after the `[daemon]`
//! header, or in a `[daemon]` table at the end), the result is read back and compared with
//! what was there, and a file that does not read back as the old one plus the two keys is left
//! as it was. The values are the layout's ([`crate::layout::Layout::update_dir`] and
//! [`orders_dir`](crate::layout::Layout::orders_dir)), the same `setup` writes, so a host that
//! is then set up finds its file as it would have written it.
//!
//! The order spool is the node's: made here (0700, the node user's) before the keys are
//! written, since the node's own start could not make it inside root's directory, and a node
//! pointed at a spool it cannot make does not start. A host with the keys already (either
//! equal to the layout's) is left alone; a key that names another directory is the operator's
//! and is reported, never changed. Re-run: the keys are there, so nothing is done.
//!
//! The first original of the file is kept, byte for byte, beside the units' originals
//! (`migration-units/kbf.toml`), because a rollback puts the old layout back and an old `kbf`
//! may refuse keys it does not know: [`restore`] writes it back when the file is still the
//! original plus these keys, and leaves a file the operator has edited since.

use kbf_host::{Entry, Exec, HostPath, Mode, Owner, Perms, Step};
use toml::{Table, Value};

use super::error::MigrateError;
use super::{Migration, Report};
use crate::cli::Mode as KbfMode;
use crate::setup::kbf_toml::UPDATE_KEYS;
use crate::setup::plan::takes_orders;

/// What became of the file.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The keys were added (or, in a dry run, would be).
    Added,
    /// Both are there already, as the layout has them.
    Already,
    /// The host's mode has no update client in its daemon, or it is not a node.
    NotANode,
    /// There is no `kbf.toml`: `setup` writes one.
    NoFile,
    /// Nothing was changed; why, for the operator.
    Left(String),
}

/// The text of the keys, in the order of [`UPDATE_KEYS`].
fn lines(update_dir: &str, orders_dir: &str) -> [(&'static str, String); 2] {
    [
        (UPDATE_KEYS[0].1, update_dir.to_owned()),
        (UPDATE_KEYS[1].1, orders_dir.to_owned()),
    ]
}

/// Whether `line` is the header of the table `name` (`[name]`, white space and a comment
/// allowed), and not of an array of tables.
fn is_header(line: &str, name: &str) -> bool {
    let line = line.trim();
    let Some(inner) = line.strip_prefix('[') else {
        return false;
    };
    if inner.starts_with('[') {
        return false;
    }
    inner.split_once(']').is_some_and(|(head, rest)| {
        head.trim() == name && {
            let rest = rest.trim();
            rest.is_empty() || rest.starts_with('#')
        }
    })
}

/// `text` with `keys` in its `[daemon]` table: right after the header when there is one, else
/// in a table of their own at the end. Every other byte stays.
fn insert(text: &str, keys: &[(&'static str, String)]) -> String {
    let body: String = keys
        .iter()
        .map(|(key, value)| format!("{key} = \"{value}\"\n"))
        .collect();
    let mut out = String::with_capacity(text.len() + body.len() + 16);
    let mut done = false;
    for line in text.split_inclusive('\n') {
        out.push_str(line);
        if !done && is_header(line, "daemon") {
            if !line.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&body);
            done = true;
        }
    }
    if !done {
        // A node's file has a `mode` line, so it is never empty here.
        if !text.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str("[daemon]\n");
        out.push_str(&body);
    }
    out
}

/// The value of `[daemon] key`, `None` when it is not there, `Err` when it is not text or
/// `[daemon]` is not a table.
fn value<'t>(table: &'t Table, key: &str) -> Result<Option<&'t str>, ()> {
    let Some(daemon) = table.get("daemon") else {
        return Ok(None);
    };
    let daemon = daemon.as_table().ok_or(())?;
    match daemon.get(key) {
        None => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or(()),
    }
}

/// The keys of the layout that `table` lacks. `Err`: one is there and not the layout's, or not
/// text, which are the operator's.
fn missing(m: &Migration, table: &Table) -> Result<Vec<(&'static str, String)>, String> {
    let (update_dir, orders_dir) = (
        m.layout.update_dir().to_string(),
        m.layout.orders_dir().to_string(),
    );
    let mut missing = Vec::new();
    for (key, want) in lines(&update_dir, &orders_dir) {
        match value(table, key) {
            Err(()) => return Err(format!("[daemon] {key} is not text")),
            Ok(Some(have)) if have == want => {}
            Ok(Some(_)) => {
                return Err(format!(
                    "[daemon] {key} names another directory: the operator's, left as it is"
                ));
            }
            Ok(None) => missing.push((key, want)),
        }
    }
    Ok(missing)
}

/// Whether `new` is `old` and the two keys: nothing else added, changed or lost.
fn only_the_keys_added(old: &Table, new: &str, keys: &[(&'static str, String)]) -> bool {
    let Some(mut parsed) = new
        .parse::<Table>()
        .ok()
        .filter(|t| t.get("daemon").is_some_and(Value::is_table))
    else {
        return false;
    };
    let (wanted, empty) = parsed
        .get_mut("daemon")
        .and_then(Value::as_table_mut)
        .map_or((false, false), |daemon| {
            let wanted = keys
                .iter()
                .all(|(key, value)| daemon.get(*key).and_then(Value::as_str) == Some(value));
            for (key, _) in keys {
                daemon.remove(*key);
            }
            (wanted, daemon.is_empty())
        });
    if empty && !old.contains_key("daemon") {
        parsed.remove("daemon");
    }
    wanted && &parsed == old
}

/// Add the keys to the host's `kbf.toml`, or say why not. In a dry run (`mode`) nothing is
/// written and nothing is made.
///
/// # Errors
/// The host cannot be read or written.
pub fn add<E: Exec>(
    m: &Migration,
    exec: &mut E,
    mode: Mode,
    report: &mut Report,
) -> Result<Outcome, MigrateError> {
    let outcome = decide(m, exec, mode)?;
    match &outcome {
        Outcome::Added => report.line(
            mode,
            format!(
                "{}: [daemon] update-dir and orders-dir added, so the node takes orders (a running \
                 node reads them at its next start)",
                m.layout.kbf_toml()
            ),
        ),
        Outcome::Already => report.line(
            mode,
            format!(
                "{}: [daemon] update-dir and orders-dir are there",
                m.layout.kbf_toml()
            ),
        ),
        Outcome::Left(why) => report.warnings.push(format!(
            "{}: the node's update-dir and orders-dir were not added, and it updates by hand until \
             they are ({why})",
            m.layout.kbf_toml()
        )),
        Outcome::NotANode | Outcome::NoFile => {}
    }
    Ok(outcome)
}

fn decide<E: Exec>(m: &Migration, exec: &mut E, mode: Mode) -> Result<Outcome, MigrateError> {
    let path = m.layout.kbf_toml();
    let io = |e| MigrateError::io(path.to_string())(e);
    let (perms, owner, bytes) = match exec.inspect(&path).map_err(io)? {
        None => return Ok(Outcome::NoFile),
        Some(Entry::File {
            perms,
            owner,
            bytes,
        }) => (perms, owner, bytes),
        Some(_) => return Ok(Outcome::Left("it is not a regular file".to_owned())),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Outcome::Left("it is not text".to_owned()));
    };
    let Ok(table) = text.parse::<Table>() else {
        return Ok(Outcome::Left("it is not valid TOML".to_owned()));
    };
    let node = table
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|name| <KbfMode as clap::ValueEnum>::from_str(name, false).ok())
        .is_some_and(takes_orders);
    if !node {
        return Ok(Outcome::NotANode);
    }
    let missing = match missing(m, &table) {
        Ok(missing) if missing.is_empty() => return Ok(Outcome::Already),
        Ok(missing) => missing,
        Err(why) => return Ok(Outcome::Left(why)),
    };
    let new = insert(&text, &missing);
    if !only_the_keys_added(&table, &new, &missing) {
        return Ok(Outcome::Left(
            "the file could not be edited without moving something else".to_owned(),
        ));
    }
    let spool = spool_owner(m, owner);
    let Some(spool) = spool else {
        return Ok(Outcome::Left(format!(
            "no user {:?} to own the order spool",
            crate::setup::plan::service_user_name(KbfMode::Daemon)
        )));
    };
    if mode == Mode::Apply {
        let kept = kept(m);
        if exec.inspect(&kept).map_err(io)?.is_none() {
            let step = Step::WriteFile {
                path: kept,
                perms: Perms::new(0o600),
                owner: None,
                bytes: text.into_bytes(),
            };
            exec.apply(&step).map_err(io)?;
        }
        let orders = m.layout.orders_dir();
        write(exec, &orders, &path, perms, owner, spool, new.into_bytes()).map_err(io)?;
    }
    Ok(Outcome::Added)
}

/// Where the first original of `kbf.toml` is kept: beside the units' originals.
fn kept(m: &Migration) -> HostPath {
    HostPath::new(&format!(
        "{}/migration-units/kbf.toml",
        m.layout.state_dir()
    ))
    .expect("a layout directory and a plain name")
}

/// Put the original `kbf.toml` back, for a rollback: only if the file is still that original
/// plus the keys [`add`] put in; a file changed since is the operator's and is left, with a
/// warning. The kept original is removed once it is back, so a migration again keeps a fresh one.
///
/// # Errors
/// The host cannot be read or written.
pub fn restore<E: Exec>(
    m: &Migration,
    exec: &mut E,
    mode: Mode,
    report: &mut Report,
) -> Result<(), MigrateError> {
    let (path, kept) = (m.layout.kbf_toml(), kept(m));
    let io = |e| MigrateError::io(path.to_string())(e);
    let Some(Entry::File {
        bytes: original, ..
    }) = exec.inspect(&kept).map_err(io)?
    else {
        return Ok(());
    };
    let Some(Entry::File {
        perms,
        owner,
        bytes: now,
    }) = exec.inspect(&path).map_err(io)?
    else {
        return Ok(());
    };
    let (Ok(original_text), Ok(now_text)) = (String::from_utf8(original), String::from_utf8(now))
    else {
        return Ok(());
    };
    let Ok(before) = original_text.parse::<Table>() else {
        return Ok(());
    };
    // The keys that migration added are the ones the original lacked.
    let added = missing(m, &before).unwrap_or_default();
    if now_text == original_text {
        // Nothing to undo (it was never written, or has been put back).
    } else if added.is_empty() || !only_the_keys_added(&before, &now_text, &added) {
        report.warnings.push(format!(
            "{path} was changed after the migration and is left as it is; \
             remove [daemon] update-dir and orders-dir by hand if the old kbf refuses them"
        ));
        return Ok(());
    } else {
        report.line(mode, format!("{path}: the original is back"));
        exec.apply(&Step::WriteFile {
            path: path.clone(),
            perms,
            owner: Some(owner),
            bytes: original_text.into_bytes(),
        })
        .map_err(io)?;
    }
    exec.apply(&Step::Remove { path: kept }).map_err(io)?;
    Ok(())
}

/// Who owns the order spool: the node's user on Linux; on a Mac the daemon is root's, so the
/// owner of `kbf.toml`. `None` for a Linux host with no such user.
fn spool_owner(m: &Migration, file_owner: Owner) -> Option<Owner> {
    match m.layout.platform() {
        crate::layout::Platform::MacOs => Some(file_owner),
        crate::layout::Platform::Linux => m.node_user.as_ref().map(|u| Owner {
            uid: u.uid,
            gid: u.gid,
        }),
    }
}

/// The spool first, then the file: a crash between the two leaves a directory and no keys.
fn write<E: Exec>(
    exec: &mut E,
    orders: &HostPath,
    path: &HostPath,
    perms: Perms,
    owner: Owner,
    spool: Owner,
    bytes: Vec<u8>,
) -> std::io::Result<()> {
    exec.apply(&Step::Mkdir {
        path: orders.clone(),
        perms: Perms::new(0o700),
        owner: Some(spool),
    })?;
    exec.apply(&Step::WriteFile {
        path: path.clone(),
        perms,
        owner: Some(owner),
        bytes,
    })
}

#[cfg(test)]
#[path = "daemon_dirs_tests.rs"]
mod tests;
