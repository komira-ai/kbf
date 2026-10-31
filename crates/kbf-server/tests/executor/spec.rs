//! What a posted run holds: the mounts, the labels, the task tool's flags, and what the
//! executor refuses to post at all.

use kbf_front::Finished;
use kbf_server::executor::MintError;
use tonic::Code;

use kbf_server::executor::DISK_MARGIN;

use crate::real_prep::Log;
use crate::rig::{GIB, INPUT_BYTES, Posted, Rig};

fn code(finished: &Finished) -> Code {
    match finished {
        Finished::Failed(status) => Code::from_i32(status.code),
        other => panic!("not a failure: {other:?}"),
    }
}

fn message(finished: &Finished) -> String {
    match finished {
        Finished::Failed(status) => status.message.clone(),
        other => panic!("not a failure: {other:?}"),
    }
}

async fn posted(rig: &mut Rig) -> Posted {
    rig.pump().await;
    let _caller = rig.submit(3);
    rig.pump().await;
    rig.runs_of(3).await.remove(0)
}

/// Catches a posted run that gives the action more than it should or leaks what it must not:
/// nothing is mounted into the container (the task tool is the kbf image's entrypoint, not a
/// host file the action could replace), the labels, which every list of runs shows and which
/// are not secret-safe, hold the run's nonce and nothing else, neither secret appears in any
/// argument but the flag that carries it, and the optional flags (a plain-http `PUT`, a working
/// directory), and the retired `--root-der`, are absent unless the config asks.
#[tokio::test(start_paused = true)]
async fn a_run_mounts_nothing_and_holds_no_secret_outside_its_flags() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    let run = posted(&mut rig).await;
    assert_eq!(rig.fake.run_mounts(&run.run), Some(Vec::new()));
    let listed = rig.api.runs(run.lease).await.expect("runs");
    let labels = &listed[0].labels;
    assert_eq!(labels.len(), 1, "{labels:?}");
    let nonce = labels.get("kbf-run").expect("the run label");
    assert!(nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    let (url, token) = (
        run.arg("--pack-url").expect("a url").to_owned(),
        run.arg("--grant-token").expect("a token").to_owned(),
    );
    for (at, arg) in run.args.iter().enumerate() {
        let carries_url =
            run.args.get(at.wrapping_sub(1)).map(String::as_str) == Some("--pack-url");
        let carries_token =
            run.args.get(at.wrapping_sub(1)).map(String::as_str) == Some("--grant-token");
        assert!(
            carries_url || !arg.contains(&url),
            "the pack url is in {arg:?}"
        );
        assert!(
            carries_token || !arg.contains(&token),
            "the read token is in {arg:?}"
        );
    }
    assert!(
        labels
            .values()
            .all(|v| !v.contains(&url) && !v.contains(&token))
    );
    for flag in [
        "--allow-plain-http-put",
        "--allow-plain-http-grant",
        "--locate",
        "--root-der",
        "--work-dir",
    ] {
        assert!(!run.args.iter().any(|a| a == flag), "{flag} was passed");
    }
}

/// Catches the config's choices not reaching the run: with a plain-http `PUT` and a working
/// directory asked for, the flags are passed, after the task tool's own, and still nothing is
/// mounted.
#[tokio::test(start_paused = true)]
async fn the_configs_choices_reach_the_run() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        cfg.allow_plain_http_put = true;
        cfg.work_dir = Some("/scratch/work".to_owned());
    })
    .await;
    let run = posted(&mut rig).await;
    assert_eq!(run.args[0], "--action");
    assert_eq!(run.arg("--work-dir"), Some("/scratch/work"));
    assert!(run.args.iter().any(|a| a == "--allow-plain-http-put"));
    assert_eq!(rig.fake.run_mounts(&run.run), Some(Vec::new()));
}

/// Catches a bearer token sent in clear: a read grant that is not served over https is
/// infrastructure to rerun (the operator's to fix), and no run is posted with the token in its
/// arguments.
#[tokio::test(start_paused = true)]
async fn a_read_grant_that_is_not_https_is_never_posted() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    rig.inputs
        .grant
        .lock()
        .expect("grant")
        .as_mut()
        .expect("ok")
        .url = "http://grant.test:9400".to_owned();
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::Internal, "{finished:?}");
    assert!(rig.runs().await.is_empty());
}

