//! A run that commits into a put directory (`--task-out-root`), through the executor
//! and the job service: what the service is asked to run, and what the frontend is asked to
//! read once it ran.

use js_proto::v1::{
    Layer, Mount, MountMode, MountPropagation, NetworkLevel, ProviderSource, layer, mount,
};
use kbf_front::Finished;
use kbf_server::executor::{InputTree, TASK_IN_TREE};

use crate::rig::{Rig, TREE};

const HOST: &str = "host-1";

/// Asserts the job the service receives for a put-directory run, over the client and the fake
/// service: no image; one layer, the action tree under the trees beside the put root; the base
/// tree's task tool run with `--tree --stage-and-commit --out-dir /out`; the run's directory
/// mounted read-write, `no_exec` and private at `/out`; no network, no file, no host port, and
/// no url, grant or token anywhere in it. Then, once the job ends
/// clean, the frontend is asked to read exactly that directory, and the caller gets the result.
#[tokio::test(start_paused = true)]
async fn a_put_dir_run_is_submitted_as_the_tree_and_its_dir_and_read_back() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
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
    for secret in ["token", "grant", "http", "sig="] {
        assert!(!words.contains(secret), "{secret} in {words}");
    }

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

/// Asserts that a frontend with no storage node, whose run committed into a put directory and
/// was accepted, has the action's result in its action cache: a client that asks for the action
/// afterwards is answered the result the callers got, with the exit code of the run. (The farm
/// wrote the accepted result's record through a plane that refused every record it was handed.)
#[tokio::test(start_paused = true)]
async fn an_accepted_put_dir_run_is_cached_by_a_frontend_with_no_storage_node() {
    let stored = Rig::no_storage_node();
    let cache = kbf_front::Backend::stored(std::sync::Arc::clone(&stored));
    let mut rig = Rig::over(&[HOST], 1, 16, cache, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(5);
    rig.pump().await;
    let job = rig.jobs_of(5).remove(0);

    rig.host_finishes(job.id(), HOST, 0);
    let Finished::Ran(ran) = rig.until_done(&caller, 4).await else {
        panic!("the run did not finish ok");
    };
    rig.pump().await;

    let cached = stored.action_result(&crate::rig::digest_of(5)).await;
    let (_, held) = cached.expect("asked").expect("the result is cached");
    assert_eq!(held, *ran);
}

/// Asserts the job the service receives for a put-directory run with
/// `--executor-input-tree provider`, over the client and the fake service: the mint is asked
/// for a tree list, and the job's one layer is tierstore's mount provider naming the key of
/// that list, `{provider: "tierstore", ref: <key>}`, with no host path; its mount, its program
/// and its network are those of the manifests' job. Catches: a provider run submitted with the
/// manifests' host-path layer (a host the provider serves would not be chosen for it), a
/// provider other than tierstore, a ref other than the list's key, or a mint asked for
/// manifests.
#[tokio::test(start_paused = true)]
async fn a_provider_run_is_submitted_as_tierstores_provider_naming_the_tree_list() {
    let key = format!("cas/l/{}", "ab".repeat(32));
    let mut rig = Rig::start(&[HOST], 1, |cfg| cfg.input_tree = InputTree::Provider).await;
    *rig.inputs.tree.lock().expect("tree") = Ok(key.clone());
    rig.pump().await;
    let caller = rig.submit(5);
    rig.pump().await;

    let jobs = rig.jobs_of(5);
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    let spec = &job.spec;
    assert_eq!(
        spec.layers,
        vec![Layer {
            source: Some(layer::Source::Provider(ProviderSource {
                provider: "tierstore".to_owned(),
                r#ref: key,
            })),
        }]
    );
    assert_eq!(
        *rig.ingest.forms.lock().expect("forms"),
        vec![InputTree::Provider]
    );
    let dir = format!(
        "cas/b/{}/{:032x}-0",
        job.arg("--lease").expect("a lease"),
        0
    );
    assert_eq!(
        spec.mounts
            .iter()
            .map(|m| m.source.clone())
            .collect::<Vec<_>>(),
        vec![Some(mount::Source::HostPath(format!("/mnt/ts/put/{dir}")))]
    );
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert_eq!(spec.network(), NetworkLevel::None);

    rig.host_finishes(job.id(), HOST, 0);
    match rig.until_done(&caller, 4).await {
        Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
}
