//! An action that asks for a Mac, through the executor and the job service: what the service is
//! asked to run, and what an executor with no Macs answers.

use js_proto::v1::{ExecMode, MountMode, mount};
use kbf_front::Finished;
use kbf_job_client::fake::Machine;
use kbf_server::executor::{
    ActionFacts, DEFAULT_MAC_OS_VERSION, MAC_OUT_TARGET, MacPool, PutDirRoots, System, TREE_TARGET,
};
use tonic::Code;

use crate::rig::{HOST, IMAGE, Rig, TREE, built_class, machine};

const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// A rig of one Linux host, a Mac host and the put root `/mnt/ts/put`, whose actions' platform
/// asks for a Mac; with `macs`, its executor takes Mac actions.
async fn rig(macs: bool) -> Rig {
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let rig = Rig::start(&[HOST], |cfg| {
        cfg.task_out = Some(roots);
        cfg.mac = macs.then(|| MacPool {
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
    *rig.inputs.facts.lock().expect("facts") = Ok(ActionFacts {
        image: IMAGE.to_owned(),
        timeout: Some(std::time::Duration::from_secs(60)),
        system: System::Macos { os_version: None },
        size_class: built_class(),
    });
    rig
}

/// Asserts that a Mac action is submitted as a Mac job: the release's task bundle as its
/// image, no command or layer, the tree and the put directory under `/work`, `os=macos`,
/// `arch=arm64`, in the native runtime. Catches a Mac action that becomes a Linux container
/// job.
#[tokio::test]
async fn a_mac_action_is_submitted_as_a_native_mac_job() {
    let rig = rig(true).await;
    let mut caller = rig.submit(5);
    let jobs = rig.until_jobs(5, 1).await;
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
    match caller.done().await {
        Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
}

/// Asserts that an executor with no Macs, the default, takes no Mac action: the caller is
/// answered INVALID_ARGUMENT at once, which no retry would change, and the job service is asked
/// to run nothing for it.
#[tokio::test]
async fn with_no_macs_a_mac_action_is_refused_and_nothing_is_submitted() {
    let rig = rig(false).await;
    let mut caller = rig.submit(6);
    match caller.done().await {
        Finished::Failed(status) => {
            assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
            assert!(status.message.contains("has none"), "{}", status.message);
        }
        other => panic!("not refused: {other:?}"),
    }
    assert!(rig.jobs().is_empty());
}
