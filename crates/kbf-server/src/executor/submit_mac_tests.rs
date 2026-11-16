//! The job of a run of an action that asks for a Mac.

use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{ExecMode, JobSpec, MountMode, NetworkLevel, mount};
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

use super::{Booking, FRONTEND_LABEL, INSTANCE_LABEL, Settled, job_id, prepare};
use crate::executor::mac::{WorkerOs, mac_argv};
use crate::executor::test_support::{GIB, Harness};
use crate::executor::traits::InputsError;
use crate::executor::{
    ActionFacts, Ctx, DEFAULT_MAC_OS_VERSION, Deps, Inputs, MAC_OUT_TARGET, MacPool, PutDirRoots,
    ReadGrant, System, TREE_TARGET, TaskOut,
};

const TREE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

/// Inputs of a Mac action of `os_version` whose run wrote the tree [`TREE`] (none when
/// `tree` is false).
struct Mac {
    os_version: Option<u32>,
    tree: bool,
    /// A Linux action instead.
    linux: bool,
}

#[tonic::async_trait]
impl Inputs for Mac {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/kbf@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: match self.linux {
                true => System::Linux,
                false => System::Macos {
                    os_version: self.os_version,
                },
            },
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Err(InputsError::Invalid(
            "a Mac run asks for no grant".to_owned(),
        ))
    }

    async fn tree(&self, lease: LeaseId) -> Result<String, InputsError> {
        match self.tree {
            true => Ok(TREE.to_owned()),
            false => Err(InputsError::Unavailable(format!(
                "no input tree was written for {lease}"
            ))),
        }
    }
}

fn pool(roots: Option<&str>) -> MacPool {
    MacPool {
        hosts: 1,
        bundle: BUNDLE.to_owned(),
        os_version: DEFAULT_MAC_OS_VERSION,
        roots: roots.map(|root| PutDirRoots::new(root).expect("roots")),
    }
}

/// What a test sets: the worker, the pool, the outputs' mode, the action's system and
/// version, the booking.
struct Case {
    on: WorkerOs,
    linux: bool,
    mac: Option<MacPool>,
    put_dir: bool,
    os_version: Option<u32>,
    tree: bool,
    booking: Booking,
}

const SHARE: Booking = Booking::Share {
    millicpus: 1_500,
    memory_bytes: GIB,
};

impl Default for Case {
    fn default() -> Self {
        Self {
            on: WorkerOs::Mac,
            linux: false,
            mac: Some(pool(None)),
            put_dir: true,
            os_version: None,
            tree: true,
            booking: SHARE,
        }
    }
}

/// Prepares the job of lease 7.8 for `case`, under the Linux put root `/mnt/ts/put`.
async fn prepared(case: Case) -> Result<Arc<super::Prepared>, Settled> {
    let mut harness = Harness::new().await;
    if case.put_dir {
        harness.cfg.task_out = TaskOut::PutDir(PutDirRoots::new("/mnt/ts/put").expect("roots"));
    }
    harness.cfg.mac = case.mac;
    let deps = Deps {
        inputs: Arc::new(Mac {
            os_version: case.os_version,
            tree: case.tree,
            linux: case.linux,
        }),
        ..harness.deps.clone()
    };
    let ctx = Ctx {
        cfg: &harness.cfg,
        deps: &deps,
        twin: &harness.twin,
        share: None,
    };
    prepare(
        &ctx.preparer(),
        case.on,
        LeaseId::new(7, 8),
        &action(),
        case.booking,
    )
    .await
}

fn host_path(mount: &js_proto::v1::Mount) -> &str {
    match &mount.source {
        Some(mount::Source::HostPath(path)) => path,
        other => panic!("not a host path: {other:?}"),
    }
}

