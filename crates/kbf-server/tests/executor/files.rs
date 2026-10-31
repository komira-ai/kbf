//! Secrets as files of the run: what the lease server is given, what a host without files
//! gets, and the two refusals that concern the files.

use kbf_front::Finished;
use kbf_lease_client::Outcome;
use kbf_lease_client::fake::Pool;
use kbf_server::executor::{Config, Delivery};
use tonic::Code;

use crate::rig::{Rig, count_tail};

const TOKEN: &str = "read-token-0001";

fn files(cfg: &mut Config) {
    cfg.delivery = Delivery::Files;
    cfg.insecure_argv_secrets = false;
}

fn with_run_files(pool: &mut Pool) {
    pool.capabilities
        .insert("run-files".to_owned(), "true".to_owned());
}

async fn rig() -> Rig {
    Rig::pooled(&["node1"], 1, 16, with_run_files, files).await
}

/// Catches a secret that reaches a host any way but a file: the presigned url and the read
/// token are in the two files the run carries and in no argument, no label and no request line;
/// the task is told the paths; the run still ends end to end.
#[tokio::test(start_paused = true)]
async fn the_fake_host_sees_the_secret_only_in_files() {
    let mut rig = rig().await;
    rig.pump().await;
    let caller = rig.submit(3);
    rig.pump().await;
    let runs = rig.runs_of(3).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let run = &runs[0];
    let files = rig.fake.run_files(&run.run).expect("the run's files");
    let contents: Vec<&str> = files.iter().map(|f| f.contents.as_str()).collect();
    assert_eq!(contents.len(), 2, "{files:?}");
    assert!(contents.contains(&TOKEN));
    let url = contents
        .iter()
        .find(|c| c.starts_with("https://store.test/cas/b/"))
        .expect("the pack url");
    for arg in &run.args {
        assert!(!arg.contains(TOKEN), "the token is in {arg:?}");
        assert!(!arg.contains("s3cr3t"), "the signature is in {arg:?}");
        assert!(!arg.contains("store.test"), "the url is in {arg:?}");
    }
    let listed = rig.api.runs(run.lease).await.expect("runs");
    for label in listed[0].labels.values() {
        assert!(!label.contains(TOKEN) && !label.contains(url), "{label}");
    }
    let requests = rig.requests().join("\n");
    assert!(!requests.contains(TOKEN), "{requests}");
    assert!(!requests.contains("s3cr3t"), "{requests}");
    for flag in ["--pack-url", "--grant-token", "--insecure-argv-secrets"] {
        assert!(!run.args.iter().any(|a| a == flag), "{flag}");
    }
    assert!(run.arg("--pack-url-file").is_some());
    assert!(run.arg("--grant-token-file").is_some());
    rig.host_finishes(&run.run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a fall back to arguments: a pool whose hosts do not report `run-files` is never
/// leased from under `files`, and no run is posted, with or without an action waiting; no
/// request carries a secret.
#[tokio::test(start_paused = true)]
async fn files_delivery_never_falls_back_to_argv_on_a_host_without_run_files() {
    let mut rig = Rig::pooled(&["node1"], 1, 16, |_| {}, files).await;
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty(), "{:?}", rig.exec.hosts());
    let _caller = rig.submit(3);
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
    assert!(rig.runs().await.is_empty());
    let requests = rig.requests();
    assert_eq!(count_tail(&requests, "POST", "/runs"), 0, "{requests:?}");
}

/// Catches a host that lost its files root after it said hello: the post is refused with
/// `files-unsupported`, the action is the farm's to rerun (not the caller's), the worker takes
/// no more work, and nothing is queued.
#[tokio::test(start_paused = true)]
async fn a_files_unsupported_refusal_drains_the_host_and_reruns_the_action() {
    let mut rig = rig().await;
    rig.pump().await;
    assert!(rig.fake.refuse_posts("node1", Some("files-unsupported")));
    let caller = rig.submit(3);
    rig.pump().await;
    assert!(rig.exec.hosts()[0].draining);
    assert!(rig.runs().await.is_empty());
    assert!(caller.finished().is_none(), "{:?}", caller.finished());
}

/// Catches a refused file being rerun for ever or blamed on the host: `invalid-file` is a fault
/// of ours that no host cures, so the action ends `InvalidArgument`, the host stays in service,
/// and the message holds neither secret.
#[tokio::test(start_paused = true)]
async fn an_invalid_file_refusal_ends_the_action_invalid() {
    let mut rig = rig().await;
    rig.pump().await;
    assert!(rig.fake.refuse_posts("node1", Some("invalid-file")));
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    let Finished::Failed(status) = &finished else {
        panic!("not a failure: {finished:?}");
    };
    assert_eq!(
        Code::from_i32(status.code),
        Code::InvalidArgument,
        "{status:?}"
    );
    assert!(!status.message.contains(TOKEN), "{}", status.message);
    assert!(!status.message.contains("s3cr3t"), "{}", status.message);
    assert!(!rig.exec.hosts()[0].draining);
    assert!(rig.runs().await.is_empty());
}
