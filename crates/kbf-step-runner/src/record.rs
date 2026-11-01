//! The step record: the updater's memory of one update (docs/design/host-updates.md, 3.5).
//!
//! Before each step the record is written and fsynced (temporary file, fsync, rename, directory
//! fsync) with the step in state `running`; when the step ends it is written again as `done`
//! with the exit code and the hook's last line. It holds no secret. It carries a `schema`
//! number; a record newer than this binary is never changed, only reported.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use update_hook::{Step, Verdict};

use crate::clock::Stamp;

/// The schema this binary writes.
pub const SCHEMA: u32 = 1;
/// The record's file name in the state directory.
pub const RECORD_FILE: &str = "step-record.json";
/// The params file's name in the state directory.
pub const PARAMS_FILE: &str = "params.toml";

/// Which release's hook a step runs from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookSel {
    /// The staged (new) release: the rule for every step.
    New,
    /// The previous release: `rollback` only, when the new hook cannot start at all.
    Previous,
}

/// Where a step is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    /// The step is chosen and has not started (or its unit was stopped on resume).
    Pending,
    /// The step started at `started` and must end by `deadline_secs` on the same boot.
    Running { started: Stamp, deadline_secs: u64 },
    /// The step ended.
    Done {
        code: Option<i32>,
        last_line: Option<String>,
    },
    /// The update is over.
    Finished {
        verdict: Verdict,
        reason: Option<String>,
    },
}

/// What the runner remembers between steps, besides the step.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// A step reported `reversible: false`.
    pub committed: bool,
    /// The last `verify` reported `promotion_pending`.
    pub promotion_pending: bool,
    /// What to report when the running `rollback` succeeds.
    pub then: Option<Verdict>,
    /// How many times `rollback` or `cleanup` was run again.
    pub retry: u32,
    /// When `cutover` succeeded: the verify window began.
    pub verify_started: Option<Stamp>,
    /// When the post-success watcher began; `Some` while it runs.
    pub watching: Option<Stamp>,
    /// The `reason` of the step that sent the update to `rollback`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The step record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: u32,
    pub service: String,
    pub target_version: String,
    pub artifact_digest: String,
    /// The sha256 of the artifact (tarball) the order names.
    pub tarball_digest: String,
    pub previous_digest: Option<String>,
    /// The number of this update on this host; the unit names carry it.
    pub generation: u64,
    /// The params file of this update: written once, the same path for every step, removed when
    /// the update ends or is abandoned.
    pub params: PathBuf,
    pub step: Step,
    pub attempt: u32,
    pub state: State,
    pub hook: HookSel,
    pub progress: Progress,
}

impl Record {
    /// The deterministic name of the unit a step of this update runs in.
    pub fn unit(&self, step: Step) -> String {
        format!("updater-{}-{}-{}", self.service, step, self.generation)
    }
}

/// What a state directory holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Absent,
    Current(Box<Record>),
    /// A record written by a newer binary: read, never changed.
    Newer {
        schema: u64,
    },
}

/// A place in the writing of a record, for a test that stops the updater there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Before the n-th write (counted from `1`) touched the disk.
    BeforeWrite(u64),
    /// After the n-th write was durable.
    AfterWrite(u64),
}

/// Why a record could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("cannot use the state directory: {0}")]
    Io(#[from] io::Error),
    #[error("the step record is not readable: {0}")]
    Corrupt(String),
    /// A test gate stopped the updater here, as a crash would.
    #[error("stopped at {0:?}")]
    Crashed(CrashPoint),
}

/// A gate that answers `true` at the points where the updater is to die.
pub type CrashGate = Arc<dyn Fn(CrashPoint) -> bool + Send + Sync>;

/// The record and the params file of one service's updates, in its state directory.
pub struct RecordStore {
    dir: PathBuf,
    gate: Option<CrashGate>,
    writes: AtomicU64,
}

impl RecordStore {
    /// The store in `dir`, which exists.
    pub fn new(dir: &Path) -> RecordStore {
        RecordStore {
            dir: dir.to_path_buf(),
            gate: None,
            writes: AtomicU64::new(0),
        }
    }

    /// The same store, stopped wherever `gate` says: the seam of the crash-and-resume tests.
    pub fn with_crash_gate(mut self, gate: CrashGate) -> RecordStore {
        self.gate = Some(gate);
        self
    }

    /// The params file the updater writes once per update.
    pub fn params_path(&self) -> PathBuf {
        self.dir.join(PARAMS_FILE)
    }

    /// Reads the record.
    pub fn load(&self) -> Result<Loaded, RecordError> {
        let bytes = match fs::read(self.dir.join(RECORD_FILE)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Absent),
            Err(e) => return Err(e.into()),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| RecordError::Corrupt(e.to_string()))?;
        let schema = value.get("schema").and_then(serde_json::Value::as_u64);
        if let Some(schema) = schema.filter(|schema| *schema > u64::from(SCHEMA)) {
            return Ok(Loaded::Newer { schema });
        }
        let record =
            serde_json::from_value(value).map_err(|e| RecordError::Corrupt(e.to_string()))?;
        Ok(Loaded::Current(Box::new(record)))
    }

    /// Writes the record, durably.
    pub fn save(&self, record: &Record) -> Result<(), RecordError> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        self.cross(CrashPoint::BeforeWrite(n))?;
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| RecordError::Corrupt(e.to_string()))?;
        write_atomic(&self.dir, RECORD_FILE, &bytes)?;
        self.cross(CrashPoint::AfterWrite(n))
    }

    /// Refuses a state directory that is not the updater's alone: the params file and the record
    /// live there, so it must be owned by the running user and closed to group and others.
    pub fn check_private(&self) -> Result<(), RecordError> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(&self.dir)?;
        // One comparison: a directory, mine, and closed to group and others.
        let mine = meta.uid() == rustix::process::geteuid().as_raw();
        let private = (meta.is_dir(), mine, meta.mode() & 0o077 == 0);
        if private == (true, true, true) {
            return Ok(());
        }
        Err(RecordError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the state directory is not a private directory of the updater",
        )))
    }

    /// Removes the params file; one that is not there is fine.
    pub fn remove_params(&self) -> Result<(), RecordError> {
        match fs::remove_file(self.params_path()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Writes the params file, durably.
    pub fn save_params(&self, text: &str) -> Result<(), RecordError> {
        Ok(write_atomic(&self.dir, PARAMS_FILE, text.as_bytes())?)
    }

    fn cross(&self, point: CrashPoint) -> Result<(), RecordError> {
        match &self.gate {
            Some(gate) if gate(point) => Err(RecordError::Crashed(point)),
            _ => Ok(()),
        }
    }
}

/// Temporary file, fsync, rename, directory fsync.
pub(crate) fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(name))?;
    File::open(dir)?.sync_all()
}
