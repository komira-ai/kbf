//! What a package host keeps of its own: the verified package files and the record of them,
//! the trial that is on, in `<state dir>/package.json` and `<state dir>/packages/`.
//!
//! Both are root's, 0600 and 0700 (the state directory is private to root,
//! [`super::production::private_dir`]). A package file is kept only after it passed every
//! check of [`super::package`], under the name of the release file it came from, and the file
//! is hashed again before it is handed to the package manager, so a file that changed in the
//! directory since is never installed.
//!
//! The record is the whole truth about a trial: it is written, with the deadline, before the
//! package manager is asked for anything, so a restart of this program or of the host finds it.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::answer::Code;
use super::backend::Fault;
use super::linux::{copy_hashing, state_fault};
use crate::trust::Trust;

/// A verified package file this host keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kept {
    /// The version text the package carries (the deb's spelling).
    pub version: String,
    /// The sha256 the file had when it was verified.
    pub sha256: String,
    /// The channel it was fetched through; the package that ran before it is fetched through
    /// the same one when this host holds no copy.
    pub channel: String,
}

/// A package on trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trial {
    /// The release on trial (all 40 digits).
    pub commit: String,
    /// The release that ran before it, whose package file is kept.
    pub from: String,
    /// When the host goes back by itself (Unix seconds).
    pub deadline_unix: u64,
    /// The name of the swap, for the log.
    pub activation: String,
}

/// `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The kept package files, by commit.
    #[serde(default)]
    pub files: BTreeMap<String, Kept>,
    /// The package on trial.
    #[serde(default)]
    pub pending: Option<Trial>,
}

impl Record {
    /// The kept file of the release whose package carries `version`.
    pub fn commit_of(&self, version: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(_, kept)| kept.version == version)
            .map(|(commit, _)| commit.as_str())
    }
}

/// The directory and the trust it is read under.
pub struct Store {
    dir: PathBuf,
    trust: Trust,
    /// The suffix of a kept file's name: `<platform>.<extension>`.
    suffix: String,
}

impl Store {
    pub fn new(dir: PathBuf, trust: Trust, platform: &str, extension: &str) -> Self {
        Self {
            dir,
            trust,
            suffix: format!("{platform}.{extension}"),
        }
    }

    fn record_file(&self) -> PathBuf {
        self.dir.join("package.json")
    }

    fn files_dir(&self) -> PathBuf {
        self.dir.join("packages")
    }

    fn file_of(&self, commit: &str) -> PathBuf {
        self.files_dir()
            .join(format!("kbf-{commit}-{}", self.suffix))
    }

    /// The record; an empty one when there is none yet.
    ///
    /// # Errors
    /// The file is not root's alone, or does not parse.
    pub fn load(&self) -> Result<Record, Fault> {
        match fs::symlink_metadata(self.record_file()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Record::default()),
            Err(e) => return Err(state_fault(format!("package.json: {e}"))),
            Ok(_) => {}
        }
        let text = self
            .trust
            .read(&self.record_file())
            .map_err(|e| state_fault(format!("package.json: {e}")))?;
        serde_json::from_str(&text).map_err(|e| state_fault(format!("package.json: {e}")))
    }

    /// Write the record whole, by a rename.
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn save(&self, record: &Record) -> Result<(), Fault> {
        let text = serde_json::to_string(record).map_err(state_fault)?;
        let aside = self.dir.join("package.json.new");
        let _ = fs::remove_file(&aside);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&aside, self.record_file()))
            .map_err(|e| state_fault(format!("package.json: {e}")))
    }

    /// Keep the verified package file `from` as the release `commit`'s, copying it (and
    /// hashing the copy, which must be the digest it was verified at).
    ///
    /// # Errors
    /// The copy failed or is not the verified bytes.
    pub fn keep(&self, commit: &str, from: &Path, sha256: &str, cap: u64) -> Result<(), Fault> {
        let failed = |why: String| Fault::failed(Code::StageFailed, why);
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(self.files_dir())
            .map_err(|e| failed(format!("packages: {e}")))?;
        let target = self.file_of(commit);
        let aside = target.with_extension("new");
        let _ = fs::remove_file(&aside);
        let copied = copy_hashing(from, &aside, cap).map_err(|e| failed(format!("copy: {e}")))?;
        if copied.sha256 != sha256 {
            let _ = fs::remove_file(&aside);
            return Err(failed(
                "the verified file changed while it was kept".to_owned(),
            ));
        }
        fs::rename(&aside, &target).map_err(|e| failed(format!("packages: {e}")))
    }

    /// The path of the kept file of `commit`, once it is hashed again and is the file that was
    /// verified.
    ///
    /// # Errors
    /// A refusal when the file is gone or is not the verified bytes: nothing is installed.
    pub fn verified(&self, commit: &str, kept: &Kept, cap: u64) -> Result<PathBuf, Fault> {
        let path = self.file_of(commit);
        let sha = hash_of(&path, cap)
            .map_err(|e| Fault::refused(Code::VerifyFailed, format!("the kept package: {e}")))?;
        if sha != kept.sha256 {
            return Err(Fault::refused(
                Code::VerifyFailed,
                "the kept package is not the file that was verified",
            ));
        }
        Ok(path)
    }

    /// Whether a file is kept for `commit`.
    pub fn has(&self, commit: &str) -> bool {
        fs::symlink_metadata(self.file_of(commit)).is_ok_and(|m| m.is_file())
    }

    /// Forget every kept file but those of `keep`, in the record and on disk. The record is
    /// not saved here.
    pub fn prune(&self, record: &mut Record, keep: &[&str]) {
        record
            .files
            .retain(|commit, _| keep.contains(&commit.as_str()));
        let Ok(entries) = fs::read_dir(self.files_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let wanted = keep
                .iter()
                .any(|commit| entry.path() == self.file_of(commit));
            if !wanted {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// The sha256 of the regular file `path` (no link followed), read whole in a bounded way.
fn hash_of(path: &Path, cap: u64) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(crate::trust::nofollow())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut hasher = Sha256::new();
    let mut capped = file.take(cap.saturating_add(1));
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut len = 0_u64;
    loop {
        let n = capped.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hasher.update(&buffer[..n]);
    }
    if len > cap {
        return Err(std::io::Error::other("larger than the limit"));
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
