//! Outputs through the native driver follow the Linux rules: no symlink followed at
//! any level, and the limits fail the action.

mod support;

use std::sync::Arc;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_proto::reapi::Digest;
use support::{MemoryCas, Spec, config, no_leases, run, runtime, scratch};

/// Catches: an output read through a symlink the action planted (the working
/// directory replaced by a link to a host directory, a link on an output's path, a
/// link inside an output directory), which would upload host files the action could
/// not otherwise reach.
#[tokio::test]
async fn no_symlink_the_action_plants_is_followed() {
    let dir = scratch("symlinks");
    let host = dir.join("host");
    std::fs::create_dir_all(host.join("sub")).expect("mkdir");
    std::fs::write(host.join("secret"), b"host secret").expect("write");
    std::fs::write(host.join("sub/secret"), b"host secret").expect("write");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let host_text = host.to_str().expect("utf8");

    // `via` is made by the driver, as the parent of an output: the action moves it
    // aside and puts the link in its place.
    let steps = format!(
        "mkdir out ; symlink {host_text} out/link ; rename via via-made ; \
         symlink {host_text} via ; symlink {host_text}/secret file"
    );
    let spec = Spec::act(&steps).outputs(&["out", "via/secret", "file"]);
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    assert!(result.output_files.is_empty(), "{result:?}");
    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(
        result.output_symlinks[0].target,
        format!("{host_text}/secret")
    );
    let digest = result.output_directories[0]
        .tree_digest
        .as_ref()
        .expect("tree");
    let root = support::tree(&cas, digest).root.expect("root");
    assert!(
        root.files.is_empty() && root.directories.is_empty(),
        "{root:?}"
    );
    assert_eq!(root.symlinks[0].name, "link");

    // The working directory itself replaced by a link to the host directory.
    let steps = format!("rename ../w ../moved ; symlink {host_text} ../w");
    let mut spec = Spec::act(&steps).outputs(&["secret", "sub"]);
    spec.working_directory = "w".to_owned();
    let result = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert!(result.output_directories.is_empty(), "{result:?}");
    assert!(no_leases(&config));
}

/// Catches: the output limits not applied by the native driver, or an output past
/// them reported as the client's error rather than a failed lease.
#[tokio::test]
async fn outputs_past_a_limit_fail_the_lease() {
    let dir = scratch("limits");
    let mut config = config(&dir);
    config.outputs.max_bytes = 10;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::act("append f eleven bytes").outputs(&["f"]);
    let outcome = run(&rt, &cas, 1, &spec).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("--output-max-bytes")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// Catches: stdout past its limit stored anyway (a runaway log would fill the CAS),
/// or its limit not named; and an upload failure of the captured output swallowed.
#[tokio::test]
async fn stdout_past_its_limit_and_a_failed_upload_fail_the_lease() {
    let dir = scratch("stdio");
    let mut config = config(&dir);
    config.outputs.max_stdio_bytes = 4;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &Spec::act("echo too long")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("--output-max-stdio-bytes")),
        "{outcome:?}"
    );
    let refusing = Arc::new(MemoryCas {
        refuse_puts: true,
        ..MemoryCas::default()
    });
    let action = Spec::act("echo hi").store(&refusing);
    let rt = runtime(support::config(&dir), &refusing);
    let outcome = rt.run(support::work(2, action, 0)).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("the CAS is down")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// Catches an output file larger than a chunk held whole or stored with the wrong
/// bytes: it goes to the CAS in chunks and comes back whole.
#[tokio::test]
async fn a_large_output_file_is_stored_whole() {
    let dir = scratch("large");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    // The action writes its input `in`, upper-cased, to `big`.
    let mut spec = Spec::act("upper big in").outputs(&["big"]);
    spec.inputs = vec![("in".to_owned(), vec![b'a'; 2_500_000])];
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    let digest = result.output_files[0].digest.as_ref().expect("digest");
    assert_eq!(*digest, kbf_daemon::cas::digest_of(&vec![b'A'; 2_500_000]));
    assert_eq!(cas.blob(digest).len(), 2_500_000);
}

/// Every non-empty blob `result` names, as the farm's action cache counts them before
/// it accepts a result (the empty blob is always held): each output file, stdout and
/// stderr, each output directory's Tree and root Directory, and every file and
/// Directory the Tree names.
fn named_blobs(cas: &MemoryCas, result: &kbf_proto::reapi::ActionResult) -> Vec<Digest> {
    let mut named: Vec<Digest> = result
        .output_files
        .iter()
        .filter_map(|f| f.digest.clone())
        .chain(result.stdout_digest.clone())
        .chain(result.stderr_digest.clone())
        .collect();
    for dir in &result.output_directories {
        let tree_digest = dir.tree_digest.clone().expect("tree digest");
        named.extend(dir.root_directory_digest.clone());
        let tree = support::tree(cas, &tree_digest);
        named.push(tree_digest);
        for directory in tree.root.iter().chain(&tree.children) {
            named.extend(directory.files.iter().filter_map(|f| f.digest.clone()));
            named.extend(
                directory
                    .directories
                    .iter()
                    .filter_map(|d| d.digest.clone()),
            );
        }
    }
    named.retain(|d| d.size_bytes > 0);
    named
}

/// Catches a directory output whose root Directory, or a Directory below it, is not
/// in the CAS when the run returns its result: the farm refuses such a result ("an OK
/// result whose outputs are not stored") and the client's action fails. The tree has
/// nested files and an empty directory, so the root, a nested and an empty Directory
/// are all named.
#[tokio::test]
async fn every_blob_a_directory_output_names_is_stored() {
    let dir = scratch("tree-blobs");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let steps = "mkdir out/a/b ; mkdir out/empty ; append out/top top ; \
                 append out/a/mid mid ; append out/a/b/leaf leaf";
    let result = run(&rt, &cas, 1, &Spec::act(steps).outputs(&["out"]))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    let tree_digest = result.output_directories[0]
        .tree_digest
        .as_ref()
        .expect("tree");
    let tree = support::tree(&cas, tree_digest);
    assert_eq!(tree.children.len(), 3, "a, a/b and empty: {tree:?}");
    let named = named_blobs(&cas, &result);
    // root, a, a/b (empty is the empty blob), the Tree, three files, stdout or stderr.
    assert!(named.len() >= 7, "{named:?}");
    let held = cas.blobs.lock().expect("lock");
    let absent: Vec<String> = named
        .iter()
        .map(kbf_daemon::cas::label)
        .filter(|blob| !held.contains_key(blob))
        .collect();
    assert!(absent.is_empty(), "named but not stored: {absent:?}");
}
