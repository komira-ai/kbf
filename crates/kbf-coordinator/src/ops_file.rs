//! The ops store on disk: two files, so that the release before the halt release still reads
//! the ops file the halt release writes.
//!
//! The ops file (`--ops-file`) keeps the shape the release before wrote and reads, whose
//! reader refuses a key it does not know: a halt is its host, check and target, an
//! acknowledgement its host and target. What the halt release adds to them lives beside it in
//! the halts file (`<ops-file>.halts`): the whole report a halt and each acknowledgement were
//! made from, the reports received while a halt stood (`halt_seen`) and the clear generation of
//! each service (`halt_generation`). The reader of the halts file skips a key it does not
//! know, and refuses only a file it cannot parse.
//!
//! On reading, the halts file's records are joined to the ops file's by host and target. A
//! record that matches nothing (the release before rewrote the ops file and cleared or replaced
//! what it was about) is dropped: a halt with no report, an acknowledgement with none, are what
//! a file from before reports reads as ([`crate::halt`]), and a cleared report with no
//! acknowledgement only halts the service again. Reports kept while a halt stood join only to
//! that halt. A generation is kept whatever the ops file says.
//!
//! A change writes the ops file first, then the halts file, each replaced whole. When the
//! halts file cannot be written, the ops file is put back as it was and the change fails, so
//! a failed change leaves both files as they were. A crash between the two writes leaves the
//! halts file behind the ops file, which loses reports and generations, never a halt for long:
//! a clear whose halts file was not written leaves acknowledgements that match nothing, so the
//! halted host's next report halts the service again, and the clear's generation is not
//! counted (no agent lifts on it). A
//! farm that never halted has no halts file: one is written once there is something to keep in
//! it, and after that on every change.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kbf_updater::version::Version;
use kbf_updater::wire::UpdateResult;
use serde::{Deserialize, Serialize};

use crate::desired::{StoreError, io_at, write_whole};
use crate::ops::{HaltAck, OpsError, OpsState};

/// The halts file of the ops file at `ops`: its name with `.halts` added.
#[must_use]
pub fn halts_path(ops: &Path) -> PathBuf {
    let mut path = ops.as_os_str().to_owned();
    path.push(".halts");
    PathBuf::from(path)
}

/// A report, with the host that made it and the version it was about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Kept {
    node_id: String,
    target: Version,
    report: UpdateResult,
}

impl Kept {
    fn of(node_id: &str, target: &Version, report: UpdateResult) -> Self {
        Self {
            node_id: node_id.to_owned(),
            target: target.clone(),
            report,
        }
    }

