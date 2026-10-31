//! The cold store's state, read again before every pass.
//!
//! Preflight reads whether the cold store is healing once, and a campaign of hours would carry
//! that reading in every result. The run asks [`ColdStateSource`] before every pass of every arm
//! instead, and the answer is stamped into the result (`cold_state`, one entry per pass).
//!
//! The source is the one preflight uses, read-only, in one of two modes. With a cold admin key in
//! the ops file it is `mc admin info` of the cold role (its drives and whether any reports
//! healing), with the heal status file's `percent_done` beside it (`source` `mc`). Without one,
//! and with a `cold_heal_status_file`, it is that file alone (`source` `file`) and `mc` is never
//! run: the file is the record of a writer on a host that can ask the store, stamped by that
//! writer, and a file older than the limit, or stamped more than 5 s after the reader's clock,
//! is unreadable. The writer's stamp is compared with the reader's clock, so both clocks must be
//! in sync (NTP): a writer ahead by more than 5 s is refused, and a writer behind uses up the
//! limit. The file is a trust input and is read only if it is a regular file of the running
//! user, not a link, not writable by group or others, and at most 64 KiB
//! ([`crate::preflight::trusted`]). The stamp recorded in the result is the file's. [`OpsColdState`] does that through the [`Host`] seam; a
//! test gives [`ColdStateSource`] its own readings. A reading that cannot be made stops the run,
//! as a load gate that cannot see does: a result that says nothing of the cold store's state must
//! not look like one that says it is quiet. An admin key that fails is that error too; nothing
//! falls back to the file.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::gate::LoadSample;
use crate::preflight::host::{Cmd, CmdOutput, Host};
use crate::preflight::mc::Mc;
use crate::preflight::ops::OpsFile;
use crate::preflight::{heal_percent, is_healing, live::sha256_hex, read_heal_file, why};
use crate::result::{ColdSource, ColdState, DriveState};
use crate::role::Role;
use crate::s3::unix_ms;

/// Where the cold store's state comes from.
pub trait ColdStateSource: Send + Sync {
    /// The state now. `None` for a run with no cold store to ask about.
    ///
    /// # Errors
    /// [`BenchError`] if the store cannot be asked and no status file answers: the run stops.
    fn read(&self) -> Result<Option<ColdState>, BenchError>;

    /// [`ColdStateSource::read`], given up after `limit`: a source that can wait on a command
    /// answers with an error then, having stopped the command. The default reads at once.
    ///
    /// # Errors
    /// As `read`; and [`BenchError`] if the reading took longer than `limit`.
    fn read_within(&self, _limit: Duration) -> ReadFuture<'_> {
        Box::pin(std::future::ready(self.read()))
    }
}

/// What [`ColdStateSource::read_within`] gives: a reading, in time.
pub type ReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ColdState>, BenchError>> + Send + 'a>>;

/// A run with no cold role: nothing to read.
pub struct NoColdState;

impl ColdStateSource for NoColdState {
    fn read(&self) -> Result<Option<ColdState>, BenchError> {
        Ok(None)
    }
}

/// The preflight's source, on a [`Host`]: `mc admin info` of the cold store (with the heal status
/// file's percent), or, with no cold admin key, the heal status file alone.
#[derive(Clone)]
pub struct OpsColdState {
    host: Arc<dyn Host>,
    ops: OpsFile,
    spec: StoreSpec,
    result_dir: PathBuf,
    max_age: Duration,
    now_ms: NowMs,
}

/// The clock of file mode: milliseconds since the Unix epoch now.
pub type NowMs = Arc<dyn Fn() -> u64 + Send + Sync>;

impl OpsColdState {
    /// A source for the cold store `spec`, asked with the cold admin key the ops file names;
    /// `mc` keeps its own files in `mc/` of `result_dir`.
    #[must_use]
    pub fn new(host: Arc<dyn Host>, ops: OpsFile, spec: StoreSpec, result_dir: PathBuf) -> Self {
        Self {
            host,
            ops,
            spec,
            result_dir,
            max_age: Duration::from_secs(30),
            now_ms: Arc::new(unix_ms),
        }
    }

