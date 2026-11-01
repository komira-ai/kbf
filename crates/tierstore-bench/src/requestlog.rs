//! The harness-side request log: every request the tool sends to a store, one JSON object per
//! line, in the result directory.
//!
//! This is the proof, with the bucket policy of the scoped accounts, that the tool reached only
//! the test buckets: nothing is audited on the live stores, so what the harness says it sent is
//! what can be shown. It is a record of what this tool sent and what it saw come back, not of
//! what a store received: a request that never left the machine is in it, and a request another
//! client made is not. It is the one file of a run that names buckets and keys, so it stays in
//! the private result directory and is never committed.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::error::BenchError;
use crate::role::Role;

/// One request, as it went out and as it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// When it was sent, milliseconds since the Unix epoch.
    pub unix_ms: u64,
    /// The store role it went to.
    pub role: Role,
    /// The HTTP method.
    pub method: String,
    /// The bucket the request addressed.
    pub bucket: String,
    /// The key, or for a listing the prefix.
    pub key: String,
    /// The status of the answer; `None` if there was none (connect failure, timeout).
    pub status: Option<u16>,
    /// How long it took, microseconds.
    pub latency_us: u64,
    /// Payload bytes of the request or the answer.
    pub bytes: u64,
    /// A short class of failure (`transport`, `not-found`, ...); `None` for a success.
    pub error_kind: Option<String>,
}

/// An append-only log of [`RequestRecord`]s, safe to share between tasks.
pub struct RequestLog {
    sink: Mutex<Box<dyn Write + Send>>,
    count: AtomicU64,
    cold: AtomicU64,
}

impl RequestLog {
    /// A log that appends to `path`, created if it is not there.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the file cannot be opened.
    pub fn create(path: &Path) -> Result<Self, BenchError> {
        let file = File::options()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| BenchError::Io {
                path: path.to_owned(),
                source,
            })?;
        Ok(Self::to_writer(file))
    }

    /// A log that writes to `writer`.
    #[must_use]
    pub fn to_writer(writer: impl Write + Send + 'static) -> Self {
        Self {
            sink: Mutex::new(Box::new(writer)),
            count: AtomicU64::new(0),
            cold: AtomicU64::new(0),
        }
    }

    /// Appends one record and flushes it, so a crash of the tool leaves every request it sent in
    /// the file.
    ///
    /// # Errors
    /// [`BenchError::Log`] if the write fails; the run must stop.
    pub fn record(&self, record: &RequestRecord) -> Result<(), BenchError> {
        let mut line = serde_json::to_string(record).expect("a record serialises");
        line.push('\n');
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        sink.write_all(line.as_bytes())
            .and_then(|()| sink.flush())
            .map_err(BenchError::Log)?;
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// How many records have been written.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// How many of the records written went to the cold role: one per HTTP request this tool
    /// sent to the cold store, a part of a multipart upload and a retry by the tool's own code
    /// each counted. The cold store guard subtracts it from the store's own request counter to
    /// get the requests that came from elsewhere.
    #[must_use]
    pub fn cold_count(&self) -> u64 {
        self.cold.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use super::*;

    /// A writer whose bytes the test can read after the log took it.
    #[derive(Clone, Default)]
    pub(crate) struct Shared(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that fails at the stage it is told to.
    struct Refusing {
        write: bool,
        flush: bool,
    }

    impl Write for Refusing {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.write {
                Err(io::Error::other("disk full"))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush
                .then(|| io::Error::other("flush"))
                .map_or(Ok(()), Err)
        }
    }

    fn record(key: &str) -> RequestRecord {
        RequestRecord {
            unix_ms: 7,
            role: Role::Instance,
            method: "PUT".to_owned(),
            bucket: "test-bucket".to_owned(),
            key: key.to_owned(),
            status: Some(200),
            latency_us: 1500,
            bytes: 4096,
            error_kind: None,
        }
    }

    /// Catches a record lost, merged with its neighbour or counted wrong: each is one JSON line
    /// that parses back to what was written.
    #[test]
    fn each_record_is_one_json_line_and_is_counted() {
        let shared = Shared::default();
        let log = RequestLog::to_writer(shared.clone());
        log.record(&record("a")).unwrap();
        log.record(&record("b")).unwrap();
        assert_eq!(log.count(), 2);
        let bytes = shared.0.lock().unwrap().clone();
        let lines: Vec<RequestRecord> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines, [record("a"), record("b")]);
    }

    /// Catches a failed write or flush being swallowed: a log with a gap must stop the run, and
    /// the failed record is not counted.
    #[test]
    fn a_failed_write_is_an_error_and_is_not_counted() {
        for (write, flush) in [(true, false), (false, true)] {
            let log = RequestLog::to_writer(Refusing { write, flush });
            let err = log.record(&record("a")).unwrap_err();
            assert!(matches!(err, BenchError::Log(_)), "{write} {flush}");
            assert_eq!(log.count(), 0);
        }
    }

    /// Catches a log file that cannot be opened being reported without its path.
    #[test]
    fn a_log_that_cannot_be_opened_names_its_path() {
        let path = Path::new("/nonexistent-dir-for-test/requests.jsonl");
        match RequestLog::create(path) {
            Err(BenchError::Io { path: p, .. }) => assert_eq!(p, path),
            other => panic!("expected Io, got {:?}", other.err()),
        }
    }
}
