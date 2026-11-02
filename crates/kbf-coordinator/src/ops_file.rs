//! The ops store on disk: two files, so that the release before the halt release still reads
//! the ops file the halt release writes.
//!
//! The ops file (`--ops-file`) keeps the shape the release before wrote and reads, whose
//! reader refuses a key it does not know: a halt is its host, check and target, an
//! acknowledgement its host and target. What the halt release adds to them lives beside it in
//! the halts file (`<ops-file>.halts`): the whole report a halt and each acknowledgement were
//! made from, the reports received while a halt stood (`halt_seen`) and the clear generation of
//! each service (`halt_generation`).
//!
//! The reader of the halts file skips a key it does not know. A record whose report it cannot
//! read (an outcome a later release added, say) is passed over as a whole and counted, and the
//! rest of the file is read; only a file it cannot parse at all is refused. A passed-over
//! record loses its report and nothing else (the reports kept while a halt stood still join to
//! it by its host and version), and that loss only halts the service again: a halt with no
//! report, an acknowledgement with none, are what a file from before reports reads as
//! ([`crate::halt`]), and a report seen while halted that is not kept halts the service again
//! after the clear.
//!
//! On reading, the halts file's records are joined to the ops file's by host and target. A
//! record that matches nothing (the release before rewrote the ops file and cleared or replaced
//! what it was about) is dropped, with the same loss. Reports kept while a halt stood join only
//! to that halt. A generation is kept whatever the ops file says. Records join by host and
//! version alone: when the release before cleared a halt and the same host halted on the same
//! version again, the old halt's reports join to the new one, about the same host and version.
//!
//! A change is committed by replacing the ops file. First the halts file it goes with is
//! written whole to `<ops-file>.halts.next`, naming the SHA-256 of the ops file text it goes
//! with; then the ops file is replaced; then the staged file is renamed over the halts file.
//! A change that fails before the ops file holds it leaves both files as they were (its staged
//! file is removed, or found stale later). Once the ops file holds the change, the change
//! stands: replacing the ops file can answer an error after its rename (the directory's fsync),
//! so on an error the ops file is read back, and when it holds the change's text the change
//! goes on. The change is answered as made only once the halts file is renamed in and its
//! directory synced: until then what it adds (a clear's generation above all) would not
//! survive the release before rewriting the ops file, so a failure to rename it fails the
//! change, which does not reach memory (no agent is told its generation), and the staged file
//! is left for the next start or change to rename in.
//!
//! A start, and each change before it writes, settles a staged file left behind: one that names
//! the digest of the ops file there is renamed over the halts file (the change it staged was
//! committed), any other is removed (it was not, or the release before has rewritten the ops
//! file since). A staged file that cannot be settled refuses the start and fails the change.
//! So the two files are read as they were before a change or as they were after it, never one
//! of each: a clear's generation and acknowledgements are on disk with the clear or not at all.
//! The one exception needs a crash after the commit and then the release before writing the
//! ops file before this one starts again: the staged file no longer names the ops file and is
//! removed, so a clear in the ops file loses its acknowledgements' reports and its generation.
//! No agent was told that generation (a change reaches memory only once its halts file is
//! renamed in), and the loss only halts the service again: the next clear counts on from the
//! generation agents were told.
//!
//! A farm that never halted has no halts file: one is written once there is something to keep
//! in it, and after that on every change.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use kbf_updater::version::Version;
use kbf_updater::wire::UpdateResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::desired::{StoreError, dir_of, io_at, write_whole};
use crate::ops::{HaltAck, OpsError, OpsState};

/// The halts file of the ops file at `ops`: its name with `.halts` added.
#[must_use]
pub(crate) fn halts_path(ops: &Path) -> PathBuf {
    let mut path = ops.as_os_str().to_owned();
    path.push(".halts");
    PathBuf::from(path)
}

/// Where a change stages the halts file of the ops file at `ops` before it commits.
#[must_use]
pub(crate) fn staged_path(ops: &Path) -> PathBuf {
    let mut path = halts_path(ops).into_os_string();
    path.push(".next");
    PathBuf::from(path)
}

/// The SHA-256 of `text`, in lowercase hex.
fn digest(text: &[u8]) -> String {
    Sha256::digest(text)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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

/// The standing halt: its host and version, which the reports kept while it stood join by,
/// and its report when there is one this release can read. The host and version are kept
/// even when there is no report, so that those reports still join after any later write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Standing {
    node_id: String,
    target: Version,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<UpdateResult>,
}

/// What the halts file keeps of one service.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct ServiceHalts {
    /// The standing halt.
    #[serde(skip_serializing_if = "Option::is_none")]
    halt: Option<Standing>,
    /// The reports of the acknowledgements.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    halt_acked: Vec<Kept>,
    /// The reports received while the standing halt stood.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    halt_seen: Vec<Kept>,
    /// How many halts an operator has cleared.
    #[serde(skip_serializing_if = "is_zero")]
    halt_generation: u64,
}