    /// The oldest a status file's stamp may be in file mode (30 s unless set).
    #[must_use]
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The clock the status file's stamp is compared with (the system clock unless set).
    #[must_use]
    pub fn with_clock(mut self, now_ms: NowMs) -> Self {
        self.now_ms = now_ms;
        self
    }

    /// One reading, its commands stopped (and killed) once `stop` is set.
    fn read_until(&self, stop: &AtomicBool) -> Result<Option<ColdState>, BenchError> {
        let bounded = Stoppable {
            inner: &*self.host,
            stop,
        };
        let host: &dyn Host = &bounded;
        if let Some(path) = self.ops.cold_file_mode() {
            let now = (self.now_ms)();
            let file = read_heal_file(host, path, now, self.max_age).map_err(|e| unreadable(&e))?;
            return Ok(Some(ColdState {
                healing: file.is_healing(),
                percent_done: file.percent_done,
                drives: file.drives,
                read_unix_ms: file.read_unix_ms,
                source: Some(ColdSource::File),
            }));
        }
        let info = Mc::for_role(host, &self.ops, Role::Cold, &self.spec, &self.result_dir)
            .and_then(|mc| mc.info())
            .map_err(|e| BenchError::Ops(format!("cold state: {}", why(&e))))?;
        let percent = heal_percent(host, &self.ops)
            .map_err(|e| BenchError::Ops(format!("cold state: {e}")))?;
        Ok(Some(ColdState {
            healing: is_healing(Some(&info), percent),
            percent_done: percent,
            drives: drives_of(&info),
            read_unix_ms: unix_ms(),
            source: Some(ColdSource::Mc),
        }))
    }
}

impl ColdStateSource for OpsColdState {
    fn read(&self) -> Result<Option<ColdState>, BenchError> {
        self.read_until(&AtomicBool::new(false))
    }

    /// Reads on a thread of the blocking pool; if `limit` passes first, the command that is
    /// running is told to stop (its own child is killed, nothing else) and the reading is an
    /// error, whatever the status file would have said: a store that does not answer is not a
    /// store that is quiet.
    fn read_within(&self, limit: Duration) -> ReadFuture<'_> {
        Box::pin(async move {
            let stop = Arc::new(AtomicBool::new(false));
            let (this, flag) = (self.clone(), stop.clone());
            let mut task = tokio::task::spawn_blocking(move || this.read_until(&flag));
            if let Ok(done) = tokio::time::timeout(limit, &mut task).await {
                return done.unwrap_or(Err(unreadable("the reading did not end")));
            }
            stop.store(true, Ordering::SeqCst);
            task.await.ok();
            Err(unreadable(&format!(
                "no answer in {} s, the command was stopped",
                limit.as_secs()
            )))
        })
    }
}

fn unreadable(why: &str) -> BenchError {
    BenchError::Ops(format!("cold state: unreadable: {why}"))
}

/// A [`Host`] whose commands end when `stop` is set, as a command that timed out (the cold
/// store's admin tool is the one command a reading runs).
struct Stoppable<'a> {
    inner: &'a dyn Host,
    stop: &'a AtomicBool,
}

impl Host for Stoppable<'_> {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read_file(path)
    }

    fn read_status_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read_status_file(path)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }

    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        self.inner
            .run_until(cmd, self.stop)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }

    fn port_open(&self, port: u16) -> bool {
        self.inner.port_open(port)
    }

    fn load_sample(&self) -> Result<LoadSample, BenchError> {
        self.inner.load_sample()
    }
}

