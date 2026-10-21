//! What both suites share: a farm of one storage-and-daemon node ([`Rig`]), a REAPI client
//! of its frontend, a builder of input trees and actions ([`Tree`], [`Job`]), the node's
//! planted action ([`Rig::plant`]), and the black-box probes ([`probe`], [`node`]).

use std::path::PathBuf;
use std::time::Duration;

use kbf_harness::farm::{Bins, Farm, Topology};
use kbf_harness::node_support::scratch_root;
use kbf_harness::procs::HANG;
use kbf_harness::reapi::{connect, execute, upload_action};
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::reapi::{Digest, ExecuteResponse};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers};
use tonic::transport::Channel;

pub use kbf_testkit::canary::{canary, noise};
pub use kbf_testkit::tree::{Job, Tree};

mod linux;
pub use linux::KillOnDrop;

/// The node as the suites see it: the platform-neutral trait from `kbf-testkit` (the Mac
/// seam) and this crate's Linux implementation of it.
pub mod node {
    pub use super::linux::LinuxNode;
    pub use kbf_testkit::node_view::{Isolation, NodeView};
}

/// The probes: the platform-neutral ones from `kbf-testkit`, and the Linux ones (`/proc`,
/// the cgroup tree) from this crate.
pub mod probe {
    pub use super::linux::{
        alive, cgroup_members, note_inherited, open_fds, processes_of, unexpected_fds,
        zombie_children,
    };
    pub use kbf_testkit::probe::*;

    /// [`kbf_testkit::probe::canary_outside_digests`] with the daemon's input-cache naming:
    /// `<sha256 hex>-<size>`, with `.x` for an executable.
    pub fn canary_outside_digests(
        node: &dyn super::node::NodeView,
        needle: &[u8],
    ) -> Result<Vec<std::path::PathBuf>, String> {
        kbf_testkit::probe::canary_outside_digests(
            node,
            needle,
            &super::linux::content_addressed_file,
        )
    }
}

/// A running farm of one storage-and-daemon node, and a client of its frontend.
pub struct Rig {
    pub farm: Farm,
    /// The farm's directory; every path of the node is below it.
    pub dir: PathBuf,
    rt: tokio::runtime::Runtime,
    channel: Channel,
    cas: std::sync::Arc<Transfers>,
    image: String,
}

impl Rig {
    /// A farm in a fresh directory named for `name`, ready for `Execute`. The farm has the
    /// one node (no plain daemon), so the one stand-in `crun` state serves every lease.
    pub fn start(name: &str) -> Self {
        let scratch = TestDir::under(
            scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-daemon-test"),
            name,
        );
        let dir = scratch.path().to_path_buf();
        probe::note_inherited();
        let bins = Bins {
            kbf: sibling_bin("kbf"),
            fake_crun: Some(sibling_bin("kbf-cli-fake-crun")),
        };
        let mut topology = Topology::rehearsal(1);
        topology.daemons = 0;
        let mut farm = Farm::start(&dir, &dir, &bins, &topology)
            .unwrap_or_else(|e| panic!("{e}"))
            .owning(scratch);
        farm.wait_ready().unwrap_or_else(|e| panic!("{e}"));
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let channel = rt.block_on(connect(farm.ca_pem(), farm.reapi()));
        let cas = std::sync::Arc::new(Transfers::over(channel.clone(), TransferConfig::default()));
        let image = farm.image().to_owned();
        Self {
            farm,
            image,
            dir,
            rt,
            channel,
            cas,
        }
    }

    /// The node as the probes see it; the farm's live roles are not residue.
    pub fn view(&mut self) -> node::LinuxNode {
        let table = self.farm.process_table();
        let roles = table
            .iter()
            .filter(|p| p.ended.is_none())
            .map(|p| p.pid)
            .collect();
        // Every role the farm has had leads a process group of its own: what is left in
        // one, beside a live role or after it ended, is the farm's.
        let groups = table.iter().map(|p| p.pid).collect();
        node::LinuxNode::new(&self.dir, roles).with_groups(groups)
    }

    /// The names of the node's lease directories now (the farm's roles do not matter here).
    pub fn leases(&self) -> std::collections::BTreeSet<String> {
        use node::NodeView as _;
        node::LinuxNode::new(&self.dir, Vec::new())
            .lease_dirs()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// The node's directory.
    pub fn node_dir(&self) -> PathBuf {
        self.dir.join("node-1")
    }

    /// The node's `--data-dir`.
    pub fn data(&self) -> PathBuf {
        self.node_dir().join("data")
    }

    /// The directory of the stand-in `crun`, its `crun-state` and the planted cgroup tree.
    pub fn bin(&self) -> PathBuf {
        self.node_dir().join("bin")
    }

    /// Sets what the stand-in `crun` runs as the next action (it ignores the REAPI Command,
    /// fake_crun.rs "action" knob): the steps of kbf-testkit's `kbf-act`, `;` between them.
    pub fn plant(&self, steps: &[&str]) {
        let state = self.bin().join("crun-state");
        let text: String = steps.iter().map(|s| format!("{s}\n")).collect();
        let tmp = state.join("action.new");
        std::fs::write(&tmp, text).expect("write the knob");
        std::fs::rename(&tmp, state.join("action")).expect("replace the knob");
    }

    /// Sets a switch file of the stand-in `crun` (`leave-state`, `delete-fails`, ...).
    pub fn knob(&self, name: &str, text: &str) {
        std::fs::write(self.bin().join("crun-state").join(name), text).expect("write the knob");
    }

    /// The text of a record the stand-in `crun` keeps (`calls`, `events`, `pid`, `root`...).
    pub fn record(&self, name: &str) -> String {
        std::fs::read_to_string(self.bin().join("crun-state").join(name)).unwrap_or_default()
    }

    /// Executes `job` and waits for the response (bounded by [`HANG`]).
    pub fn run(&self, job: &Job) -> ExecuteResponse {
        let handle = self.start_run(job);
        self.join(handle)
    }

    /// Starts `job`; the response is read with [`Rig::join`].
    pub fn start_run(&self, job: &Job) -> tokio::task::JoinHandle<ExecuteResponse> {
        let (channel, cas, job) = (self.channel.clone(), self.cas.clone(), job.clone());
        let image = self.image.clone();
        self.rt.spawn(async move {
            let action = upload_action(&cas, &image, &job).await;
            execute(channel, action).await
        })
    }

    /// The response of a job [`Rig::start_run`] started.
    pub fn join(&self, handle: tokio::task::JoinHandle<ExecuteResponse>) -> ExecuteResponse {
        self.rt.block_on(async {
            tokio::time::timeout(HANG, handle)
                .await
                .expect("the action ran in time")
                .expect("the task")
        })
    }

    /// The bytes of a blob of the farm's CAS, through the frontend.
    pub fn blob(&self, digest: &Digest) -> Option<Vec<u8>> {
        self.rt.block_on(async { self.cas.get(digest).await.ok() })
    }

    /// The stdout of an executed action.
    pub fn stdout_of(&self, response: &ExecuteResponse) -> String {
        let result = response.result.as_ref().expect("a result");
        if !result.stdout_raw.is_empty() {
            return String::from_utf8_lossy(&result.stdout_raw).into_owned();
        }
        let digest = result.stdout_digest.as_ref().expect("a stdout digest");
        let bytes = self.blob(digest).expect("the stdout blob");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Waits up to [`HANG`] for `ready` to hold, polling every 20 ms.
    pub fn wait_until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        let end = std::time::Instant::now() + HANG;
        while !ready() {
            assert!(
                std::time::Instant::now() < end,
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
