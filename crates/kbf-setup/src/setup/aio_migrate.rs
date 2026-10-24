//! Moving an all-in-one host of the earlier layout to the proxy layout.
//!
//! An earlier release ran an all-in-one host as one process, `kbf-all-in-one.service`, whose
//! `kbf.toml` says `mode = "all-in-one"`. `setup --mode=all-in-one --proxy` splits that file
//! into the files of the proxy layout instead of refusing it:
//!
//! - The original is kept first, root's 0600, in `migration-units/` of the installer's state
//!   directory (`kbf.toml.all-in-one`). A copy that is there is never overwritten, and one that
//!   differs from the file is refused: neither may be lost.
//! - Its settings travel as flags. A flag this run does not give takes the old file's value (the
//!   cold store, the metadata directory, the identity, the REAPI pair, the worker client CA and
//!   `[scheduler] state-store`), and a flag that is given overrides it, except `--cold-store` and
//!   `--meta-dir`: those two must name the file's place, or the sibling would start on another
//!   metadata directory than the cold store's ([`Earlier::adopt`] refuses). The run is then
//!   validated, planned and written as a fresh host's is: the stores, the identity and the daemon
//!   driver go to the sibling's `kbf.toml`, the state store to `frontend.toml`, the REAPI pair and
//!   the client CA to the `[proxy]` table of `installer.toml`. The meta directory and the cold
//!   store are unchanged.
//! - The operator token stays the same token. The earlier layout's belonged to the sibling's
//!   user and the proxy's children could not read it, so the file is rewritten as theirs; a token
//!   at another path than the layout's is copied to it, and when both files are there and differ
//!   the layout's is kept and the output says the other is not used ([`Earlier::keep_token`]).
//! - A key with no place in the new layout (the listeners, the worker listener's certificate, a
//!   setting the operator added) is named in the output, never printed and never dropped
//!   without saying so; the original keeps it.
//! - None of the services of the move is stopped or started. The earlier unit drains without a
//!   time limit and holds the metadata directory the sibling needs, so setup prints the commands,
//!   in order, and the operator runs them (as for `kbf-frontend.service` beside the proxy). Setup
//!   enables `kbf-installer.service` and starts it, as on every run. The earlier unit must not
//!   be restarted once `kbf.toml` is replaced, so the generic note that a running kbf keeps the
//!   old file is not printed for the move.
//!
//! Only a file that still says `all-in-one` is moved, so a second run on a moved host is the
//! fresh host's: it plans nothing new, does not look at the copy, and (as for any fresh host)
//! needs every flag, since the moved files are not read for values. Values from the file reach
//! the other modules only as flags and settings; no message carries one.

use kbf_host::{Entry, Exec, HostPath, Owner};
use toml::{Table, Value};

use super::SetupError;
use super::kbf_settings::all_in_one_writes;
use super::kbf_toml::all_in_one_table;
use super::paths;
use super::plan::{self, Ctx, Pl, dir, file};
use super::validate::Raw;
use crate::layout::Platform;

/// Keys of the earlier file that the proxy layout writes, besides those of the flags: the state
/// store (to `frontend.toml`) and the operator token (carried to the layout's own path).
const CARRIED: [(&str, &str); 2] = [("scheduler", "state-store"), ("tls", "api-token-file")];

/// The `[stores]` keys (and flags) that the move keeps as the file has them.
const LOCATIONS: [&str; 2] = ["cold-store", "meta-dir"];

/// The `kbf.toml` of the earlier layout, as it is on the host. It holds the whole file, whose
/// `state-store` can carry a password, so it has no `Debug`.
pub struct Earlier {
    path: &'static str,
    /// The file's bytes: what the copy holds.
    bytes: Vec<u8>,
    table: Table,
}

/// The earlier layout's `kbf.toml` when the host has it: a regular file whose top-level `mode` is
/// `all-in-one`. Reads the host, changes nothing.
///
/// # Errors
/// The file cannot be looked at.
pub fn read(exec: &dyn Exec, platform: Platform) -> Result<Option<Earlier>, SetupError> {
    let path = paths::kbf_toml(platform);
    let Some(Entry::File { bytes, .. }) = exec.inspect(&HostPath::new(path)?)? else {
        return Ok(None);
    };
    let table = all_in_one_table(&String::from_utf8_lossy(&bytes));
    Ok(table.map(|table| Earlier { path, bytes, table }))
}

