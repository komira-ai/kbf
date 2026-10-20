//! What both suites share: a farm of one storage-and-daemon node ([`Rig`]), a REAPI client
//! of its frontend, a builder of input trees and actions ([`Tree`], [`Job`]), the node's
//! planted action ([`Rig::plant`]), and the black-box probes ([`probe`], [`node`]).

use std::path::PathBuf;
use std::time::Duration;

use kbf_harness::farm::{Bins, Farm, Topology};
use kbf_harness::node_support::scratch_root;
use kbf_harness::procs::HANG;
use kbf_harness::stand_in::sibling_bin;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, Command as ReapiCommand, Digest, Directory, DirectoryNode, ExecuteRequest,
    ExecuteResponse, FileNode, Platform, SymlinkNode,
};
use kbf_testkit::dir::TestDir;
use kbf_transfer::{Cas as _, TransferConfig, Transfers};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

pub mod node;
pub mod probe;

/// A directory of an action's input root.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub files: Vec<(String, Vec<u8>)>,
    pub links: Vec<(String, String)>,
    pub dirs: Vec<(String, Tree)>,
}

impl Tree {
    /// `in/<name>` holding `bytes`: the shape every action here reads.
    pub fn with_input(name: &str, bytes: &[u8]) -> Self {
        Self {
            dirs: vec![(
                "in".to_owned(),
                Self {
                    files: vec![(name.to_owned(), bytes.to_vec())],
                    ..Self::default()
                },
            )],
            ..Self::default()
        }
    }
}

/// One action: its input root and the output paths it declares.
#[derive(Clone, Debug, Default)]
pub struct Job {
    pub input: Tree,
    pub outputs: Vec<String>,
    /// The action's own timeout; none is the server's.
    pub timeout: Option<Duration>,
    /// Whether the server may serve the result from its action cache.
    pub cacheable: bool,
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
            scratch_root(PathBuf::from(env!("CARGO_TARGET_TMPDIR"))).join("kbf-blackbox"),
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
        let channel = rt.block_on(connect(&farm));
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

async fn connect(farm: &Farm) -> Channel {
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(farm.ca_pem()))
        .domain_name("localhost");
    Endpoint::from_shared(format!("https://{}", farm.reapi()))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect to the frontend")
}

/// Uploads `tree` (children first); its digest.
fn upload_tree<'a>(
    cas: &'a Transfers,
    tree: &'a Tree,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Digest> + Send + 'a>> {
    Box::pin(async move {
        let mut dir = Directory::default();
        for (name, bytes) in &tree.files {
            let digest = cas.put(bytes.clone()).await.expect("upload");
            dir.files.push(FileNode {
                name: name.clone(),
                digest: Some(digest),
                ..FileNode::default()
            });
        }
        for (name, target) in &tree.links {
            dir.symlinks.push(SymlinkNode {
                name: name.clone(),
                target: target.clone(),
                ..SymlinkNode::default()
            });
        }
        for (name, child) in &tree.dirs {
            dir.directories.push(DirectoryNode {
                name: name.clone(),
                digest: Some(upload_tree(cas, child).await),
            });
        }
        // REAPI: each list is sorted by name.
        dir.files.sort_by(|a, b| a.name.cmp(&b.name));
        dir.directories.sort_by(|a, b| a.name.cmp(&b.name));
        dir.symlinks.sort_by(|a, b| a.name.cmp(&b.name));
        cas.put(dir.encode_to_vec()).await.expect("upload")
    })
}

/// Uploads the action of `job`; its digest. Unless the job is cacheable the action is
/// `do_not_cache`, so every run is a run.
async fn upload_action(cas: &Transfers, image: &str, job: &Job) -> Digest {
    let root = upload_tree(cas, &job.input).await;
    // The stand-in `crun` runs its planted steps whatever the arguments are.
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        output_paths: job.outputs.clone(),
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(cas.put(command.encode_to_vec()).await.expect("upload")),
        input_root_digest: Some(root),
        timeout: job.timeout.map(|t| prost_types::Duration {
            seconds: i64::try_from(t.as_secs()).expect("seconds"),
            nanos: 0,
        }),
        do_not_cache: !job.cacheable,
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: image.to_owned(),
            }],
        }),
        ..Action::default()
    };
    cas.put(action.encode_to_vec()).await.expect("upload")
}

/// Executes `action` until its operation is done.
async fn execute(reapi: Channel, action: Digest) -> ExecuteResponse {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    ExecuteResponse::decode(any.value.as_slice()).expect("response")
}

/// Kills the processes it is given when it drops: a test that plants a process the product
/// cannot reach must not leave it to the host.
#[derive(Default)]
pub struct KillOnDrop(pub Vec<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for &pid in &self.0 {
            if let Some(pid) = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

/// `n` hex characters, unpredictable to the product and different for each `seed`: an
/// incompressible canary that no file legitimately holds by accident.
pub fn canary(seed: u64, n: usize) -> String {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03;
    let mut out = String::with_capacity(n);
    while out.len() < n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push_str(&format!("{state:016x}"));
    }
    out.truncate(n);
    out
}

/// Binary input bytes, different for each `seed`.
pub fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}