/// The drives of an `mc admin info` answer, each named by a short hash of its endpoint (or path),
/// sorted by that name. A drive with neither is named by the hash of the empty text.
#[must_use]
pub fn drives_of(info: &Value) -> Vec<DriveState> {
    let servers = info.pointer("/info/servers").and_then(Value::as_array);
    let mut drives: Vec<DriveState> = servers
        .into_iter()
        .flatten()
        .flat_map(|server| {
            server
                .get("drives")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .map(|drive| {
            let text = |key: &str| drive.get(key).and_then(Value::as_str);
            let name = text("endpoint")
                .or_else(|| text("path"))
                .unwrap_or_default();
            DriveState {
                id: format!("d-{}", &sha256_hex(name.as_bytes())[..8]),
                state: text("state").unwrap_or_default().to_owned(),
                healing: drive.get("healing").and_then(Value::as_bool) == Some(true),
            }
        })
        .collect();
    drives.sort_by(|a, b| a.id.cmp(&b.id));
    drives
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::preflight::host::SystemHost;

    /// Catches a drive set that names a host or path, one that loses a drive or its healing flag,
    /// and an order that depends on the order the store listed them in.
    #[test]
    fn drives_are_named_by_a_hash_and_sorted() {
        let info = json!({"info": {"servers": [
            {"drives": [
                {"endpoint": "http://host-mark:9000/data/disk2", "state": "ok", "healing": true},
                {"path": "/private-mark/disk1", "state": "offline"}
            ]},
            {"drives": [{"endpoint": "http://host-mark:9000/data/disk1", "state": "ok"}]}
        ]}});
        let drives = drives_of(&info);
        assert_eq!(drives.len(), 3);
        assert!(drives.windows(2).all(|w| w[0].id < w[1].id));
        let text = serde_json::to_string(&drives).unwrap();
        assert!(!text.contains("host-mark"), "{text}");
        assert!(!text.contains("private-mark"), "{text}");
        assert!(drives.iter().all(|d| d.id.len() == 10));
        assert!(drives.iter().all(|d| d.id.starts_with("d-")));
        let healing: Vec<_> = drives.iter().filter(|d| d.healing).collect();
        assert_eq!(healing.len(), 1);
        assert_eq!(healing[0].state, "ok");
        assert_eq!(drives.iter().filter(|d| d.state == "offline").count(), 1);
        // The same drives listed the other way round are the same set.
        let flipped = json!({"info": {"servers": [
            {"drives": [{"endpoint": "http://host-mark:9000/data/disk1", "state": "ok"}]},
            {"drives": [
                {"path": "/private-mark/disk1", "state": "offline"},
                {"endpoint": "http://host-mark:9000/data/disk2", "state": "ok", "healing": true}
            ]}
        ]}});
        assert_eq!(drives_of(&flipped), drives);
    }

    /// Catches an answer with no servers or no drives being read as a set with drives in it.
    #[test]
    fn an_answer_without_drives_gives_an_empty_set() {
        assert!(drives_of(&json!({})).is_empty());
        assert!(drives_of(&json!({"info": {"servers": [{"version": "v"}]}})).is_empty());
    }

    /// Catches a host that is told to stop answering for the whole machine, not for its commands:
    /// files, ports and the load are the inner host's own; a command runs to its end while the
    /// flag is down, and is an error of kind `TimedOut` once it is up and the command has not
    /// ended.
    #[test]
    fn a_stoppable_host_passes_everything_on_and_stops_only_commands() {
        let (down, up) = (AtomicBool::new(false), AtomicBool::new(true));
        let host = Stoppable {
            inner: &SystemHost,
            stop: &down,
        };
        assert!(host.exists(Path::new("/")));
        assert!(host.read_file(Path::new("/")).is_err());
        assert!(!host.port_open(1));
        let _ = host.load_sample();
        assert_eq!(host.run(&Cmd::new("echo", &["a"])).unwrap().stdout, "a\n");
        let stopped = Stoppable {
            inner: &SystemHost,
            stop: &up,
        };
        let err = stopped.run(&Cmd::new("sleep", &["3600"])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Catches a run with no cold store being asked anything.
    #[test]
    fn no_cold_store_gives_no_reading() {
        assert_eq!(NoColdState.read().unwrap(), None);
    }
}
