//! `frontend.json`, root's and 0600: what this program knows of the frontend container that
//! the container runtime does not say.
//!
//! - the image id of each release that was verified and loaded (`images`), so that a
//!   container is made from the id this program checked and never from a tag that could since
//!   have been moved;
//! - the release that runs (`current`);
//! - the release on trial (`pending`), written before anything is stopped: its commit and
//!   image, the commit and image it replaced, the deadline, and the configuration the
//!   containers of this switch are made from. A restart of this program or of the host finds
//!   it, and the old container is put back from it.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::config::FrontendConfig;
use crate::trust::Trust;

/// A release and the image it runs from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub commit: String,
    pub image: String,
}

/// A release on trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trial {
    pub commit: String,
    pub image: String,
    /// The release that ran, and its image, which a revert goes back to.
    pub from: String,
    pub from_image: String,
    pub deadline_unix: Option<u64>,
    /// The configuration as it was when the switch began.
    pub config: FrontendConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The image id of each release that is loaded, by commit.
    #[serde(default)]
    pub images: BTreeMap<String, String>,
    #[serde(default)]
    pub current: Option<Slot>,
    #[serde(default)]
    pub pending: Option<Trial>,
}

impl Record {
    /// Read the file at `path`; a record with nothing in it when there is no file.
    ///
    /// # Errors
    /// The file is there and is not root's alone, or does not parse.
    pub fn load(trust: &Trust, path: &Path) -> Result<Self, String> {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("frontend.json: {e}")),
            Ok(_) => {}
        }
        let text = trust
            .read(path)
            .map_err(|e| format!("frontend.json: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("frontend.json: {e}"))
    }

    /// Write the file at `path` whole or not at all (a new file beside it, then a rename).
    ///
    /// # Errors
    /// The file could not be written.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let say = |e: std::io::Error| format!("frontend.json: {e}");
        let text = serde_json::to_string(self).map_err(|e| format!("frontend.json: {e}"))?;
        let aside = path.with_extension("json.new");
        let _ = fs::remove_file(&aside);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)
            .map_err(say)?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&aside, path))
            .map_err(say)
    }

    /// Forget the images of every release but those the record still needs: the one that
    /// runs, the one on trial and the one it replaced, and `keep`.
    pub fn prune(&mut self, keep: &str) {
        let needed: Vec<String> = self
            .current
            .iter()
            .map(|s| s.commit.clone())
            .chain(
                self.pending
                    .iter()
                    .flat_map(|t| [t.commit.clone(), t.from.clone()]),
            )
            .chain([keep.to_owned()])
            .collect();
        self.images.retain(|commit, _| needed.contains(commit));
    }
}
