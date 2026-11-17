//! An action that asks for a Mac, through the farm, the executor and the job service: which
//! worker the farm places it on, and what the service is asked to run.

use js_proto::v1::{ExecMode, MountMode, mount};
use kbf_server::executor::{
    DEFAULT_MAC_OS_VERSION, MAC_OUT_TARGET, MacPool, PutDirRoots, System, TREE_TARGET, TaskOut,
};
use kbf_types::LeaseKind;

use kbf_job_client::fake::Machine;

use crate::rig::{Rig, TREE, machine};

const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

fn mac_needs() -> kbf_caps::Request {
    kbf_caps::Request::from_platform([("OSFamily", "macos")]).expect("a Mac's platform")
}

/// A rig of one Linux worker, `macs` Mac workers and the put root `/mnt/ts/put`, whose
/// actions' platform asks for a Mac.
async fn rig(macs: usize) -> Rig {
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let rig = Rig::start(&["host-1"], 1, |cfg| {
        cfg.task_out = TaskOut::PutDir(roots);
        cfg.mac = (macs > 0).then(|| MacPool {
            hosts: macs,
            bundle: BUNDLE.to_owned(),
            os_version: DEFAULT_MAC_OS_VERSION,
            roots: None,
        });
    })
    .await;
    // A Mac of the job service, as its agent reports one: macOS on arm64, in the native runtime.
    rig.fake.add_host(
        "mac-1",
        Machine {
            os: "macos".to_owned(),
            exec: ExecMode::SandboxExec,
            ..machine(64, "arm64")
        },
    );
    *rig.inputs.facts.lock().expect("facts") = Ok(kbf_server::executor::ActionFacts {
        image: crate::rig::IMAGE.to_owned(),
        timeout: Some(std::time::Duration::from_secs(60)),
        system: System::Macos { os_version: None },
        size_class: crate::rig::built_class(),
    });
    rig
}

/// Asserts that a pool of Macs registers its workers beside the Linux ones, each a node of
/// `os=macos` and `arch=arm64`; and that a Mac action is placed on a Mac worker and submitted
/// as a Mac job: the release's task bundle as its image, no command or layer, the tree and the put directory under
/// `/work`, `os=macos`, `arch=arm64`, in the native runtime. Catches a Mac action that becomes
/// a Linux container job, or a Mac worker the farm takes for a Linux one.
#[tokio::test(start_paused = true)]
async fn a_mac_action_is_placed_on_a_mac_worker_and_submitted_as_a_native_mac_job() {
    let mut rig = rig(1).await;
    rig.pump().await;
    let workers: Vec<String> = rig
        .exec
        .workers()
        .iter()
        .map(|w| w.worker.to_string())
        .collect();
    assert_eq!(workers, ["js:front-1-0", "js-mac:front-1-0"]);
    let mut nodes: Vec<(String, String)> = rig
        .farm
        .update_look()
        .iter()
        .map(|node| (node.os.clone(), node.arch.to_string()))
        .collect();
    nodes.sort();
    assert_eq!(
        nodes,
        [
            ("linux".to_owned(), "x86_64".to_owned()),
            ("macos".to_owned(), "arm64".to_owned())
        ]
    );

    let caller = rig.submit_needing(5, LeaseKind::Action, mac_needs());
    rig.pump().await;
    let jobs = rig.jobs_of(5);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let spec = &jobs[0].spec;
    assert_eq!(spec.image, BUNDLE);
    assert!(spec.layers.is_empty());
    assert_eq!(spec.command, "");
    assert_eq!(
        spec.args.first().map(String::as_str),
        Some("--materialize-work=.kbf-tree")
    );
    assert_eq!(jobs[0].arg("--out-dir"), Some(".kbf-out"));
    let mounts: Vec<(String, &str, MountMode)> = spec
        .mounts
        .iter()
        .map(|m| match &m.source {
            Some(mount::Source::HostPath(path)) => (path.clone(), m.target.as_str(), m.mode()),
            other => panic!("not a host path: {other:?}"),
        })
        .collect();
    assert_eq!(
        mounts[0],
        (
            format!("/mnt/ts/trees/{TREE}"),
            TREE_TARGET,
            MountMode::ReadOnly
        )
    );
    assert_eq!(
        (mounts[1].1, mounts[1].2),
        (MAC_OUT_TARGET, MountMode::ReadWrite)
    );
    assert!(mounts[1].0.starts_with("/mnt/ts/put/cas/b/"), "{mounts:?}");
    let constraints = spec.constraints.as_ref().expect("constraints");
    assert_eq!(
        (constraints.os.as_str(), constraints.arch.as_str()),
        ("macos", "arm64")
    );
    assert_eq!(constraints.exec(), ExecMode::SandboxExec);

    // The Mac runs it, and the caller gets its result as from any job.
    rig.host_finishes(jobs[0].id(), "mac-1", 0);
    match rig.until_done(&caller, 4).await {
        kbf_front::Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
}

/// Asserts that an executor with no Macs, the default, registers no Mac worker and takes no
/// Mac action: the farm keeps it queued, as it did before there were Mac workers, and the job
/// service is asked to run nothing for it.
#[tokio::test(start_paused = true)]
async fn with_no_macs_a_mac_action_is_not_placed_on_the_executor() {
    let mut rig = rig(0).await;
    rig.pump().await;
    let workers: Vec<String> = rig
        .exec
        .workers()
        .iter()
        .map(|w| w.worker.to_string())
        .collect();
    assert_eq!(workers, ["js:front-1-0"]);
    let caller = rig.submit_needing(6, LeaseKind::Action, mac_needs());
    rig.pump().await;
    assert!(rig.jobs_of(6).is_empty());
    assert!(rig.jobs().is_empty());
    drop(caller);
}
