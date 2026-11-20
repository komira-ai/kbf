//! An action that asks for a Mac, through the executor and the job service: what the service is
//! asked to run, what an executor with no Macs answers, and what a Mac action that names no Xcode,
//! or one no Mac has, is answered.

use js_proto::v1::{ExecMode, MountMode, mount};
use kbf_front::Finished;
use kbf_job_client::fake::Machine;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_server::executor::{
    ActionFacts, DEFAULT_MAC_OS_VERSION, MAC_OUT_TARGET, MacPool, NO_XCODE_REASON, PutDirRoots,
    System, TREE_TARGET, XcodeBuild,
};
use prost::Message;
use tonic::Code;

use crate::rig::{HOST, IMAGE, Rig, TREE, built_class, machine};

const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// The one Xcode the Mac host has.
const XCODE: &str = "16C5032a";

/// A rig of one Linux host, a Mac host with the Xcode [`XCODE`] and the put root `/mnt/ts/put`,
/// whose actions' platform asks for a Mac with the Xcode `xcode`; with `macs`, its executor takes
/// Mac actions.
async fn rig(macs: bool, xcode: Option<&str>) -> Rig {
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
    // with one Xcode installed.
    rig.fake.add_host(
        "mac-1",
        Machine {
            os: "macos".to_owned(),
            exec: ExecMode::SandboxExec,
            xcodes: vec![XCODE.to_owned()],
            ..machine(64, "arm64")
        },
    );
    *rig.inputs.facts.lock().expect("facts") = Ok(ActionFacts {
        image: IMAGE.to_owned(),
        timeout: Some(std::time::Duration::from_secs(60)),
        system: System::Macos {
            os_version: None,
            xcode: xcode.map(|build| XcodeBuild::parse(build).expect("a build")),
        },
        size_class: built_class(),
    });
    rig
}

/// Asserts that a Mac action is submitted as a Mac job: the release's task bundle as its
/// image, no command or layer, the tree and the put directory under `/work`, `os=macos`,
/// `arch=arm64`, in the native runtime, asking for the Xcode build the action names; and that the
/// job service places it on the Mac that has that Xcode. Catches a Mac action that becomes a
/// Linux container job, and one placed whatever Xcodes its Mac has.
#[tokio::test]
async fn a_mac_action_is_submitted_as_a_native_mac_job() {
    let rig = rig(true, Some(XCODE)).await;
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
    assert_eq!(constraints.xcode, XCODE);

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
    let rig = rig(false, Some(XCODE)).await;
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

/// Asserts that a Mac action that names no Xcode is refused at once, INVALID_ARGUMENT with the
/// reason `MAC_ACTION_NAMES_NO_XCODE` in kbf's domain and a message that names the property, and
/// that the job service is asked to run nothing for it, though a Mac could take it. Catches a Mac
/// action run with whichever Xcode the Mac has selected.
#[tokio::test]
async fn a_mac_action_that_names_no_xcode_is_refused_with_its_reason_and_never_placed() {
    let rig = rig(true, None).await;
    let mut caller = rig.submit(7);
    let Finished::Failed(status) = caller.done().await else {
        panic!("not refused");
    };
    assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
    assert!(
        status.message.starts_with(
            "a Mac action must name the Xcode it runs with: give the platform property \
             xcode=<build>"
        ),
        "{}",
        status.message
    );
    let [detail] = status.details.as_slice() else {
        panic!("one detail: {:?}", status.details);
    };
    let info = ErrorInfo::decode(detail.value.as_slice()).expect("an ErrorInfo");
    assert_eq!(
        (info.reason.as_str(), info.domain.as_str()),
        (NO_XCODE_REASON, kbf_front::ERROR_DOMAIN)
    );
    assert!(rig.jobs().is_empty());
}

/// Asserts that a Mac action that names an Xcode no Mac has is not placed: the job service
/// refuses its job as one no host can run, and the caller is answered INVALID_ARGUMENT at once.
/// Catches a job whose Xcode the service does not see, which any Mac would take.
#[tokio::test]
async fn a_mac_action_that_names_an_xcode_no_mac_has_is_never_placed() {
    let rig = rig(true, Some("17A324")).await;
    let mut caller = rig.submit(8);
    let Finished::Failed(status) = caller.done().await else {
        panic!("not refused");
    };
    assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
    assert!(rig.jobs().is_empty());
}
