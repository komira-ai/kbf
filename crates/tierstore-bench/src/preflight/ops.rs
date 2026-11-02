//! The ops file: what the ops verbs (`preflight`, `provision`, `verify-teardown`) need to know
//! about the node and the stores beyond the run file.
//!
//! It is JSON, named `ops.json`, and lives in the result directory, which is private (the
//! request log and the run file's keys live with it). It names machines' paths, units, ports,
//! buckets and credential files, so it is never committed and nothing of it is copied into a
//! result: the verbs' own output carries role names, counts and hashes only. The shape:
//!
//! ```json
//! {
//!   "tierstore": {"binary": "...", "sha256": "...", "version": "0.0.37", "plan_flags": ["--hot"]},
//!   "live": {"unit": "...", "binary": "...", "port": 9300, "dirs": ["..."]},
//!   "test_instance": {"unit": "...", "port": 9301, "dirs": ["..."]},
//!   "live_buckets": {"cold": "..."},
//!   "admin": {"cold": "/path/to/admin-key.json"},
//!   "verifier": {"cold": "/path/to/reader-key.json"},
//!   "mc": "/path/to/mc",
//!   "node": {"roles": ["instance", "generator"], "link_interface": "..."},
//!   "cold_heal_status_file": "...",
//!   "paper": {"page": "...", "map": "..."}
//! }
//! ```
//!
//! A credentials file (`admin`, `verifier`) is JSON of `access_key_id` and `secret_access_key`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::host::{Host, read_text};
use crate::config::Secret;
use crate::error::BenchError;
use crate::role::Role;

/// The port of the farm's live tierstore. The test instance never uses it, whatever the file
/// says the live port is.
pub const LIVE_PORT: u16 = 9300;

/// The file name of the ops file inside the result directory.
pub const FILE_NAME: &str = "ops.json";

/// The pinned tierstore binary the test instance runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedBinary {
    /// Where the binary is.
    pub binary: PathBuf,
    /// The SHA-256 it must have, lowercase hex.
    pub sha256: String,
    /// The release it is, as recorded in the results.
    pub version: String,
    /// The flags the measurement plan uses; `--help` of the binary must list every one.
    pub plan_flags: Vec<String>,
}

/// The farm's live tierstore on this node, which the run must not touch.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveTierstore {
    /// Its systemd unit.
    pub unit: String,
    /// Its binary.
    pub binary: PathBuf,
    /// Its port.
    pub port: u16,
    /// Its state and data directories. Required: `run` refuses a result directory, a campaign
    /// ledger or a ledger lock that is, holds or lies inside one of them (links and `..`
    /// resolved), and refuses an ops file that lists none.
    pub dirs: Vec<PathBuf>,
}

/// The test instance of tierstore: its own unit, port and directories, all removed at teardown.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestInstance {
    /// Its systemd unit.
    pub unit: String,
    /// Its port.
    pub port: u16,
    /// Its state and data directories.
    pub dirs: Vec<PathBuf>,
}

/// Which roles this machine plays, for the hardware facts.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeOps {
    /// The roles this machine has in the run.
    #[serde(default)]
    pub roles: Vec<Role>,
    /// The network interface whose speed is recorded.
    pub link_interface: Option<String>,
}

/// The paper's page and the file that maps its open places to cells.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperFiles {
    /// The paper's HTML.
    pub page: PathBuf,
    /// The mapping, a JSON array whose entries carry a `paper_line`.
    pub map: PathBuf,
}

/// The ops file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsFile {
    /// The pinned binary.
    pub tierstore: PinnedBinary,
    /// The live tierstore on this node.
    pub live: LiveTierstore,
    /// The test instance on this node.
    pub test_instance: TestInstance,
    /// For each store role, the name of one live bucket: the one the scoped account must be
    /// refused.
    #[serde(default)]
    pub live_buckets: BTreeMap<Role, String>,
    /// For each store role, the admin key pair's file.
    #[serde(default)]
    pub admin: BTreeMap<Role, PathBuf>,
    /// For each store role, a key pair's file that may list the test bucket and its uploads
    /// (the admin's, or a read-only account's); `verify-teardown` reads with it.
    #[serde(default)]
    pub verifier: BTreeMap<Role, PathBuf>,
    /// The MinIO client; `mc` on `PATH` if absent.
    pub mc: Option<PathBuf>,
    /// This machine's roles.
    #[serde(default)]
    pub node: NodeOps,
    /// A file the operator keeps of the cold store's heal progress: JSON of `percent_done`, or,
    /// for a cold role with no `admin` entry (file mode), the writer's full record, see the
    /// README (`read_unix_ms`, `healing`, `percent_done`, `drives`, `version`).
    pub cold_heal_status_file: Option<PathBuf>,
    /// The paper and its map, for `paper-scan`.
    pub paper: Option<PaperFiles>,
}

/// A key pair read from a credentials file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPair {
    /// The access key id.
    pub access_key_id: String,
    /// The secret.
    pub secret_access_key: Secret,
}

impl OpsFile {
    /// Reads `ops.json` of the result directory `dir`.
    ///
    /// # Errors
    /// [`BenchError::Io`] if it cannot be read, [`BenchError::Ops`] if it is not an ops file.
    pub fn load(host: &dyn Host, dir: &Path) -> Result<Self, BenchError> {
        let path = dir.join(FILE_NAME);
        let text = read_text(host, &path)?;
        serde_json::from_str(&text)
            .map_err(|e| BenchError::Ops(format!("{FILE_NAME} is not an ops file: {e}")))
    }

    /// The heal status file the cold role is read from alone (file mode): the ops file has no
    /// admin key for `cold` and names a status file. `mc` is then never run for the cold role.
    #[must_use]
    pub fn cold_file_mode(&self) -> Option<&Path> {
        if self.admin.contains_key(&Role::Cold) {
            return None;
        }
        self.cold_heal_status_file.as_deref()
    }

    /// What is wrong with the test instance's isolation from the live one, one text per
    /// problem: the live unit's name, the live port or [`LIVE_PORT`], or a directory that is,
    /// holds or lies inside a live directory.
    #[must_use]
    pub fn isolation_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.test_instance.unit == self.live.unit {
            problems.push("the test unit has the live unit's name".to_owned());
        }
        if self.test_instance.port == self.live.port || self.test_instance.port == LIVE_PORT {
            problems.push("the test port is a live port".to_owned());
        }
        for test in &self.test_instance.dirs {
            for live in &self.live.dirs {
                if test.starts_with(live) || live.starts_with(test) {
                    problems.push("a test directory overlaps a live directory".to_owned());
                }
            }
        }
        problems
    }
}

/// Reads the key pair in `path`.
///
/// # Errors
/// [`BenchError::Io`] if the file cannot be read, [`BenchError::Ops`] if it is not a key pair.
pub fn read_key_pair(host: &dyn Host, path: &Path) -> Result<KeyPair, BenchError> {
    let text = read_text(host, path)?;
    // No detail of the parse error: it may quote the secret.
    serde_json::from_str(&text).map_err(|_| {
        BenchError::Ops("a key file is not JSON of access_key_id and secret_access_key".to_owned())
    })
}
