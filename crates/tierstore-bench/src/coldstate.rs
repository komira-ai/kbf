//! The cold store's state, read again before every pass.
//!
//! Preflight reads whether the cold store is healing once, and a campaign of hours would carry
//! that reading in every result. The run asks [`ColdStateSource`] before every pass of every arm
//! instead, and the answer is stamped into the result (`cold_state`, one entry per pass).
//!
//! The source is the one preflight uses, read-only: `mc admin info` of the cold role (its
//! drives and whether any reports healing) and the operator's heal status file
//! (`cold_heal_status_file` of the ops file, `percent_done`). [`OpsColdState`] does that through
//! the [`Host`] seam; a test gives [`ColdStateSource`] its own readings. A reading that cannot be
//! made stops the run, as a load gate that cannot see does: a result that says nothing of the
//! cold store's state must not look like one that says it is quiet.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::preflight::host::Host;
use crate::preflight::mc::Mc;
use crate::preflight::ops::OpsFile;
use crate::preflight::{heal_percent, is_healing, live::sha256_hex};
use crate::result::{ColdState, DriveState};
use crate::role::Role;
use crate::s3::unix_ms;

/// Where the cold store's state comes from.
pub trait ColdStateSource: Send + Sync {
    /// The state now. `None` for a run with no cold store to ask about.
    ///
    /// # Errors
    /// [`BenchError`] if the store cannot be asked and no status file answers: the run stops.
    fn read(&self) -> Result<Option<ColdState>, BenchError>;
}

/// A run with no cold role: nothing to read.
pub struct NoColdState;

impl ColdStateSource for NoColdState {
    fn read(&self) -> Result<Option<ColdState>, BenchError> {
        Ok(None)
    }
}

/// The preflight's source, on a [`Host`]: `mc admin info` of the cold store and the heal status
/// file of the ops file.
pub struct OpsColdState {
    host: Arc<dyn Host>,
    ops: OpsFile,
    spec: StoreSpec,
    result_dir: PathBuf,
}

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
        }
    }
}

impl ColdStateSource for OpsColdState {
    fn read(&self) -> Result<Option<ColdState>, BenchError> {
        let host: &dyn Host = &*self.host;
        let info = Mc::for_role(host, &self.ops, Role::Cold, &self.spec, &self.result_dir)
            .and_then(|mc| mc.info())
            .ok();
        let percent = heal_percent(host, &self.ops)
            .map_err(|e| BenchError::Ops(format!("cold state: {e}")))?;
        if info.is_none() && percent.is_none() {
            return Err(BenchError::Ops(
                "cold state: neither the admin info nor a status file gives the heal state"
                    .to_owned(),
            ));
        }
        Ok(Some(ColdState {
            healing: is_healing(info.as_ref(), percent),
            percent_done: percent,
            drives: info.as_ref().map(drives_of).unwrap_or_default(),
            read_unix_ms: unix_ms(),
        }))
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

    /// Catches a run with no cold store being asked anything.
    #[test]
    fn no_cold_store_gives_no_reading() {
        assert_eq!(NoColdState.read().unwrap(), None);
    }
}