/// Asserts the job of a Mac action: the release's task bundle as its image, its entrypoint
/// run (no command), no layer, the Mac flags, the tree read-only at `/work/.kbf-tree` and the run's put directory read-write at
/// `/work/.kbf-out` under the Linux roots, `os=macos`, `arch=arm64` in the native runtime, the
/// share booked, no network, no file, the wall limit and the labels of any job; and that the
/// key kept for the ingest is the directory. Catches a Mac action sent as a Linux container job.
#[tokio::test(start_paused = true)]
async fn a_mac_action_is_a_native_job_with_the_tree_and_the_put_directory_under_work() {
    let prepared = prepared(Case {
        os_version: Some(26),
        ..Case::default()
    })
    .await
    .expect("prepared");
    let spec: &JobSpec = &prepared.spec;
    let dir = "cas/b/7-8/aa-0";
    assert_eq!(prepared.key.as_str(), dir);
    assert_eq!(spec.job_id, job_id("front", LeaseId::new(7, 8), 0));
    assert_eq!(spec.image, BUNDLE);
    assert!(spec.layers.is_empty());
    assert_eq!(spec.command, "");
    assert_eq!(
        spec.args,
        mac_argv(&action(), LeaseId::new(7, 8), Duration::from_secs(20))
    );
    let targets: Vec<(&str, &str, MountMode)> = spec
        .mounts
        .iter()
        .map(|m| (host_path(m), m.target.as_str(), m.mode()))
        .collect();
    let tree = format!("/mnt/ts/trees/{TREE}");
    let put = format!("/mnt/ts/put/{dir}");
    assert_eq!(
        targets,
        [
            (tree.as_str(), TREE_TARGET, MountMode::ReadOnly),
            (put.as_str(), MAC_OUT_TARGET, MountMode::ReadWrite),
        ]
    );
    let constraints = spec.constraints.as_ref().expect("constraints");
    assert_eq!(
        (constraints.os.as_str(), constraints.arch.as_str()),
        ("macos", "arm64")
    );
    assert_eq!(constraints.exec(), ExecMode::SandboxExec);
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.files.is_empty() && spec.ports.is_empty() && spec.host_ports.is_empty());
    assert!(spec.env.is_empty());
    assert_eq!(spec.wall_limit_seconds, Some(20 + 300));
    assert_eq!(spec.labels[FRONTEND_LABEL], "front");
    assert!(spec.labels.contains_key(INSTANCE_LABEL));
    let needs = spec.needs.as_ref().expect("needs");
    assert_eq!((needs.cpu_millis, needs.whole_host), (1_500, false));
}

/// Asserts that a pool that names its own roots mounts the tree and the put directory under
/// them, not under `--task-out-root`'s.
#[tokio::test(start_paused = true)]
async fn a_pool_with_its_own_roots_mounts_under_them() {
    let prepared = prepared(Case {
        mac: Some(pool(Some("/Volumes/ts/put"))),
        ..Case::default()
    })
    .await
    .expect("prepared");
    let paths: Vec<&str> = prepared.spec.mounts.iter().map(host_path).collect();
    assert_eq!(
        paths,
        [
            format!("/Volumes/ts/trees/{TREE}"),
            "/Volumes/ts/put/cas/b/7-8/aa-0".to_owned()
        ]
    );
}

/// Asserts each refusal of a Mac run as the caller's, before anything is submitted: an action
/// that names no macOS on a Mac worker (`--default-platform none`), a Mac action on a Linux
/// worker, an executor with no Macs, an action that asks for a newer macOS than they run, a
/// `whole_machine` lease, and outputs that do not go into a put directory.
#[tokio::test(start_paused = true)]
async fn a_mac_run_the_executor_cannot_make_is_the_callers() {
    let cases = [
        (
            Case {
                linux: true,
                ..Case::default()
            },
            "the farm placed the action on a Mac worker, and its platform asks for no macOS: \
             name the system (OSFamily)",
        ),
        (
            Case {
                on: WorkerOs::Linux,
                ..Case::default()
            },
            "the farm placed the action on a Linux worker, and its platform asks for macOS",
        ),
        (
            Case {
                mac: None,
                ..Case::default()
            },
            "the action asks for a Mac, and this executor has none (--executor-mac-hosts)",
        ),
        (
            Case {
                os_version: Some(27),
                ..Case::default()
            },
            "the action asks for macOS 27 or newer (os_version), and this executor's Macs are \
             sure to run only macOS 26 or newer (--executor-mac-os-version)",
        ),
        (
            Case {
                booking: Booking::WholeHost,
                ..Case::default()
            },
            "a whole_machine action cannot run on a Mac of the job service: it commits into a \
             put directory, and a whole-host job has no mount",
        ),
        (
            Case {
                put_dir: false,
                ..Case::default()
            },
            "a Mac action commits into a put directory: give --task-out-root",
        ),
    ];
    for (case, why) in cases {
        let refused = prepared(case).await.err().expect("refused");
        assert_eq!(refused, Settled::Invalid(why.to_owned()));
    }
}

/// Asserts that a Mac run whose tree was not written is infrastructure, as a Linux one is.
#[tokio::test(start_paused = true)]
async fn a_mac_run_with_no_tree_is_infrastructure() {
    let refused = prepared(Case {
        tree: false,
        ..Case::default()
    })
    .await
    .err()
    .expect("refused");
    assert_eq!(
        refused,
        Settled::Infra("no input tree was written for 7.8".to_owned())
    );
}

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";