/// Catches a read grant over plain http posted without the operator's word, and the opt-in not
/// reaching the task: with `allow_plain_http_grant` set an `http://` grant is posted, its url
/// is the task's `--grant-url`, and the task tool is given exactly one
/// `--allow-plain-http-grant`; an `https://` grant with the flag set is posted too, with the
/// same one flag (the flag is the config's, not the url's).
#[tokio::test(start_paused = true)]
async fn a_plain_http_grant_is_posted_only_when_allowed_and_the_task_is_told() {
    for url in ["http://grant.test:9400", "https://grant.test:9400"] {
        let mut rig = Rig::start(&["node1"], 1, |cfg| cfg.allow_plain_http_grant = true).await;
        rig.pump().await;
        rig.inputs
            .grant
            .lock()
            .expect("grant")
            .as_mut()
            .expect("ok")
            .url = url.to_owned();
        let run = posted(&mut rig).await;
        assert_eq!(run.arg("--grant-url"), Some(url));
        let given = run
            .args
            .iter()
            .filter(|a| *a == "--allow-plain-http-grant")
            .count();
        assert_eq!(given, 1, "{:?}", run.args.len());
    }
}

/// Catches the config's `locate` not reaching the task, or reaching it twice: with it set the
/// task tool is given exactly one `--locate` (the default config's runs have none, which the
/// leak test above pins), and the grant and the other flags are those of a run without it.
#[tokio::test(start_paused = true)]
async fn executor_posts_run_with_locate_flag() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| cfg.locate = true).await;
    let run = posted(&mut rig).await;
    let given = run.args.iter().filter(|a| *a == "--locate").count();
    assert_eq!(given, 1, "{:?}", run.args.len());
    assert_eq!(run.args.last().map(String::as_str), Some("--locate"));
    assert_eq!(run.args[0], "--action");
    assert!(run.arg("--grant-url").is_some());
}

/// Catches a write url sent in clear without the operator's word: a pack url that is not
/// https is infrastructure to rerun while `allow_plain_http_put` is unset, and no run is posted
/// with it; with the flag set the same url is posted.
#[tokio::test(start_paused = true)]
async fn a_pack_url_that_is_not_https_is_posted_only_when_plain_http_is_allowed() {
    for allowed in [false, true] {
        let mut rig =
            Rig::start(&["node1"], 1, move |cfg| cfg.allow_plain_http_put = allowed).await;
        rig.presign
            .plain
            .store(true, std::sync::atomic::Ordering::Relaxed);
        rig.pump().await;
        let caller = rig.submit(3);
        match allowed {
            false => {
                let finished = rig.until_done(&caller, 3).await;
                assert_eq!(code(&finished), Code::Internal, "{finished:?}");
                assert!(rig.runs().await.is_empty());
            }
            true => {
                let run = posted(&mut rig).await;
                let url = run.arg("--pack-url").expect("a pack url");
                assert!(url.starts_with("http://store.test/"), "{url}");
            }
        }
    }
}

/// Catches a run the preparation refuses (an input not held, a tree over budget) being rerun as
/// if the farm were at fault: asking again gives the same answer, so it is the caller's to fix
/// and ends as an invalid argument, not as a retry, and nothing is posted.
#[tokio::test(start_paused = true)]
async fn a_run_the_preparation_refuses_is_the_callers_not_rerun() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    *rig.ingest.mint_fails.lock().expect("mint") =
        Some(MintError::Refused("an input is not held".to_owned()));
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
    assert!(
        format!("{finished:?}").contains("an input is not held"),
        "{finished:?}"
    );
    assert!(rig.runs().await.is_empty(), "posted without a key");
}

/// Catches a key the mint would not issue being posted anyway: the run needs a key to write,
/// and without one it is infrastructure, so nothing is posted and the farm runs the action
/// again, which the mint then answers.
#[tokio::test(start_paused = true)]
async fn a_pack_key_that_cannot_be_issued_is_rerun_not_posted() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    *rig.ingest.mint_fails.lock().expect("mint") =
        Some(MintError::Unavailable("no secret".to_owned()));
    let caller = rig.submit(3);
    rig.farm.tick().await;
    crate::rig::settle().await;
    rig.exec.step().await;
    assert!(rig.runs().await.is_empty(), "posted without a key");
    rig.pump().await;
    let run = rig.runs_of(3).await.remove(0);
    rig.host_finishes(&run.run, kbf_lease_client::Outcome::Succeeded, Some(0));
    let finished = rig.until_done(&caller, 6).await;
    assert!(matches!(finished, Finished::Ran(_)), "{finished:?}");
}

/// Catches a lease taken of any host: the request names the system and the architecture the
/// worker reports, so a pool that mixes in hosts of another kind never hands this executor
/// one. A pool of aarch64 hosts, or of another system, gives it none and it holds none.
#[tokio::test(start_paused = true)]
async fn only_hosts_of_the_reported_system_and_architecture_are_leased() {
    for (key, value) in [("arch", "aarch64"), ("os", "darwin")] {
        let mut rig = Rig::pooled(
            &["node1"],
            1,
            16,
            |pool| {
                pool.capabilities.insert(key.to_owned(), value.to_owned());
            },
            |_| {},
        )
        .await;
        rig.pump().await;
        assert!(
            rig.exec.hosts().is_empty(),
            "{key}={value}: a host was leased"
        );
        assert!(rig.api.leases().await.expect("leases").is_empty());
    }
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
}

