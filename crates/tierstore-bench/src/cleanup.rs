//! `cleanup`: delete the objects a run wrote to the cold store, and only those.
//!
//! The list of what to delete is the run's own request log (`requests.jsonl` in the result
//! directory), not a listing of the store: a key is deleted when the log has a `PUT` of it to the
//! configured cold bucket that was answered `200` (the store took it) or not answered at all (it
//! may have landed). A `PUT` answered `412` was an object somebody else wrote first, and a `PUT`
//! the store refused (any other status) wrote nothing; neither is deleted. Every key to be
//! deleted must be under `<prefix>/<run id>/`; if the log names one that is not, nothing is
//! deleted and the run stops.
//!
//! After the deletes the run prefix is listed once, and the number of objects left under it is
//! reported: an object there that the log does not list (a put that was in flight when a process
//! was killed, or another client's) is left alone and shows up in that count.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::cells::cold_put::run_prefix;
use crate::config::RunConfig;
use crate::error::BenchError;
use crate::requestlog::{RequestLog, RequestRecord};
use crate::role::Role;
use crate::s3::{BenchS3, Timeouts};

/// The name of the request log in a result directory.
pub const REQUEST_LOG_FILE: &str = "requests.jsonl";

/// What a cleanup did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupReport {
    /// Distinct keys the log lists as written (the store answered `200`).
    pub put_ok: u64,
    /// Distinct further keys the log lists as sent without an answer (deleted in case they
    /// landed).
    pub put_unanswered: u64,
    /// Deletes the store acknowledged (an absent key counts: a delete of it succeeds).
    pub deleted: u64,
    /// Deletes that got no acknowledgement.
    pub failed: u64,
    /// Objects listed under the run prefix after the deletes.
    pub remaining: u64,
}

impl CleanupReport {
    /// Distinct keys the log lists, answered or not.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.put_ok + self.put_unanswered
    }
}

/// How the log saw a key being written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Acknowledged,
    Unanswered,
}

/// The keys of `records` to delete, under `run_prefix`, with how each was written.
fn keys_to_delete(
    records: &[RequestRecord],
    bucket: &str,
    run_prefix: &str,
) -> Result<BTreeMap<String, Seen>, BenchError> {
    let mut keys: BTreeMap<String, Seen> = BTreeMap::new();
    for record in records {
        if record.method != "PUT" || record.role != Role::Cold || record.bucket != bucket {
            continue;
        }
        let seen = match record.status {
            Some(200) => Seen::Acknowledged,
            None => Seen::Unanswered,
            Some(_) => continue,
        };
        if !record.key.starts_with(run_prefix) {
            return Err(BenchError::Request(
                "the request log names a written key outside the run prefix",
            ));
        }
        let entry = keys.entry(record.key.clone()).or_insert(seen);
        if seen == Seen::Acknowledged {
            *entry = Seen::Acknowledged;
        }
    }
    Ok(keys)
}

/// The records of the log text. A last line that does not parse (a write cut short by a kill)
/// is dropped; any other that does not is an error.
fn parse_log(text: &str) -> Result<Vec<RequestRecord>, BenchError> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut records = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if i + 1 == lines.len() => {}
            Err(e) => {
                return Err(BenchError::Result(format!(
                    "request log line {} is not a record: {e}",
                    i + 1
                )));
            }
        }
    }
    Ok(records)
}

