//! A run that commits into a put directory (`--task-out-mode put-dir`), through the executor
//! and the job service: what the service is asked to run, and what the frontend is asked to
//! read once it ran.

use js_proto::v1::{Layer, Mount, MountMode, MountPropagation, NetworkLevel, layer, mount};
use kbf_front::Finished;
use kbf_server::executor::{PutDirRoots, TASK_IN_TREE, TaskOut};

use crate::rig::{Rig, TREE};

const HOST: &str = "host-1";

/// Asserts the job the service receives for a put-directory run, over the client and the fake
/// service: no image; one layer, the action tree under the trees beside the put root; the base
/// tree's task tool run with `--tree --stage-and-commit --out-dir /out`; the run's directory
/// mounted read-write, `no_exec` and private at `/out`; no network, no file, no host port, and
/// no url, grant or token anywhere in it (no write was presigned). Then, once the job ends
/// clean, the frontend is asked to read exactly that directory, and the caller gets the result.
#[tokio::test(start_paused = true)]
async fn a_put_dir_run_is_submitted_as_the_tree_and_its_dir_and_read_back() {
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.task_out = TaskOut::PutDir(roots)).await;
    rig.pump().await;
    let caller = rig.submit(5);
    rig.pump().await;

    let jobs = rig.jobs_of(5);
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    let spec = &job.spec;
    let dir = format!(
        "cas/b/{}/{:032x}-0",
        job.arg("--lease").expect("a lease"),
        0
    );
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert_eq!(
        spec.layers,
        vec![Layer {
            source: Some(layer::Source::HostPath(format!("/mnt/ts/trees/{TREE}"))),
        }]
    );
    assert_eq!(
        spec.mounts,
        vec![Mount {
            source: Some(mount::Source::HostPath(format!("/mnt/ts/put/{dir}"))),
            target: "/out".to_owned(),
            mode: MountMode::ReadWrite.into(),
            sub_path: String::new(),
            no_exec: true,
            propagation: MountPropagation::Private.into(),
        }]
    );
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.files.is_empty() && spec.host_ports.is_empty() && spec.ports.is_empty());
    for flag in ["--tree", "--stage-and-commit"] {
        assert!(
            spec.args.iter().any(|arg| arg == flag),
            "{flag}: {:?}",
            spec.args
        );
    }
    assert_eq!(job.arg("--out-dir"), Some("/out"));
    let words = format!("{:?} {:?}", spec.args, spec.env);
    for secret in ["read-token-0001", "grant.test", "store.test", "sig="] {
        assert!(!words.contains(secret), "{secret} in {words}");
    }
    assert!(rig.presign.keys().is_empty(), "no write was presigned");

    rig.host_finishes(job.id(), HOST, 0);
    match rig.until_done(&caller, 4).await {
        Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
    let read: Vec<String> = rig
        .ingest
        .calls
        .lock()
        .expect("calls")
        .iter()
        .map(|c| c.1.clone())
        .collect();
    assert_eq!(read, vec![dir]);
}