/// Everything the halts file holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct HaltsFile {
    /// Per service name; a service with nothing to keep is not listed.
    services: BTreeMap<String, ServiceHalts>,
    /// The SHA-256 of the ops file text this file was written with.
    #[serde(skip_serializing_if = "Option::is_none")]
    ops: Option<String>,
}

/// [`ServiceHalts`] as read: each record is read on its own, so that one this release cannot
/// read is passed over and not the file.
#[derive(Deserialize)]
struct ReadService {
    #[serde(default)]
    halt: Option<Value>,
    #[serde(default)]
    halt_acked: Vec<Value>,
    #[serde(default)]
    halt_seen: Vec<Value>,
    #[serde(default)]
    halt_generation: u64,
}

/// [`HaltsFile`] as read.
#[derive(Deserialize)]
struct ReadHalts {
    #[serde(default)]
    services: BTreeMap<String, ReadService>,
    #[serde(default)]
    ops: Option<String>,
}

/// The record `value` holds; none, counted in `skipped`, when this release cannot read it.
fn kept(value: Value, skipped: &mut usize) -> Option<Kept> {
    let read = serde_json::from_value(value).ok();
    *skipped += usize::from(read.is_none());
    read
}

/// The standing halt `value` holds: none, counted in `skipped`, when this release cannot read
/// its host or version; with no report, counted, when it cannot read the report.
fn standing(mut value: Value, skipped: &mut usize) -> Option<Standing> {
    let report = value.get_mut("report").map(Value::take);
    let Ok(mut halt) = serde_json::from_value::<Standing>(value) else {
        *skipped += 1;
        return None;
    };
    halt.report = report.and_then(|r| {
        let read = serde_json::from_value(r).ok();
        *skipped += usize::from(read.is_none());
        read
    });
    Some(halt)
}

impl ReadHalts {
    /// What this release reads of the file, and how many records it passed over.
    fn known(self) -> (HaltsFile, usize) {
        let mut skipped = 0;
        let mut services = BTreeMap::new();
        for (name, read) in self.services {
            let halt = read.halt.and_then(|v| standing(v, &mut skipped));
            let mut each = |values: Vec<Value>| -> Vec<Kept> {
                values
                    .into_iter()
                    .filter_map(|v| kept(v, &mut skipped))
                    .collect()
            };
            let service = ServiceHalts {
                halt,
                halt_acked: each(read.halt_acked),
                halt_seen: each(read.halt_seen),
                halt_generation: read.halt_generation,
            };
            services.insert(name, service);
        }
        let file = HaltsFile {
            services,
            ops: self.ops,
        };
        (file, skipped)
    }
}

/// `state` as the two files hold it: the ops file's part, in the shape of the release before,
/// and the halts file's.
fn split(state: &OpsState) -> (OpsState, HaltsFile) {
    let mut ops = state.clone();
    let mut halts = HaltsFile::default();
    for (name, marks) in &mut ops.services {
        let mut kept = ServiceHalts::default();
        if let Some(halt) = &mut marks.halt {
            kept.halt = Some(Standing {
                node_id: halt.node_id.clone(),
                target: halt.target.clone(),
                report: halt.report.take(),
            });
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
        if let (Some(halt), Some(kept)) = (&mut marks.halt, kept.halt)
            && kept.node_id == halt.node_id
            && kept.target == halt.target
        {
            halt.report = kept.report;
            standing = true;
        }
        for ack in &mut marks.halt_acked {
            if let Some(k) = kept
                .halt_acked
                .iter()
                .find(|k| k.is_about(&ack.node_id, &ack.target))
            {
                ack.report = Some(k.report.clone());
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

/// The file at `path`: none if there is none.
fn read_if_any(path: &Path) -> Result<Option<Vec<u8>>, OpsError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_owned(),
            source,
        }
        .into()),
    }
}

/// The halts file at `path` (nothing if there is none), and how many records of it this
/// release passed over.
fn read_halts(path: &Path) -> Result<(HaltsFile, usize), OpsError> {
    let Some(bytes) = read_if_any(path)? else {
        return Ok((HaltsFile::default(), 0));
    };
    let read: ReadHalts = serde_json::from_slice(&bytes).map_err(|e| OpsError::Invalid {
        path: path.to_owned(),
        why: e.to_string(),
    })?;
    Ok(read.known())
}

/// Renames the staged halts file `staged` over the halts file `halts`, durably.
fn publish(staged: &Path, halts: &Path) -> Result<(), StoreError> {
    fs::rename(staged, halts).map_err(io_at(halts))?;
    let dir = dir_of(halts);
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_at(dir))
}

/// The digest a staged halts file names; none if it names none or cannot be parsed.
fn names(staged: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Staged {
        ops: Option<String>,
    }
    serde_json::from_slice::<Staged>(staged).ok()?.ops
}