/// Catches a run that books the wrong scratch disk: the run posted carries the bytes of the
/// input closure the grant reports, the operator's output allowance and the fixed margin, each
/// once. The allowance is set to a number the default is not, so a run that ignores the config
/// cannot match; the input bytes are not a whole GiB, so a sum that drops them cannot either.
#[tokio::test(start_paused = true)]
async fn a_run_books_its_inputs_plus_the_output_allowance_plus_the_margin() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| cfg.output_disk_bytes = 5 * GIB + 7).await;
    let run = posted(&mut rig).await;
    assert_eq!(
        rig.fake.run_disk_bytes(&run.run),
        Some(INPUT_BYTES + 5 * GIB + 7 + DISK_MARGIN)
    );
}

/// Catches a grant that does not say how large its closure is booking nothing, or booking the
/// wrong thing: the run books the allowance and the margin, and the executor says in its log
/// that it did not know the inputs, so an operator who finds a full host can see why.
#[tokio::test(start_paused = true)]
async fn a_run_whose_inputs_are_not_measured_books_the_allowance_and_says_so() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let _logged = tracing::dispatcher::set_default(&dispatch);
    let mut rig = Rig::start(&["node1"], 1, |cfg| cfg.output_disk_bytes = 5 * GIB + 7).await;
    rig.pump().await;
    rig.inputs
        .grant
        .lock()
        .expect("grant")
        .as_mut()
        .expect("ok")
        .input_bytes = None;
    let _caller = rig.submit(3);
    rig.pump().await;
    let run = rig.runs_of(3).await.remove(0);
    assert_eq!(
        rig.fake.run_disk_bytes(&run.run),
        Some(5 * GIB + 7 + DISK_MARGIN)
    );
    let said = log.text();
    assert!(
        said.contains("size of the inputs is not known") && said.contains("books the allowance"),
        "{said}"
    );
}

/// Catches a run whose limits the pool's shape cannot hold being rerun for ever: every host
/// shares the shape, so the refusal `limits-exceed-shape` is the caller's, not the farm's.
/// Nothing is queued, the host stays held (a refusal is not a reason to drop or drain it), and
/// the action ends `InvalidArgument` (no rerun), the message naming the limit (disk, with the
/// bytes asked) and the shape. With a shape that holds the run, the same executor posts it.
#[tokio::test(start_paused = true)]
async fn a_run_whose_disk_exceeds_the_hosts_shape_is_refused_cleanly() {
    let booked = INPUT_BYTES + 8 * GIB + DISK_MARGIN;
    let mut rig = Rig::pooled(
        &["node1"],
        1,
        16,
        |pool| pool.shape.disk_bytes = Some(booked - 1),
        |_| {},
    )
    .await;
    rig.pump().await;
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::InvalidArgument, "{finished:?}");
    let said = message(&finished);
    assert!(
        said.contains("limits") && said.contains("shape"),
        "the message names neither the limits nor the shape: {said}"
    );
    assert!(
        said.contains(&format!("disk {booked} bytes")),
        "the message does not name the disk asked: {said}"
    );
    assert!(rig.runs().await.is_empty(), "a run the host cannot hold");
    let asked = rig
        .requests()
        .iter()
        .any(|r| r.ends_with("/runs") && r.starts_with("POST"));
    assert!(
        asked,
        "the lease server was never asked: {:?}",
        rig.requests()
    );
    assert_eq!(rig.exec.hosts().len(), 1, "the host was dropped");

    let mut rig = Rig::pooled(
        &["node1"],
        1,
        16,
        |pool| pool.shape.disk_bytes = Some(booked),
        |_| {},
    )
    .await;
    let run = posted(&mut rig).await;
    assert_eq!(rig.fake.run_disk_bytes(&run.run), Some(booked));
}

/// Catches the shape rule swallowing every refusal: a refusal that is not about the limits
/// (here a run id that exists) is still the farm's to run again, and ends as infrastructure.
#[tokio::test(start_paused = true)]
async fn a_refusal_that_is_not_about_the_shape_is_still_infrastructure() {
    let mut rig = Rig::pooled(&["node1"], 1, 16, |_| {}, |_| {}).await;
    rig.pump().await;
    assert!(rig.fake.refuse_posts("node1", Some("run-exists")));
    let caller = rig.submit(3);
    let finished = rig.until_done(&caller, 3).await;
    assert_eq!(code(&finished), Code::Internal, "{finished:?}");
    assert!(
        message(&finished).contains("infra failure"),
        "{}",
        message(&finished)
    );
    assert!(rig.runs().await.is_empty());
}
