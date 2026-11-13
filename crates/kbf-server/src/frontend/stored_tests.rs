//! A frontend started with no `--storage`: its flags are refused where they cannot run, and one
//! that starts keeps the blobs clients upload in packs and the action cache in the state store
//! (here a pack index in this process), answers `/readyz` from the state store and the job
//! service alone, and dials no storage node.

use std::error::Error;
use std::sync::Arc;

use kbf_job_client::fake::Fault;
use kbf_objstore::s3::fake::FakeS3;
use kbf_pack_index::action::ActionStore;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{self, ActionResult, GetActionResultRequest, OutputFile};
use kbf_state::ManualClock;
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::oneshot;
use tonic::Code;
use tonic::transport::Endpoint;

use super::executor_tests::{flag_list, flags_with, job_server, source, state_store, try_get};
use super::pack_tests::{argv_over, upload};
use super::stored::stored_checks;
use super::{FrontendFlags, FrontendHandle, Prepared, boxed, command, parse};
use crate::health::Readiness;
use crate::test_dir::scratch_dir;

const JS_AGENT: &[&str] = &["--executor=js-agent", "--executor-insecure-argv-secrets"];

/// The error of parsing the command line `argv`, as text.
fn refused(argv: Vec<String>) -> String {
    match command().try_get_matches_from(argv) {
        Err(e) => e.to_string(),
        Ok(m) => parse(&m).expect_err("refused").to_string(),
    }
}

/// Starts the frontend `flags` describe over `index`, as `bind_frontend` starts one over the
/// Postgres index, which stops when the returned sender is sent to.
fn started(
    flags: &FrontendFlags,
    index: &Arc<MemoryPackIndex>,
    state: Option<crate::StateStore>,
) -> Result<(FrontendHandle, oneshot::Sender<()>), Box<dyn Error>> {
    let (stop, stopped) = oneshot::channel::<()>();
    let shutdown = boxed(move || {
        Ok(async move {
            let _ = stopped.await;
        })
    });
    let packs: Arc<dyn PackIndex> = index.clone();
    let handle = Prepared::new(flags)?.start_over(
        state,
        Some(packs),
        Some(index.clone()),
        shutdown,
        Some(source()),
    )?;
    Ok((handle, stop))
}

fn memory() -> Arc<MemoryPackIndex> {
    Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())))
}

fn proto(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// Asserts that a frontend with the executor, the pack index and the packs and no `--storage`
/// parses with no storage node, and that every one of the three it needs, named by its flag, is
/// refused when it is left out, as is the default executor, which needs a node for its daemons.
#[tokio::test]
async fn a_frontend_with_no_storage_needs_the_executor_the_pack_index_and_the_packs() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("stored-flags");
    let fake = job_server(&["host-1"]).await;
    let full = flag_list(dir.path(), &s3, &fake, JS_AGENT);
    let parsed = parse(
        &command()
            .try_get_matches_from(argv_over(None, &full))
            .expect("flags"),
    )
    .expect("parses with no storage node");
    assert!(parsed.storage.is_none());

    let mut daemon = full.clone();
    daemon.retain(|flag| !flag.starts_with("--executor="));
    let why = refused(argv_over(None, &daemon));
    assert!(
        why.contains("no --storage needs --executor js-agent") && why.contains("kbf-daemon"),
        "{why}"
    );
    daemon.push("--executor=kbf-daemon".to_owned());
    let why = refused(argv_over(None, &daemon));
    assert!(
        why.contains("no --storage needs --executor js-agent"),
        "{why}"
    );

    let mut no_packs = full.clone();
    no_packs
        .retain(|flag| !flag.starts_with("--pack-store-") && !flag.starts_with("--pack-writer"));
    let why = refused(argv_over(None, &no_packs));
    assert!(
        why.contains("no --storage needs --pack-store-endpoint"),
        "{why}"
    );

    no_packs.retain(|flag| !flag.starts_with("--pack-"));
    let mut argv = argv_over(None, &no_packs);
    argv.retain(|flag| flag != "--pack-index");
    let why = refused(argv);
    assert!(why.contains("no --storage needs --pack-index"), "{why}");
}

/// Asserts that the three flags of the client identity toward a storage node are refused without
/// `--storage`, naming it, and that `--storage` still needs its transport rules.
#[test]
fn the_storage_tls_flags_need_a_storage_node() {
    for flag in [
        "--storage-client-cert=/c",
        "--storage-client-key=/k",
        "--storage-ca=/ca",
    ] {
        let why = refused(argv_over(None, &[flag.to_owned()]));
        assert!(why.contains("--storage"), "{flag}: {why}");
    }
}

