//! What the base's priming boot did of the simulator's root setup, and how a clone uses it.
//!
//! `kbf-guest sim` begins with root steps that only need to happen once per base: select
//! Xcode, accept its license, run its first launch, and import or add every staged runtime
//! (about 74 s on a clone, measured). The base's one priming boot (`image prime`, which
//! runs `kbf-guest mkuser`, [`crate::mkuser`]) runs them as its own steps before the
//! user's ready marker, then writes [`simpaths::PRIMED`]: the Xcode path and the entries of
//! `staged.json` ([`simpaths::MANIFEST`]) for Xcode and for each runtime, as they were when the
//! steps ran.
//!
//! # When a step is skipped
//! A clone's `sim` skips a root step when the record covers it, that is when `staged.json`
//! and the record both name the thing the step is about and hold the same entry for it:
//! - `xcode-select`, `license`, `first-launch`: the Xcode entry (its path, tree sizes and
//!   file counts) is the same and so is the Xcode's `version.plist` (version and build), which
//!   the record keeps as hex;
//! - `runtime-add-<name>` and `runtime-import-<name>`: the runtime `<name>` has the same
//!   entry (kind, size and SHA-256 of an image, or of every file of a bundle).
//!
//! No record, an unreadable or unknown one, no `staged.json`, or an entry that differs (a
//! runtime added or restaged after the prime, another Xcode) means the step runs as it
//! always did. The decision is per step: a base primed with one runtime and restaged with
//! two skips the first runtime's import and runs the second's.
//!
//! The record is trusted as far as the guest's own disk is: only root writes it.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::sim::{Argv, BUNDLE_SUFFIX, Inputs, Status, Step, StepFrame, Who, name_ok, plan};
use crate::simpaths;

/// The `version` the record carries; a record of another version covers nothing.
pub const RECORD_VERSION: u64 = 1;
/// The note of a step frame whose command was not run because the prime did it.
pub const SKIPPED_NOTE: &str = "skipped: done at prime";
/// The most bytes read of `staged.json` or the record.
pub const MAX_READ_BYTES: usize = 4 << 20;

/// The most bytes read of the Xcode's `version.plist`.
pub const MAX_IDENTITY_BYTES: usize = 16 << 10;

const IMPORT_PREFIX: &str = "runtime-import-";
const ADD_PREFIX: &str = "runtime-add-";

/// What `staged.json` says of the things the root steps are about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    /// The entry of the staged Xcode, if the setup staged one.
    pub xcode: Option<Value>,
    /// The entry of each staged runtime, by name (names the probe would not use are left
    /// out, as the probe leaves them out).
    pub runtimes: BTreeMap<String, Value>,
}

/// Reads `staged.json`.
///
/// # Errors
/// It is not JSON, or has no `runtimes` list.
pub fn parse_staged(text: &str) -> Result<Staged, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let list = v
        .get("runtimes")
        .and_then(Value::as_array)
        .ok_or("no runtimes list")?;
    let runtimes = list
        .iter()
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?;
            (name_ok(name, ".dmg") || name_ok(name, BUNDLE_SUFFIX))
                .then(|| (name.to_owned(), r.clone()))
        })
        .collect();
    Ok(Staged {
        xcode: v.get("xcode").filter(|x| x.is_object()).cloned(),
        runtimes,
    })
}

/// The argument list of a step whose argument list is fixed; a late one (built from earlier
/// output, which no root step has) gives none.
#[must_use]
pub fn fixed_argv(argv: Argv) -> Vec<String> {
    match argv {
        Argv::Fixed(v) => v,
        Argv::Late(_) => Vec::new(),
    }
}

/// The root steps of the probe for `staged`, exactly as [`plan`] builds them (the same
/// ids and commands), each with the steps it needs.
#[must_use]
pub fn root_steps(staged: &Staged) -> Vec<Step> {
    let inputs = Inputs {
        runtimes: staged.runtimes.keys().cloned().collect(),
        ..Inputs::default()
    };
    plan(&inputs)
        .into_iter()
        .filter(|s| s.who == Who::Root)
        .collect()
}

