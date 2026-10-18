//! `update.json`: the intent record in the proxy state directory (FP section 4, Q5).
//! Written by temp file, fsync, rename and a directory fsync on every phase change, so a
//! reader sees the old record or the new one and never a torn one. Mode 0600.
//!
//! A record that is missing is `Ok(None)`. One that cannot be read or parsed is an error:
//! the API then refuses new updates (`409 state_lost`) and leaves both children as the
//! generation holder says.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::model::{Halt, Intent};
use super::ports::Recorder;

const NAME: &str = "update.json";
const TEMP: &str = ".update.json.tmp";

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("update.json cannot be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("update.json is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
}

/// Writes the intent record under `dir`.
pub struct FileRecorder {
    dir: PathBuf,
}

impl FileRecorder {
    pub fn new(dir: &Path) -> FileRecorder {
        FileRecorder {
            dir: dir.to_owned(),
        }
    }

    pub fn load(dir: &Path) -> Result<Option<Intent>, LoadError> {
        match fs::read(dir.join(NAME)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, intent: &Intent) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(intent).map_err(std::io::Error::other)?;
        let temp = self.dir.join(TEMP);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, self.dir.join(NAME))?;
        File::open(&self.dir)?.sync_all()
    }
}

impl Recorder for FileRecorder {
    fn record(&self, intent: &Intent) -> Result<(), Halt> {
        self.write(intent)
            .map_err(|e| Halt::Port(format!("writing update.json: {e}")))
    }
}
