//! An action that asks for a Mac, through the executor and the job service: what the service is
//! asked to run, where it places it, and what an executor with no Macs, or a job service with
//! none, answers.

use js_proto::v1::{Constraints, ExecMode, MountMode, ProviderSource, mount};
use kbf_front::Finished;
use kbf_job_client::fake::Machine;
use kbf_server::executor::{
    ActionFacts, DEFAULT_MAC_OS_VERSION, InputTree, MAC_OUT_TARGET, MacPool, PutDirRoots, System,
    TREE_TARGET,
};
use tonic::Code;

use crate::rig::{HOST, IMAGE, Rig, TREE, built_class, machine};

const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// A rig of one Linux host and, with `mac_host`, a Mac host with no Xcode, and the put root
/// `/mnt/ts/put`, whose actions' platform asks for a Mac; with `macs`, its executor takes Mac
/// actions.
async fn rig(macs: bool, mac_host: bool) -> Rig {
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
    // A Mac of the job service, as its agent reports one: macOS on arm64, in the native runtime,
    // with only the Command Line Tools (no Xcode).
    if mac_host {
        rig.fake.add_host(
            "mac-1",
            Machine {
                os: "macos".to_owned(),
                exec: ExecMode::SandboxExec,
                xcodes: Vec::new(),
                ..machine(64, "arm64")
            },
        );
    }
    *rig.inputs.facts.lock().expect("facts") = Ok(ActionFacts {
        image: IMAGE.to_owned(),
        timeout: Some(std::time::Duration::from_secs(60)),
        system: System::Macos { os_version: None },
        size_class: built_class(),
    });
    rig
}

/// Asserts that a Mac action is submitted as a Mac job: the release's task bundle as its
/// image, no command or layer, the tree (from tierstore's provider, the mint asked for the tree
/// list) and the put target under `/work`, constraints of exactly `os=macos` and `arch=arm64`
/// (no `exec`, no Xcode); and that the job service places it on the Mac, which has no Xcode, and
/// not on the Linux host. Catches a Mac action that becomes a Linux container job, and one that
/// still asks for an Xcode or the `sandbox-exec` runtime.
#[tokio::test]
async fn a_mac_action_is_submitted_as_a_mac_job_of_os_and_arch_only() {
    let rig = rig(true, true).await;
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
    let [tree, put] = spec.mounts.as_slice() else {
        panic!("two mounts: {:?}", spec.mounts);
    };
    assert_eq!(
        tree.source,
        Some(mount::Source::Provider(ProviderSource {
            provider: "tierstore".to_owned(),
            r#ref: TREE.to_owned(),
        }))
    );
    assert_eq!(
        (tree.target.as_str(), tree.mode()),
        (TREE_TARGET, MountMode::ReadOnly)
    );
    assert_eq!(
        (put.target.as_str(), put.mode()),
        (MAC_OUT_TARGET, MountMode::ReadWrite)
    );
    let Some(mount::Source::HostPath(dir)) = &put.source else {
        panic!("the put target is not a host path: {put:?}");
    };
    assert!(dir.starts_with("/mnt/ts/put/cas/b/"), "{dir}");
    assert!(tree.no_exec && put.no_exec);
    assert_eq!(
        *rig.ingest.forms.lock().expect("forms"),
        [InputTree::Provider]
    );
    assert_eq!(
        spec.constraints,
        Some(Constraints {
            os: "macos".to_owned(),
            arch: "arm64".to_owned(),
            ..Constraints::default()
        })
    );

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
    let rig = rig(false, true).await;
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

/// Asserts that a Mac action, when the job service has no Mac, is not placed on its Linux host:
/// the service refuses the job as one no host can run, and the caller is answered
/// INVALID_ARGUMENT at once. Catches a Mac job that does not ask for `os=macos`, which any host
/// would take.
#[tokio::test]
async fn a_mac_action_with_no_mac_in_the_job_service_is_never_placed() {
    let rig = rig(true, false).await;
    let mut caller = rig.submit(8);
    let Finished::Failed(status) = caller.done().await else {
        panic!("not refused");
    };
    assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
    assert!(rig.jobs().is_empty());
}