/// The record of a priming boot that ran [`root_steps`] of `staged` to the end: `None` when
/// the setup staged no Xcode (there was nothing to do).
#[must_use]
pub fn record_json(staged: &Staged, identity: Option<&str>) -> Option<Value> {
    let xcode = staged.xcode.clone()?;
    let runtimes: Vec<Value> = staged
        .runtimes
        .iter()
        .map(|(name, entry)| {
            let action = if name.ends_with(BUNDLE_SUFFIX) {
                "import"
            } else {
                "add"
            };
            json!({"name": name, "action": action, "staged": entry})
        })
        .collect();
    Some(json!({
        "version": RECORD_VERSION,
        "xcode_path": simpaths::XCODE_APP,
        "xcode": xcode,
        "xcode_identity": identity,
        "runtimes": runtimes,
    }))
}

/// What the record says was done.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    xcode: Value,
    identity: Option<String>,
    runtimes: BTreeMap<String, Value>,
}

fn parse_record(text: &str) -> Result<Record, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    if v.get("version").and_then(Value::as_u64) != Some(RECORD_VERSION) {
        return Err("another version".to_owned());
    }
    if v.get("xcode_path").and_then(Value::as_str) != Some(simpaths::XCODE_APP) {
        return Err("another Xcode path".to_owned());
    }
    let xcode = v.get("xcode").cloned().ok_or("no xcode entry")?;
    let identity = v
        .get("xcode_identity")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let runtimes = v
        .get("runtimes")
        .and_then(Value::as_array)
        .ok_or("no runtimes list")?
        .iter()
        .filter_map(|r| {
            Some((
                r.get("name")?.as_str()?.to_owned(),
                r.get("staged")?.clone(),
            ))
        })
        .collect();
    Ok(Record {
        xcode,
        identity,
        runtimes,
    })
}

/// What says which Xcode is on the disk: the bytes of its `version.plist` (version and
/// build), in hex. `None` when it cannot be read; the Xcode steps are then never skipped.
pub fn xcode_identity(read: &dyn Reader) -> Option<String> {
    read(simpaths::XCODE_VERSION_PLIST, MAX_IDENTITY_BYTES)
        .ok()
        .filter(|b| !b.is_empty())
        .map(hex::encode)
}

/// Reads a file of the guest: its path and the most bytes to read.
pub trait Reader: Fn(&str, usize) -> Result<Vec<u8>, String> {}
impl<F: Fn(&str, usize) -> Result<Vec<u8>, String>> Reader for F {}

/// `staged.json` and the record, as the guest found them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Primed {
    staged: Option<Staged>,
    record: Option<Record>,
    /// The identity of the Xcode on the disk now ([`xcode_identity`]).
    identity: Option<String>,
}

impl Primed {
    /// Reads both files through `read` (path and byte cap); one that is missing or does not
    /// parse is as if it were not there, and then nothing is covered.
    pub fn load(read: &dyn Reader) -> Self {
        let text = |path: &str| {
            read(path, MAX_READ_BYTES)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        };
        Self {
            staged: text(simpaths::MANIFEST).and_then(|t| parse_staged(&t).ok()),
            record: text(simpaths::PRIMED).and_then(|t| parse_record(&t).ok()),
            identity: xcode_identity(read),
        }
    }

    /// Whether the record covers root step `id` (see the module documentation).
    #[must_use]
    pub fn covers(&self, id: &str) -> bool {
        let (Some(staged), Some(record)) = (&self.staged, &self.record) else {
            return false;
        };
        match id {
            "xcode-select" | "license" | "first-launch" => {
                self.identity.is_some()
                    && self.identity == record.identity
                    && staged.xcode.as_ref() == Some(&record.xcode)
            }
            _ => id
                .strip_prefix(IMPORT_PREFIX)
                .or_else(|| id.strip_prefix(ADD_PREFIX))
                .is_some_and(|name| {
                    staged.runtimes.contains_key(name)
                        && staged.runtimes.get(name) == record.runtimes.get(name)
                }),
        }
    }
}

/// Fills `f` as the frame of `step`, which was not run because the prime did it: `ok`, with
/// the command it would have run and the reason.
pub fn mark_done(f: &mut StepFrame, step: &Step) {
    f.argv = fixed_argv(step.argv.clone());
    f.status = Status::Ok;
    f.note = Some(SKIPPED_NOTE.to_owned());
}