    fn is_about(&self, node_id: &str, target: &Version) -> bool {
        self.node_id == node_id && self.target == *target
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What the halts file keeps of one service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ServiceHalts {
    /// The report of the standing halt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    halt: Option<Kept>,
    /// The reports of the acknowledgements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    halt_acked: Vec<Kept>,
    /// The reports received while the standing halt stood.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    halt_seen: Vec<Kept>,
    /// How many halts an operator has cleared.
    #[serde(default, skip_serializing_if = "is_zero")]
    halt_generation: u64,
}

/// Everything the halts file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HaltsFile {
    /// Per service name; a service with nothing to keep is not listed.
    #[serde(default)]
    services: BTreeMap<String, ServiceHalts>,
}

/// `state` as the two files hold it: the ops file's part, in the shape of the release before,
/// and the halts file's.
fn split(state: &OpsState) -> (OpsState, HaltsFile) {
    let mut ops = state.clone();
    let mut halts = HaltsFile::default();
    for (name, marks) in &mut ops.services {
        let mut kept = ServiceHalts::default();
        if let Some(halt) = &mut marks.halt {
            kept.halt = halt
                .report
                .take()
                .map(|report| Kept::of(&halt.node_id, &halt.target, report));
        }
        for ack in &mut marks.halt_acked {
            if let Some(report) = ack.report.take() {
                kept.halt_acked
                    .push(Kept::of(&ack.node_id, &ack.target, report));
            }
        }
        kept.halt_seen = std::mem::take(&mut marks.halt_seen)
            .into_iter()
            .filter_map(|ack| {
                let report = ack.report?;
                Some(Kept::of(&ack.node_id, &ack.target, report))
            })
            .collect();
        kept.halt_generation = std::mem::take(&mut marks.halt_generation);
        if kept != ServiceHalts::default() {
            halts.services.insert(name.clone(), kept);
        }
    }
    (ops, halts)
}

/// `ops`, read from the ops file, with what `halts` keeps for it joined to it.
fn join(mut ops: OpsState, halts: HaltsFile) -> OpsState {
    for (name, kept) in halts.services {
        if kept.halt_generation == 0 && !ops.services.contains_key(&name) {
            continue;
        }
        let marks = ops.services.entry(name).or_default();
        let mut standing = false;
        if let (Some(halt), Some(report)) = (&mut marks.halt, kept.halt)
            && report.is_about(&halt.node_id, &halt.target)
        {
            halt.report = Some(report.report);
            standing = true;
        }
        for ack in &mut marks.halt_acked {
            if ack.report.is_none() {
                ack.report = kept
                    .halt_acked
                    .iter()
                    .find(|k| k.is_about(&ack.node_id, &ack.target))
                    .map(|k| k.report.clone());
            }
        }
        if standing {
            marks.halt_seen = kept
                .halt_seen
                .into_iter()
                .map(|k| HaltAck {
                    node_id: k.node_id,
                    target: k.target,
                    report: Some(k.report),
                })
                .collect();
        }
        marks.halt_generation = marks.halt_generation.max(kept.halt_generation);
    }
    ops
}

/// The halts file at `path`: nothing if there is none.
fn read_halts(path: &Path) -> Result<HaltsFile, OpsError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| OpsError::Invalid {
            path: path.to_owned(),
            why: e.to_string(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HaltsFile::default()),
        Err(source) => Err(StoreError::Io {
            path: path.to_owned(),
            source,
        }
        .into()),
    }
}

/// `ops`, read from the ops file at `path`, joined with its halts file.
///
/// # Errors
/// [`OpsError`]: the halts file exists and cannot be read or parsed.
pub fn joined(path: &Path, ops: OpsState) -> Result<OpsState, OpsError> {
    Ok(join(ops, read_halts(&halts_path(path))?))
}

fn pretty<T: Serialize>(path: &Path, value: &T) -> Result<Vec<u8>, OpsError> {
    let mut text = serde_json::to_vec_pretty(value)
        .map_err(io::Error::from)
        .map_err(io_at(path))?;
    text.push(b'\n');
    Ok(text)
}

/// Writes `state` to the ops file at `path` and its halts file, in that order. When the halts
/// file cannot be written, the ops file is put back as it was (removed, if there was none) and
/// the error is answered: a change that fails leaves both files as they were, so a clear that
/// cannot be written leaves the halt standing on disk too.
///
/// # Errors
/// [`OpsError::Io`]: a file could not be read or replaced.
pub fn write(path: &Path, state: &OpsState) -> Result<(), OpsError> {
    let (ops, halts) = split(state);
    let halts_path = halts_path(path);
    let halts_text = pretty(&halts_path, &halts)?;
    let keep = !halts.services.is_empty() || halts_path.exists();
    let before = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_at(path)(e).into()),
    };
    write_whole(path, &pretty(path, &ops)?)?;
    if !keep {
        return Ok(());
    }
    let Err(why) = write_whole(&halts_path, &halts_text) else {
        return Ok(());
    };
    let restored = match &before {
        Some(bytes) => write_whole(path, bytes).map_err(|e| e.to_string()),
        None => fs::remove_file(path).map_err(|e| e.to_string()),
    };
    if let Err(also) = restored {
        tracing::error!(
            path = %path.display(),
            %why,
            %also,
            "the halts file could not be written and the ops file could not be put back"
        );
    }
    Err(why.into())
}