/// The problems of the flags as `Invalid` lists them. On a host being moved a value that was not
/// given as a flag came from the earlier file, which the last line says.
pub fn invalid(mut problems: Vec<String>, moving: bool) -> SetupError {
    if moving {
        problems.push(
            "a value not given as a flag is the earlier all-in-one kbf.toml's; a flag of the same name overrides it"
                .to_owned(),
        );
    }
    SetupError::Invalid(problems)
}

/// The file at `path`, when it is a regular file.
fn bytes_of(exec: &dyn Exec, path: &str) -> Result<Option<Vec<u8>>, SetupError> {
    let found = exec.inspect(&HostPath::new(path)?)?;
    Ok(match found {
        Some(Entry::File { bytes, .. }) => Some(bytes),
        _ => None,
    })
}

/// Whether the proxy layout has a place for `[table] key = value` of the earlier file.
fn carried(table: &str, key: &str, value: &Value) -> bool {
    let placed = all_in_one_writes(table, key) || CARRIED.contains(&(table, key));
    placed && value.is_str()
}

impl Earlier {
    /// The text value of `[table] key`; none when it is not there or not text.
    fn value(&self, table: &str, key: &str) -> Option<&str> {
        let inner = self.table.get(table).and_then(Value::as_table);
        inner.and_then(|t| t.get(key)).and_then(Value::as_str)
    }

    /// Where the file says the operator token is.
    fn token_file(&self) -> Option<&str> {
        self.value("tls", "api-token-file")
    }

    /// Fills the flags this run did not give from the file, so that the move is validated as a
    /// fresh host's run with all its flags. `--proxy` is required, as for every all-in-one host.
    /// `--cold-store` and `--meta-dir` may be given only with the file's own value: the move
    /// keeps both where they are, and a sibling on another metadata directory would start with
    /// no log of the cold store it is given. Neither value is in the message.
    ///
    /// # Errors
    /// The run does not give `--proxy`, or gives a cold store or metadata directory other than
    /// the file's.
    pub fn adopt(&self, raw: &mut Raw) -> Result<(), SetupError> {
        if !raw.behind_proxy() {
            return Err(SetupError::NeedsOperator(format!(
                "{} says mode = \"all-in-one\", the layout of one process for everything that an earlier release set up: give --proxy and setup splits it into the proxy layout (the proxy, a frontend child and the kbf-storage-and-daemon sibling), keeping the file; it stops and starts none of the services of the move",
                self.path
            )));
        }
        for key in LOCATIONS {
            let both = raw.kbf.get(key).zip(self.value("stores", key));
            if both.is_some_and(|(given, old)| given != old) {
                return Err(SetupError::NeedsOperator(format!(
                    "--{key} names another place than {} does: the move keeps the cold store and the metadata directory where they are, since a sibling on another metadata directory starts with no log of the cold store; give the file's value or leave the flag out",
                    self.path
                )));
            }
        }
        let carry = |table: &str, key: &str| self.value(table, key).map(str::to_owned);
        raw.kbf.fill_from(carry);
        raw.database = raw.database.take().or(carry("scheduler", "state-store"));
        Ok(())
    }

    /// The keys of the file that the proxy layout has no place for, as `[table] key` (a top-level
    /// key by its name): the listeners, the worker listener's certificate, anything the operator
    /// added, and a key of the layout that is not text.
    pub fn unplaced(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (table, value) in &self.table {
            let Value::Table(inner) = value else {
                if table != "mode" {
                    names.push(table.clone());
                }
                continue;
            };
            for (key, v) in inner {
                if !carried(table, key, v) {
                    names.push(format!("[{table}] {key}"));
                }
            }
        }
        names
    }

    /// Refuses a copy of an all-in-one `kbf.toml` that is already kept and is not this file: the
    /// first original stays, a second is never written over it, and neither is lost. Asked
    /// before anything is planned, so a refusal leaves the host as it was. Reads the host.
    ///
    /// # Errors
    /// The copy holds other bytes than the file.
    pub fn check(&self, exec: &dyn Exec, ctx: &Ctx) -> Result<(), SetupError> {
        let kept = ctx.layout.all_in_one_kbf_toml();
        let there = bytes_of(exec, kept.as_str())?;
        if there.is_some_and(|bytes| bytes != self.bytes) {
            return Err(SetupError::NeedsOperator(format!(
                "{kept} holds another all-in-one kbf.toml than {} does now: setup keeps the first original and overwrites neither; move one of them aside, then run setup again",
                self.path
            )));
        }
        Ok(())
    }

