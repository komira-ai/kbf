//! A CAS in memory, actions stored in it, and a VM driver over a fake host.

#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kbf_daemon::Work;
use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_driver_vm::{FakeHost, VmConfig, VmDriver};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Digest, Directory, FileNode, Platform};
use kbf_types::{LeaseId, Resources};
use prost::Message;

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(10);

pub const GIB: u64 = 1 << 30;

/// A CAS in memory.
#[derive(Debug, Default)]
pub struct MemoryCas {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryCas {
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    pub fn blob(&self, digest: &Digest) -> Vec<u8> {
        self.lock()
            .get(&label(digest))
            .cloned()
            .unwrap_or_else(|| panic!("blob {} is stored", label(digest)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.lock()
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }
}

/// An action to store: `sh -c <script>` with an empty input root unless changed.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub outputs: Vec<String>,
    pub inputs: Vec<(String, Vec<u8>)>,
    pub properties: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl Spec {
    pub fn sh(script: &str) -> Self {
        Self::argv(&["/bin/sh", "-c", script])
    }

    pub fn argv(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            ..Self::default()
        }
    }

    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn outputs(mut self, outputs: &[&str]) -> Self {
        self.outputs = outputs.iter().map(|o| (*o).to_owned()).collect();
        self
    }

    pub fn input(mut self, name: &str, bytes: &[u8]) -> Self {
        self.inputs.push((name.to_owned(), bytes.to_vec()));
        self
    }

    pub fn property(mut self, name: &str, value: &str) -> Self {
        self.properties.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Stores the action in `cas` and returns its digest.
    pub fn store(&self, cas: &MemoryCas) -> Digest {
        let root = Directory {
            files: (self.inputs.iter())
                .map(|(name, bytes)| FileNode {
                    name: name.clone(),
                    digest: Some(cas.insert(bytes.clone())),
                    ..FileNode::default()
                })
                .collect(),
            ..Directory::default()
        };
        let command = Command {
            arguments: self.argv.clone(),
            environment_variables: (self.env.iter())
                .map(|(name, value)| EnvironmentVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            output_paths: self.outputs.clone(),
            ..Command::default()
        };
        let action = Action {
            command_digest: Some(cas.insert(command.encode_to_vec())),
            input_root_digest: Some(cas.insert(root.encode_to_vec())),
            timeout: self.timeout.map(|t| prost_types::Duration {
                seconds: i64::try_from(t.as_secs()).expect("seconds"),
                nanos: i32::try_from(t.subsec_nanos()).expect("nanos"),
            }),
            platform: (!self.properties.is_empty()).then(|| Platform {
                properties: (self.properties.iter())
                    .map(|(name, value)| Property {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            }),
            ..Action::default()
        };
        cas.insert(action.encode_to_vec())
    }
}

/// A fresh scratch directory for one test, holding a golden image (`image/`, one disk
/// file) and the driver's scratch root (`vm/`).
pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-vm")
        .join(format!("{name}-{}", std::process::id()));
    if std::fs::symlink_metadata(&dir).is_ok() {
        kbf_outputs::remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(dir.join("image/data")).expect("image");
    std::fs::write(dir.join("image/disk.img"), b"golden").expect("disk");
    std::fs::write(dir.join("image/data/aux.img"), b"aux").expect("aux");
    dir
}

/// A configuration for tests: a fast boot poll and no free-space floor (a CI runner's
/// disk is small).
pub fn config(scratch: &Path) -> VmConfig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut config = VmConfig::new(scratch.join("image"), scratch.join("vm"));
    config.boot_poll = Duration::from_millis(10);
    config.min_free_bytes = 0;
    config
}

pub type Driver = VmDriver<FakeHost, MemoryCas>;

pub fn driver(config: VmConfig, host: &Arc<FakeHost>, cas: &Arc<MemoryCas>) -> Arc<Driver> {
    Arc::new(VmDriver::new(config, Arc::clone(host), Arc::clone(cas)).expect("driver"))
}

/// A `vm` lease booked at the default VM size: 6 cores and 17 GiB.
pub fn work(seq: u64, action_digest: Digest) -> Work {
    Work {
        lease_id: LeaseId::new(1, seq),
        kind: "vm".to_owned(),
        action_digest,
        resources: Resources::new(6000, 17 * GIB),
    }
}

/// The name lease `1.seq` has on the host and on disk.
pub fn name(seq: u64) -> String {
    format!("lease-1-{seq}")
}

/// The outputs share of lease `1.seq`, which the fake guest sees at its host path.
pub fn outputs_share(config: &VmConfig, seq: u64) -> String {
    config
        .leases()
        .join(name(seq))
        .join("outputs")
        .display()
        .to_string()
}

/// Everything in the lease and clone directories.
pub fn leftovers(config: &VmConfig) -> Vec<PathBuf> {
    [config.leases(), config.clones()]
        .iter()
        .flat_map(|dir| std::fs::read_dir(dir).expect("read dir"))
        .map(|entry| entry.expect("entry").path())
        .collect()
}

/// Waits until `done` holds, at most [`PROMPT`].
pub async fn until(what: &str, done: impl Fn() -> bool) {
    let by = Instant::now() + PROMPT;
    while !done() {
        assert!(Instant::now() < by, "{what}: not within {PROMPT:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until lease `1.seq`'s command has started in its guest: the guest has made its
/// stdout file in the outputs share.
pub async fn until_running(config: &VmConfig, seq: u64) {
    let stdout = PathBuf::from(outputs_share(config, seq)).join("stdout");
    until("the command started", || stdout.exists()).await;
}