/// Settles the staged halts file of the ops file at `path`, if a change left one: renamed over
/// the halts file when it names the digest of the ops file there (the change was committed),
/// removed otherwise.
fn settle(path: &Path) -> Result<(), OpsError> {
    let staged = staged_path(path);
    let Some(bytes) = read_if_any(&staged)? else {
        return Ok(());
    };
    let ops = read_if_any(path)?.map(|text| digest(&text));
    if ops.is_some() && names(&bytes) == ops {
        tracing::warn!(
            path = %staged.display(),
            "a change was committed to the ops file before its halts file was renamed in: renaming it now"
        );
        publish(&staged, &halts_path(path))?;
    } else {
        tracing::warn!(
            path = %staged.display(),
            "removing the halts file of a change that was not committed"
        );
        fs::remove_file(&staged).map_err(io_at(&staged))?;
    }
    Ok(())
}

/// `ops`, read from the ops file at `path`, joined with its halts file once a staged halts file
/// left behind is settled; and how many records of the halts file this release passed over.
///
/// # Errors
/// [`OpsError`]: a staged halts file cannot be settled, or the halts file exists and cannot be
/// read or parsed.
pub(crate) fn joined(path: &Path, ops: OpsState) -> Result<(OpsState, usize), OpsError> {
    settle(path)?;
    let halts = halts_path(path);
    let (file, skipped) = read_halts(&halts)?;
    if skipped > 0 {
        tracing::warn!(
            path = %halts.display(),
            skipped,
            "records of the halts file this release cannot read were passed over"
        );
    }
    Ok((join(ops, file), skipped))
}

fn pretty<T: Serialize>(path: &Path, value: &T) -> Result<Vec<u8>, OpsError> {
    let mut text = serde_json::to_vec_pretty(value)
        .map_err(io::Error::from)
        .map_err(io_at(path))?;
    text.push(b'\n');
    Ok(text)
}

/// A point of [`write_with`] where a test makes the file system fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Just after the ops file was replaced: an error replacing it answers after its rename.
    OpsReplaced,
    /// Reading the ops file back after an error replacing it.
    ReadBack,
    /// Removing the staged halts file of a change that failed.
    Discard,
    /// Renaming the staged halts file over the halts file.
    Publish,
}

/// Writes `state` to the ops file at `path` and its halts file, as one commit (the module
/// docs say how).
///
/// # Errors
/// [`OpsError`]: a file could not be read or replaced, or the halts file could not be renamed
/// in after the ops file held the change.
pub(crate) fn write(path: &Path, state: &OpsState) -> Result<(), OpsError> {
    write_with(path, state, &|_| Ok(()))
}

/// [`write`], with `fail` asked at each [`Step`] whether the file system fails there.
pub(crate) fn write_with(
    path: &Path,
    state: &OpsState,
    fail: &dyn Fn(Step) -> io::Result<()>,
) -> Result<(), OpsError> {
    settle(path)?;
    let (ops, mut halts) = split(state);
    let text = pretty(path, &ops)?;
    let halts_path = halts_path(path);
    if halts.services.is_empty() && !halts_path.exists() {
        return Ok(write_whole(path, &text)?);
    }
    halts.ops = Some(digest(&text));
    let staged = staged_path(path);
    write_whole(&staged, &pretty(&staged, &halts)?)?;
    let replaced =
        write_whole(path, &text).and_then(|()| fail(Step::OpsReplaced).map_err(io_at(path)));
    if let Err(why) = replaced {
        match fail(Step::ReadBack).and_then(|()| fs::read(path)) {
            Ok(now) if now == text => tracing::warn!(
                path = %path.display(),
                %why,
                "replacing the ops file answered an error, but it holds the change: the change stands"
            ),
            Err(unknown) if unknown.kind() != io::ErrorKind::NotFound => {
                // Whether the ops file holds the change is not known: the staged file is
                // left for the next start or change to settle by what the ops file holds.
                tracing::error!(
                    path = %path.display(),
                    %why,
                    %unknown,
                    "the ops file could not be replaced or read back"
                );
                return Err(why.into());
            }
            _ => {
                if let Err(left) = fail(Step::Discard).and_then(|()| fs::remove_file(&staged)) {
                    tracing::warn!(
                        path = %staged.display(),
                        %left,
                        "the halts file of a failed change is left to be removed later"
                    );
                }
                return Err(why.into());
            }
        }
    }
    let published = fail(Step::Publish)
        .map_err(io_at(&halts_path))
        .and_then(|()| publish(&staged, &halts_path));
    if let Err(why) = published {
        tracing::error!(
            path = %staged.display(),
            %why,
            "the ops file holds the change but its halts file could not be renamed in: the change fails, and the next start or change renames it in"
        );
        return Err(why.into());
    }
    Ok(())
}