/// Asserts the start line says there is no storage node, where it says which there is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_no_storage_serves_blobs_and_the_action_cache_with_no_node() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("stored-serves");
    let fake = job_server(&["host-1"]).await;
    let flags = flags_with(None, dir.path(), &s3, &fake, JS_AGENT);
    assert!(flags.storage.is_none());
    assert!(flags.storage_endpoint().expect("no endpoint").is_none());
    let index = memory();
    let (handle, stop) = started(&flags, &index, None).expect("starts");
    assert!(
        handle.start_line().ends_with(" storage=none"),
        "{}",
        handle.start_line()
    );
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());

    // An upload lands in a pack, and the index holds it: no node was asked.
    let blobs: [(Digest, &[u8]); 2] = [
        (kbf_segments::sha256(b"no node one"), b"no node one"),
        (kbf_segments::sha256(b"no node two"), b"no node two"),
    ];
    assert_eq!(upload(front, &blobs).await, [0, 0]);
    let keys = s3.keys();
    assert!(keys.iter().any(|k| k.starts_with("cas/f/fe9/")), "{keys:?}");

    // The action cache is the table beside the index: a row written there is what a client
    // reads, once the blobs it names are held, and an action with no row is a miss.
    let held = &blobs[0].0;
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(proto(held)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    let action = kbf_segments::sha256(b"an action");
    let key = BlobDigest {
        hash: action.hash,
        size: action.size_bytes,
    };
    index
        .put_action(&key, &result.encode_to_vec(), &[])
        .expect("a row");
    let channel = Endpoint::from_shared(format!("http://{front}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connects");
    let mut cache = ActionCacheClient::new(channel);
    let got = cache
        .get_action_result(GetActionResultRequest {
            action_digest: Some(proto(&action)),
            ..GetActionResultRequest::default()
        })
        .await
        .expect("a hit")
        .into_inner();
    assert_eq!(got, result);
    let absent = kbf_segments::sha256(b"another action");
    let miss = cache
        .get_action_result(GetActionResultRequest {
            action_digest: Some(proto(&absent)),
            ..GetActionResultRequest::default()
        })
        .await
        .expect_err("a miss");
    assert_eq!(miss.code(), Code::NotFound);

    let text = try_get(api, "/readyz").await.expect("answered");
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    stop.send(()).expect("stopped");
    served.await.expect("joined").expect("served");
}

/// Asserts `/readyz` of a frontend with no storage node asks the state store and the job
/// service: it passes with both, and fails naming `executor` when the job service answers a read
/// with a status that says it will not serve the frontend, and again passes once it answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_of_a_frontend_with_no_storage_follows_the_state_store_and_the_job_service() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("stored-ready");
    let fake = job_server(&["host-1"]).await;
    let flags = flags_with(None, dir.path(), &s3, &fake, JS_AGENT);
    let (_, state) = state_store();
    let (handle, stop) = started(&flags, &memory(), Some(state)).expect("starts");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());

    let ready = try_get(api, "/readyz").await.expect("answered");
    assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");

    fake.inject_on(
        "GetJob",
        Fault::Status {
            code: Code::Unavailable,
            message: "down".to_owned(),
        },
    );
    let down = try_get(api, "/readyz").await.expect("answered");
    assert!(down.starts_with("HTTP/1.1 503"), "{down}");
    assert!(down.contains("executor"), "{down}");

    let again = try_get(api, "/readyz").await.expect("answered");
    assert!(again.starts_with("HTTP/1.1 200"), "{again}");
    stop.send(()).expect("stopped");
    served.await.expect("joined").expect("served");
}

/// Asserts the checks of a frontend with no storage node are the stop signal and role, the state
/// store when there is one and the executor when there is one, and nothing else: none with
/// neither passes, and the stop signal fails them whichever there are.
#[tokio::test]
async fn the_checks_with_no_storage_node_are_the_state_store_and_the_executor_alone() {
    let readiness = Arc::new(Readiness::default());
    let timeout = std::time::Duration::from_secs(5);
    let none = stored_checks(Arc::clone(&readiness), None, None, timeout);
    assert!(none.failing().await.is_empty());
    let (_, state) = state_store();
    let with_state = stored_checks(Arc::clone(&readiness), Some(state.store), None, timeout);
    assert!(with_state.failing().await.is_empty());
    readiness.stop();
    let failing = none.failing().await;
    assert_eq!(failing.len(), 1);
}

/// Asserts the plane of a frontend is a node when there is a connection to one and the stored
/// plane when there is none, and that the stored plane is refused, naming the flag, without the
/// packs or without the action cache, whichever of the two is left out.
#[tokio::test]
async fn the_plane_is_a_node_with_a_connection_and_needs_the_packs_and_the_action_cache_without() {
    use super::stored::Plane;

    let s3 = FakeS3::start().await;
    let dir = scratch_dir("stored-plane");
    let fake = job_server(&["host-1"]).await;
    let flags = flags_with(None, dir.path(), &s3, &fake, JS_AGENT);
    let index = memory();
    let (handle, _stop) = started(&flags, &index, None).expect("starts");
    let packs = handle.packs.clone().expect("the packs");
    let actions: Arc<dyn ActionStore> = index.clone();

    let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    let node = Plane::of(Some(channel), None, None).expect("a node needs neither");
    assert!(matches!(node, Plane::Node { .. }));
    let stored = Plane::of(None, Some(&packs), Some(Arc::clone(&actions))).expect("stored");
    assert!(matches!(stored, Plane::Stored(_)));
    let no_packs = Plane::of(None, None, Some(Arc::clone(&actions)));
    let why = no_packs.err().expect("refused").to_string();
    assert!(why.contains("--pack-store-endpoint"), "{why}");
    let no_actions = Plane::of(None, Some(&packs), None);
    let why = no_actions.err().expect("refused").to_string();
    assert!(why.contains("--pack-index"), "{why}");
}