/// Opens the request log of `config.result_dir` for the cleanup's own records. Unlike
/// [`RequestLog::create`] it does not make a log that is not there: a cleanup with no log has
/// nothing it may delete.
///
/// # Errors
/// [`BenchError::Io`] if the log is missing or cannot be opened.
pub fn open_log(config: &RunConfig) -> Result<Arc<RequestLog>, BenchError> {
    let path = config.result_dir.join(REQUEST_LOG_FILE);
    if !path.is_file() {
        return Err(BenchError::Io {
            path,
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
    }
    Ok(Arc::new(RequestLog::create(&path)?))
}

/// Adds one finished delete task to the report.
fn tally(
    done: Result<Result<bool, BenchError>, tokio::task::JoinError>,
    report: &mut CleanupReport,
) -> Result<(), BenchError> {
    let acknowledged = done.map_err(|_| BenchError::Request("a delete task panicked"))??;
    if acknowledged {
        report.deleted += 1;
    } else {
        report.failed += 1;
    }
    Ok(())
}

/// Deletes what the request log in `config.result_dir` lists under `<prefix>/<run_id>/`, with
/// `in_flight` deletes at a time, records each delete in `log`, then counts what is left under
/// the run prefix.
///
/// # Errors
/// [`BenchError`]: no cold store is configured, the log is missing or has a line that is not a
/// record, the log lists a written key outside the run prefix (nothing is deleted then), the
/// request log cannot be written, or the listing after the deletes fails.
pub async fn run(
    config: &RunConfig,
    log: &Arc<RequestLog>,
    prefix: &str,
    run_id: &str,
    in_flight: u32,
) -> Result<CleanupReport, BenchError> {
    let under = run_prefix(prefix, run_id)?;
    let spec = config.store(Role::Cold)?;
    let path = config.result_dir.join(REQUEST_LOG_FILE);
    let text = std::fs::read_to_string(&path).map_err(|source| BenchError::Io {
        path: path.clone(),
        source,
    })?;
    let keys = keys_to_delete(&parse_log(&text)?, &spec.bucket, &under)?;
    let count = |seen: Seen| {
        u64::try_from(keys.values().filter(|s| **s == seen).count()).unwrap_or(u64::MAX)
    };
    let mut report = CleanupReport {
        put_ok: count(Seen::Acknowledged),
        put_unanswered: count(Seen::Unanswered),
        deleted: 0,
        failed: 0,
        remaining: 0,
    };

    let s3 = Arc::new(BenchS3::new(
        Role::Cold,
        spec,
        Timeouts::default(),
        log.clone(),
    )?);
    let width = usize::try_from(in_flight.max(1)).unwrap_or(1);
    let mut tasks: JoinSet<Result<bool, BenchError>> = JoinSet::new();
    for key in keys.into_keys() {
        while tasks.len() >= width {
            if let Some(done) = tasks.join_next().await {
                tally(done, &mut report)?;
            }
        }
        let s3 = s3.clone();
        tasks.spawn(async move { Ok(s3.delete(&key).await?.is_ok()) });
    }
    while let Some(done) = tasks.join_next().await {
        tally(done, &mut report)?;
    }

    let mut token = None;
    loop {
        let page = s3
            .list_page(&under, token.as_ref(), 1000)
            .await?
            .result
            .map_err(|_| BenchError::Request("cannot list the run prefix after the deletes"))?;
        report.remaining += u64::try_from(page.objects.len()).unwrap_or(u64::MAX);
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(method: &str, key: &str, status: Option<u16>) -> RequestRecord {
        RequestRecord {
            unix_ms: 1,
            role: Role::Cold,
            method: method.to_owned(),
            bucket: "b".to_owned(),
            key: key.to_owned(),
            status,
            latency_us: 1,
            bytes: 1,
            error_kind: None,
        }
    }

    /// Catches a key counted twice, or an acknowledged write downgraded by a later unanswered
    /// retry of the same key.
    #[test]
    fn a_key_is_listed_once_and_acknowledged_wins() {
        let records = [
            record("PUT", "p/r/a", None),
            record("PUT", "p/r/a", Some(200)),
            record("PUT", "p/r/a", None),
            record("PUT", "p/r/b", None),
        ];
        let keys = keys_to_delete(&records, "b", "p/r/").unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys["p/r/a"] == Seen::Acknowledged);
        assert!(keys["p/r/b"] == Seen::Unanswered);
    }

    /// Catches a torn last line stopping a cleanup, and a torn line in the middle being skipped
    /// (which would hide a write).
    #[test]
    fn only_a_torn_last_line_of_the_log_is_forgiven() {
        let good = serde_json::to_string(&record("PUT", "p/r/a", Some(200))).unwrap();
        let torn = &good[..good.len() / 2];
        assert_eq!(parse_log(&format!("{good}\n{torn}")).unwrap().len(), 1);
        assert!(parse_log(&format!("{torn}\n{good}\n")).is_err());
        assert_eq!(parse_log("").unwrap().len(), 0);
    }
}
