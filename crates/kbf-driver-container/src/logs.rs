//! An action's stdout and stderr stored with its outputs.
//!
//! Into a store with no intake (the transfer engine, the packed store of a daemon) the
//! two logs are hashed ([`hash_file`], the pass the output files get) and sent as
//! [`Record`]s in the same [`Cas::put_records`] call as the outputs, in the same
//! [`Cas::missing`] question: a daemon that packs what one call holds writes the outputs
//! and both logs as one pack, and a log the store already holds is not sent. Into a
//! store with an intake each log is stored through it, as [`collect_log`] does.

use std::path::Path;
use std::sync::Arc;

use kbf_daemon::cas::{Cas, FileSource, Record, Source, label, proto_digest};
use kbf_proto::reapi::{ActionResult, Digest};
use rustix::fs::{CWD, Mode, OFlags};

use crate::hashing::hash_file;
use crate::outputs::{blocking, collect_log, limit};
use crate::tree::{Exceeded, TreeError};

/// The name of the file the driver captures stdout in, in the lease directory.
pub(crate) const STDOUT: &str = "stdout";
/// The name of the file the driver captures stderr in, in the lease directory.
pub(crate) const STDERR: &str = "stderr";

/// Where an action's logs are and how large each may be.
#[derive(Clone, Copy)]
pub(crate) struct Logs<'a> {
    /// The lease directory, holding [`STDOUT`] and [`STDERR`].
    pub dir: &'a Path,
    /// The most bytes each log may hold.
    pub max: u64,
}

/// The log `name` of `logs` hashed, as a record the store reads again, by name, when it
/// sends it. At most `max + 1` bytes are read; a log past the limit is
/// [`TreeError::Limit`], and one that is no regular file is an error, never read.
async fn hash_log(logs: &Logs<'_>, name: &'static str) -> Result<Record, TreeError> {
    let (dir, max) = (logs.dir.to_owned(), logs.max);
    let shown = logs.dir.join(name);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let hashed = blocking(&shown, move || {
        let dir = Arc::new(rustix::fs::openat(CWD, &dir, flags, Mode::empty())?);
        let hashed = hash_file(&dir, name, max, &mut Vec::new(), None)?;
        Ok(hashed.map(|d| Record {
            digest: d.digest,
            crc32c: d.crc32c,
            source: Source::File(FileSource::new(
                Arc::clone(&dir),
                Vec::new().into(),
                name.to_owned(),
                d.stamp,
            )),
        }))
    })
    .await?;
    hashed.ok_or_else(|| limit(&shown, Exceeded::Stdio, max))
}

/// Before the outputs are sent: into a store with no intake (`plain`), [`add_to`] the
/// outputs' `records`; otherwise nothing. The choice is made here and in [`after_outputs`],
/// not in the caller, so that every line of the caller runs whatever the store.
pub(crate) async fn with_outputs(
    records: &mut Vec<Record>,
    logs: Option<&Logs<'_>>,
    plain: bool,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    let Some(logs) = logs.filter(|_| plain) else {
        return Ok(());
    };
    add_to(records, logs, result).await
}

/// After the outputs are sent: into a store with an intake (not `plain`), each log
/// [`through_intake`]; otherwise nothing, since [`with_outputs`] sent them.
pub(crate) async fn after_outputs(
    cas: &impl Cas,
    logs: Option<&Logs<'_>>,
    plain: bool,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    let Some(logs) = logs.filter(|_| !plain) else {
        return Ok(());
    };
    through_intake(cas, logs, result).await
}

/// Hashes both logs of `logs`, sets `result`'s stdout and stderr digests, and adds their
/// records to `records` (a blob `records` holds already, or the other log, once only).
/// The first log that cannot be hashed is the error.
pub(crate) async fn add_to(
    records: &mut Vec<Record>,
    logs: &Logs<'_>,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    let mut held: std::collections::HashSet<_> = records.iter().map(|r| r.digest).collect();
    for (name, slot) in [
        (STDOUT, &mut result.stdout_digest),
        (STDERR, &mut result.stderr_digest),
    ] {
        let record = hash_log(logs, name).await?;
        *slot = Some(proto_digest(&record.digest));
        if held.insert(record.digest) {
            records.push(record);
        }
    }
    Ok(())
}

/// Stores both logs through the store's intake, as [`collect_log`] does, and sets
/// `result`'s digests.
pub(crate) async fn through_intake(
    cas: &impl Cas,
    logs: &Logs<'_>,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    for (name, slot) in [
        (STDOUT, &mut result.stdout_digest),
        (STDERR, &mut result.stderr_digest),
    ] {
        *slot = Some(collect_log(cas, &logs.dir.join(name), logs.max).await?);
    }
    Ok(())
}

/// The records among `records` the store does not hold: asked of the store once, for all.
pub(crate) async fn missing_only(cas: &impl Cas, records: Vec<Record>) -> Vec<Record> {
    let digests: Vec<Digest> = records.iter().map(|r| proto_digest(&r.digest)).collect();
    let missing: std::collections::HashSet<String> =
        cas.missing(&digests).await.iter().map(label).collect();
    records
        .into_iter()
        .zip(&digests)
        .filter(|(_, digest)| missing.contains(&label(digest)))
        .map(|(record, _)| record)
        .collect()
}

/// What a stopped action left in its logs: each log's digest when it was stored, and why
/// each that was not (past the limit, unreadable, the store failed) was left out. Into a
/// store with no intake both logs go in one [`Cas::put_records`].
pub(crate) async fn store_both(
    cas: &impl Cas,
    logs: &Logs<'_>,
) -> ([Option<Digest>; 2], Vec<TreeError>) {
    let mut found = [None, None];
    let mut problems = Vec::new();
    if cas.intake().is_some() {
        for (name, slot) in [STDOUT, STDERR].into_iter().zip(&mut found) {
            match collect_log(cas, &logs.dir.join(name), logs.max).await {
                Ok(digest) => *slot = Some(digest),
                Err(e) => problems.push(e),
            }
        }
        return (found, problems);
    }
    let mut records = Vec::new();
    for (name, slot) in [STDOUT, STDERR].into_iter().zip(&mut found) {
        match hash_log(logs, name).await {
            Ok(record) => {
                *slot = Some(proto_digest(&record.digest));
                if !records.iter().any(|r: &Record| r.digest == record.digest) {
                    records.push(record);
                }
            }
            Err(e) => problems.push(e),
        }
    }
    if let Err(e) = cas.put_records(missing_only(cas, records).await).await {
        problems.push(e.into());
        found = [None, None];
    }
    (found, problems)
}