    /// Plans the copy of the original and tells the operator what setup does not do. The file
    /// the run replaces comes later in the plan, so a crash between the two leaves the original
    /// in place and the copy made; a run again finds the same bytes kept and plans nothing for
    /// them ([`Earlier::check`] has refused any other).
    ///
    /// # Errors
    /// A step cannot be planned.
    pub fn plan(
        &self,
        p: &mut Pl<'_, '_>,
        ctx: &Ctx,
        notes: &mut Vec<String>,
    ) -> Result<(), SetupError> {
        let kept = ctx.layout.all_in_one_kbf_toml();
        dir(p, ctx.layout.migration_units().as_str(), 0o700, ctx.root)?;
        file(p, kept.as_str(), 0o600, ctx.root, self.bytes.clone())?;
        let unplaced = self.unplaced();
        if !unplaced.is_empty() {
            notes.push(format!(
                "{} holds settings the proxy layout has no place for, and the new files do not have them: {}; the original is kept in {kept}",
                self.path,
                unplaced.join(", ")
            ));
        }
        notes.extend(commands(self.path, &kept.to_string()));
        Ok(())
    }

    /// Keeps the operator token the earlier layout had: the layout's own file if it is there,
    /// else the one `[tls] api-token-file` names. It is rewritten as `owner`'s (the proxy's
    /// children; the earlier layout's belonged to the sibling's user), mode 0600, with the bytes
    /// it has; a step is planned only where the file differs. When both files are there and hold
    /// other bytes, the layout's wins and a note names the other file (never its bytes) as not
    /// used. False when there is no token to keep, and the caller makes the first one.
    ///
    /// # Errors
    /// A file cannot be looked at or planned.
    pub fn keep_token(
        &self,
        p: &mut Pl<'_, '_>,
        exec: &dyn Exec,
        owner: Owner,
        platform: Platform,
        notes: &mut Vec<String>,
    ) -> Result<bool, SetupError> {
        let token = paths::api_token(platform);
        let old = match self.token_file() {
            Some(old) if old != token => bytes_of(exec, old)?.map(|bytes| (old, bytes)),
            _ => None,
        };
        let (bytes, from) = match (bytes_of(exec, token)?, old) {
            (Some(bytes), Some((old, other))) => {
                if other != bytes {
                    notes.push(format!(
                        "the operator token of {old} differs from {token}: the proxy layout reads {token}, which is kept, and {old} is not used; setup does not change it"
                    ));
                }
                (bytes, None)
            }
            (Some(bytes), None) => (bytes, None),
            (None, Some((old, bytes))) => (bytes, Some(old)),
            (None, None) => return Ok(false),
        };
        file(p, token, 0o600, owner, bytes)?;
        if let Some(old) = from {
            notes.push(format!(
                "the operator token of {old} is copied to {token}, where the proxy layout reads it; setup does not remove the old file"
            ));
        }
        Ok(true)
    }
}

/// What the operator runs to finish the move, in order, one note each. Setup runs none of it: the
/// earlier unit's drain has no time limit, and until it has exited it holds the metadata
/// directory, which the sibling needs.
fn commands(path: &str, kept: &str) -> [String; 5] {
    let old = plan::EARLIER_ALL_IN_ONE_UNIT;
    let sibling = plan::STORAGE_AND_DAEMON_UNIT;
    let (socket, service) = (plan::PROXY_UNITS[0], plan::PROXY_UNITS[1]);
    [
        format!(
            "{path} is split and its original kept in {kept}; setup stops and starts none of the services of the move (it enables kbf-installer.service and starts it, as on any run), and until the steps below are done {old} and the new units are all enabled, so do not reboot the host in between. Do not restart {old}: {path} is no longer the file it was written for. Run, in this order:"
        ),
        format!(
            "migration 1/4: systemctl stop {old}, then wait until it has exited (systemctl is-active {old} says inactive): it drains with no time limit"
        ),
        format!("migration 2/4: systemctl disable {old}"),
        "migration 3/4: systemctl daemon-reload".to_owned(),
        format!(
            "migration 4/4: systemctl start {sibling}, then systemctl start {socket} {service}; a start that finds the metadata directory still held exits 75 and is retried"
        ),
    ]
}
