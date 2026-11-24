//! `mac-farm` against a fake farm on loopback (`mac_farm/fake.rs`): the blobs it uploads and
//! how, the action it executes, how it follows the operation when the stream is cut, what it
//! prints and how it ends for each answer of the farm; and the programs it runs, with
//! the `xtask` binary in place of git and Cargo. The run on a real farm Mac is the workflow's.

#[path = "mac_farm/fake.rs"]
mod fake;
#[path = "mac_farm/outputs.rs"]
mod outputs;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kbf_proto::google::longrunning::operation;
use kbf_proto::google::rpc;
use kbf_proto::reapi::execution_stage::Value as Stage;
use kbf_proto::reapi::{Action, ActionResult, Command, Digest, ExecuteResponse};
use kbf_testkit::dir::TestDir;
use prost::Message as _;
use tonic::{Code, Status};
use xtask::Error;
use xtask::fetch::Sha256;
use xtask::mac_farm::client::{Farm, FarmError, Progress};
use xtask::mac_farm::toolchain::{Pin, Pins};
use xtask::mac_farm::tree::digest_of;
use xtask::mac_farm::{Args, Host, fetch_crates, git_files, run_with};

use fake::{Answer, State, answered, at, done};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-mac-farm"),
        name,
    )
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A fake farm and the runtime it runs on.
struct Rig {
    runtime: tokio::runtime::Runtime,
    state: Arc<State>,
    addr: std::net::SocketAddr,
}

impl Rig {
    fn new() -> Rig {
        let runtime = runtime();
        let state = Arc::new(State::default());
        let addr = fake::start(&runtime, &state);
        Rig {
            runtime,
            state,
            addr,
        }
    }

    fn farm(&self, instance: &str) -> Farm {
        self.runtime
            .block_on(Farm::connect(
                &format!("http://{}", self.addr),
                false,
                instance,
            ))
            .expect("connect")
    }

    /// Puts `bytes` in the fake's CAS; their digest.
    fn put(&self, bytes: &[u8]) -> Digest {
        let digest = digest_of(bytes);
        self.state
            .blobs
            .lock()
            .expect("blobs")
            .insert(digest.hash.clone(), bytes.to_vec());
        digest
    }

    fn args(&self, scratch: &Path) -> Args {
        Args {
            address: format!("grpc://{}", self.addr),
            instance: String::new(),
            tls: "false".to_owned(),
            properties: "OSFamily=macos".to_owned(),
            scratch: scratch.to_owned(),
            timeout_secs: 900,
            git: PathBuf::from("git"),
            cargo_program: PathBuf::from("cargo"),
            outputs: Vec::new(),
            out_dir: None,
            cargo: vec!["test".to_owned(), "-p".to_owned(), "kbf-update".to_owned()],
        }
    }

    /// A run of the verb over a one-file checkout and a one-file toolchain; its result, stdout
    /// and stderr.
    fn run(&self, name: &str) -> (Result<Vec<String>, Error>, String, String) {
        self.run_in(&scratch(name), |a| a)
    }

    /// [`Rig::run`] in `dir`, with the flags `flags` makes of the rig's.
    fn run_in(
        &self,
        dir: &TestDir,
        flags: impl FnOnce(Args) -> Args,
    ) -> (Result<Vec<String>, Error>, String, String) {
        let root = dir.path().join("checkout");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").expect("write");
        let xz = toolchain_xz();
        let pins = Pins {
            rust: "1".to_owned(),
            component: vec![Pin {
                name: "cargo".to_owned(),
                url: "https://example.invalid/cargo.tar.xz".to_owned(),
                sha256: Sha256::of(&xz).to_string(),
                dir: "cargo".to_owned(),
            }],
        };
        let mut tracked = || Ok(vec!["Cargo.toml".to_owned()]);
        let mut fetch = |home: &Path| {
            std::fs::write(home.join("config.toml"), "").expect("write");
            Ok(())
        };
        let mut download = |_: &str| Ok(xz.clone());
        let mut host = Host {
            tracked: &mut tracked,
            fetch: &mut fetch,
            pins,
            download: &mut download,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = run_with(
            &flags(self.args(&dir.path().join("scratch"))),
            &root,
            &mut host,
            &mut out,
            &mut err,
        );
        (
            result,
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    }

    /// The Action and Command of the n-th Execute.
    fn executed(&self, n: usize) -> (Action, Command) {
        let request = self.state.executes.lock().expect("executes")[n].clone();
        let blobs = self.state.blobs.lock().expect("blobs");
        let digest = request.action_digest.expect("action");
        let action = Action::decode(blobs[&digest.hash].as_slice()).expect("an Action");
        let command_digest = action.command_digest.clone().expect("command");
        let command = Command::decode(blobs[&command_digest.hash].as_slice()).expect("a Command");
        (action, command)
    }
}

/// A component tarball holding `cargo/bin/cargo`.
fn toolchain_xz() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o755);
    header.set_size(5);
    builder
        .append_data(&mut header, "cargo-1/cargo/bin/cargo", &b"cargo"[..])
        .expect("append");
    let tar = builder.into_inner().expect("tar");
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut tar.as_slice(), &mut xz).expect("xz");
    xz
}

fn exited(exit_code: i32, stdout: Option<Digest>, stderr: Option<Digest>) -> ExecuteResponse {
    ExecuteResponse {
        result: Some(ActionResult {
            exit_code,
            stdout_digest: stdout,
            stderr_digest: stderr,
            ..ActionResult::default()
        }),
        ..ExecuteResponse::default()
    }
}

fn failure(e: Result<Vec<String>, Error>) -> String {
    e.expect_err("a failure").to_string()
}

/// Catches a passing command reported as anything else, its stdout not printed, an input of the
/// run left out of the upload, and a second run that uploads what the farm already has (the
/// farm's CAS is shared, so an unchanged input is sent once): the second run finds every blob.
#[test]
fn a_command_that_passes_on_the_mac_ends_ok_and_prints_its_output() {
    let rig = Rig::new();
    let out = rig.put(b"test result: ok\n");
    for _ in 0..2 {
        rig.state.queue(Answer::Updates(vec![
            at("op/1", Stage::Queued),
            at("op/1", Stage::Executing),
            answered("op/1", &exited(0, Some(out.clone()), None)),
        ]));
    }
    let (result, stdout, stderr) = rig.run("passes");
    assert_eq!(
        result.expect("ok"),
        ["ok: `cargo test -p kbf-update` passed on a farm Mac"]
    );
    assert_eq!(
        stdout,
        format!(
            "::add-mask::grpc://{0}\n::add-mask::{0}\ntest result: ok\n",
            rig.addr
        )
    );
    assert!(stderr.contains(" missing from the farm ("), "{stderr}");
    let (action, command) = rig.executed(0);
    assert_eq!(command.arguments[0], ".kbf-mac/toolchain/bin/cargo");
    assert_eq!(action.timeout.map(|t| t.seconds), Some(900));
    let platform = action.platform.expect("platform").properties;
    assert_eq!(platform.len(), 1);
    assert_eq!(
        (platform[0].name.as_str(), platform[0].value.as_str()),
        ("OSFamily", "macos")
    );
    let batches = rig.state.batches.load(Ordering::SeqCst);
    assert!(batches > 0);
    let (again, _, stderr) = rig.run("passes-again");
    assert!(again.is_ok());
    assert!(
        stderr.contains(", 0 missing from the farm (0 bytes to upload)"),
        "{stderr}"
    );
    assert_eq!(rig.state.batches.load(Ordering::SeqCst), batches);
}

/// Catches a command's own failure read as a pass, its exit code not named, and its stderr
/// not printed.
#[test]
fn a_command_that_fails_on_the_mac_fails_the_verb_with_its_exit_code() {
    let rig = Rig::new();
    let err = rig.put(b"error[E0425]\n");
    rig.state.queue(Answer::Updates(vec![answered(
        "op/2",
        &exited(101, None, Some(err)),
    )]));
    let (result, _, stderr) = rig.run("fails");
    assert_eq!(
        failure(result),
        "`cargo test -p kbf-update` exited 101 on a farm Mac"
    );
    assert!(stderr.ends_with("error[E0425]\n"), "{stderr}");
}

/// Catches a cached result not said to be one: a pass from the cache is a pass on identical
/// inputs, and the log says which it was.
#[test]
fn a_cached_result_is_named() {
    let rig = Rig::new();
    let mut response = exited(0, None, None);
    response.cached_result = true;
    rig.state
        .queue(Answer::Updates(vec![answered("op/3", &response)]));
    let (result, _, _) = rig.run("cached");
    assert_eq!(
        result.expect("ok"),
        ["ok: `cargo test -p kbf-update` passed on a farm Mac, a cached result"]
    );
}

/// Catches a job the farm lost read as a pass or as the command's failure, its status not
/// named, and the output tail the farm attached not printed.
#[test]
fn a_job_the_farm_could_not_finish_fails_with_its_status_and_output_tail() {
    let rig = Rig::new();
    let mut metadata = BTreeMap::new();
    metadata.insert("stderr".to_owned(), "killed".to_owned());
    let tail = prost_types::Any {
        type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
        value: rpc::ErrorInfo {
            reason: "JOB_OUTPUT_TAIL".to_owned(),
            metadata,
            ..rpc::ErrorInfo::default()
        }
        .encode_to_vec(),
    };
    let response = ExecuteResponse {
        status: Some(rpc::Status {
            code: Code::Unavailable as i32,
            message: "the job was lost".to_owned(),
            details: vec![tail],
        }),
        ..ExecuteResponse::default()
    };
    rig.state
        .queue(Answer::Updates(vec![answered("op/4", &response)]));
    let (result, _, stderr) = rig.run("lost");
    assert_eq!(
        failure(result),
        "the farm did not run `cargo test -p kbf-update` to its end: Unavailable: the job was lost"
    );
    assert!(
        stderr.ends_with("--- the job's stderr (its end) ---\nkilled\n"),
        "{stderr}"
    );
}

/// Catches an operation that ended with an error (no response) taken as no answer at all: it
/// is the farm's refusal, named by its status.
#[test]
fn an_operation_that_ended_with_an_error_fails_with_that_error() {
    let rig = Rig::new();
    let status = rpc::Status {
        code: Code::InvalidArgument as i32,
        message: "the action's timeout is over the executor's limit".to_owned(),
        details: Vec::new(),
    };
    rig.state.queue(Answer::Updates(vec![done(
        "op/5",
        Some(operation::Result::Error(status)),
    )]));
    let (result, _, _) = rig.run("op-error");
    assert_eq!(
        failure(result),
        "the farm did not run `cargo test -p kbf-update` to its end: InvalidArgument: the action's timeout is over the executor's limit"
    );
}

/// Catches an OK answer with no result read as a pass.
#[test]
fn an_answer_with_no_result_fails() {
    let rig = Rig::new();
    rig.state.queue(Answer::Updates(vec![answered(
        "op/6",
        &ExecuteResponse::default(),
    )]));
    let (result, _, _) = rig.run("no-result");
    assert_eq!(
        failure(result),
        "the farm answered `cargo test -p kbf-update` with no result"
    );
}

/// Catches a farm with no Mac (its Execute refused at once) waited on or read as a pass: the
/// refusal and its message are the verb's failure.
#[test]
fn a_farm_with_no_mac_refuses_and_the_verb_says_why() {
    let rig = Rig::new();
    rig.state.queue(Answer::Refuse(Status::invalid_argument(
        "the action asks for a Mac, and this executor has none",
    )));
    let (result, _, _) = rig.run("no-mac");
    assert_eq!(
        failure(result),
        "Execute: InvalidArgument: the action asks for a Mac, and this executor has none"
    );
}

fn follow(rig: &Rig, farm: &Farm) -> (Result<ExecuteResponse, FarmError>, Vec<Progress>) {
    let mut seen = Vec::new();
    let action = digest_of(b"action");
    let got = rig
        .runtime
        .block_on(farm.execute(&action, &mut |p| seen.push(p)));
    (got, seen)
}

/// Catches a cut stream given up on (a frontend restart would fail every run), the operation
/// asked for by another name, a stage repeated in the log for each update, and a stage the log
/// has no line for (the cache check) printed as one it has.
#[test]
fn a_cut_stream_is_followed_with_wait_execution() {
    let rig = Rig::new();
    let farm = rig.farm("");
    rig.state.queue(Answer::Updates(vec![
        at("op/7", Stage::CacheCheck),
        at("op/7", Stage::Queued),
        at("op/7", Stage::Queued),
        Err(Status::unavailable("the frontend restarts")),
    ]));
    rig.state.queue(Answer::Updates(vec![
        at("op/7", Stage::Executing),
        answered("op/7", &exited(0, None, None)),
    ]));
    let (got, seen) = follow(&rig, &farm);
    assert_eq!(
        got.expect("a response").result.map(|r| r.exit_code),
        Some(0)
    );
    assert_eq!(seen, [Progress::Queued, Progress::Executing]);
    assert_eq!(*rig.state.waits.lock().expect("waits"), ["op/7"]);
    assert_eq!(rig.state.executes.lock().expect("executes").len(), 1);
}

/// Catches an operation the farm has forgotten (WaitExecution's NOT_FOUND) taken as a
/// failure: the action is executed again.
#[test]
fn an_operation_the_farm_forgot_is_executed_again() {
    let rig = Rig::new();
    let farm = rig.farm("");
    rig.state
        .queue(Answer::Updates(vec![at("op/8", Stage::Queued)]));
    rig.state.queue(Answer::Refuse(Status::not_found("gone")));
    rig.state.queue(Answer::Updates(vec![answered(
        "op/9",
        &exited(0, None, None),
    )]));
    let (got, _) = follow(&rig, &farm);
    assert!(got.is_ok());
    assert_eq!(rig.state.executes.lock().expect("executes").len(), 2);
}

/// Catches a stream that keeps breaking followed for ever, and the last reason not named.
#[test]
fn a_stream_cut_again_and_again_is_given_up() {
    let rig = Rig::new();
    let farm = rig.farm("");
    for _ in 0..3 {
        rig.state.queue(Answer::Updates(vec![
            at("op/10", Stage::Queued),
            Err(Status::unavailable("cut")),
        ]));
    }
    let (got, _) = follow(&rig, &farm);
    assert_eq!(
        got.expect_err("given up").to_string(),
        "the Execute stream was cut 3 times; last: Unavailable: cut"
    );
    rig.state
        .queue(Answer::Updates(vec![at("op/10", Stage::Queued)]));
    rig.state.queue(Answer::Refuse(Status::internal("broken")));
    let (got, _) = follow(&rig, &farm);
    assert_eq!(
        got.expect_err("refused").to_string(),
        "WaitExecution: Internal: broken"
    );
    assert_eq!(rig.state.waits.lock().expect("waits").len(), 3);
}

/// Catches a done operation with no response, or one that does not decode, read as a pass.
#[test]
fn a_done_operation_without_a_response_fails() {
    let rig = Rig::new();
    let farm = rig.farm("");
    rig.state.queue(Answer::Updates(vec![done("op/11", None)]));
    let (got, _) = follow(&rig, &farm);
    assert_eq!(
        got.expect_err("no response").to_string(),
        "the operation ended with no response"
    );
    rig.state.queue(Answer::Updates(vec![done(
        "op/12",
        Some(operation::Result::Response(prost_types::Any {
            type_url: String::new(),
            value: vec![0xff, 0xff],
        })),
    )]));
    let (got, _) = follow(&rig, &farm);
    assert!(matches!(got, Err(FarmError::Garbage(_))), "{got:?}");
}

/// Catches an output the farm sent inline not printed, an inline output printed twice (it is a
/// copy of the blob its digest names), and an output the farm cannot serve taken as empty.
#[test]
fn inline_output_is_printed_once_and_an_output_that_cannot_be_read_fails() {
    let rig = Rig::new();
    let both = rig.put(b"from the blob\n");
    let mut response = exited(0, Some(both.clone()), None);
    if let Some(result) = response.result.as_mut() {
        result.stdout_raw = b"inline\n".to_vec();
        result.stderr_raw = b"inline err\n".to_vec();
    }
    rig.state
        .queue(Answer::Updates(vec![answered("op/13", &response)]));
    let (result, stdout, stderr) = rig.run("inline");
    assert!(result.is_ok());
    assert!(stdout.ends_with("\ninline\n"), "{stdout}");
    assert!(!stdout.contains("from the blob"), "{stdout}");
    assert!(stderr.ends_with("inline err\n"), "{stderr}");
    let gone = digest_of(b"never stored");
    rig.state.queue(Answer::Updates(vec![answered(
        "op/14",
        &exited(0, Some(gone), None),
    )]));
    let (result, _, _) = rig.run("unreadable");
    assert!(failure(result).starts_with("ByteStream.Read: NotFound: no blobs/"),);
}

/// Catches a blob of exactly the batch limit streamed (or one byte over it batched), a streamed
/// blob of a whole number of chunks whose last message is not `finish_write` or that sends a
/// message after it, and a resource name sent again after the first message: the fake refuses
/// each of those streams.
#[test]
fn blobs_at_the_batch_and_chunk_limits_go_the_right_way() {
    let rig = Rig::new();
    let farm = rig.farm("");
    let mib = 1 << 20;
    let mut blobs = BTreeMap::new();
    let mut digests = Vec::new();
    for (n, size) in [mib, mib + 1, 2 * mib].into_iter().enumerate() {
        let bytes = vec![u8::try_from(n).expect("small"); size];
        let digest = digest_of(&bytes);
        blobs.insert(digest.hash.clone(), bytes);
        digests.push(digest);
    }
    rig.runtime
        .block_on(farm.upload(&blobs, &digests))
        .expect("upload");
    assert_eq!(rig.state.batches.load(Ordering::SeqCst), 1);
    let mut writes: Vec<usize> = rig
        .state
        .writes
        .lock()
        .expect("writes")
        .iter()
        .map(|(_, messages)| *messages)
        .collect();
    writes.sort_unstable();
    assert_eq!(writes, [2, 2]);
    for digest in &digests {
        assert_eq!(
            rig.runtime.block_on(farm.read(digest)).expect("read"),
            blobs[&digest.hash]
        );
    }
}

/// Catches a large blob put in a batch (the farm's message limit), a batch over its byte
/// budget, a streamed blob not under the instance or not in order, and FindMissingBlobs asked
/// about more digests in one call than its limit.
#[test]
fn small_blobs_go_in_batches_and_large_ones_are_streamed() {
    let rig = Rig::new();
    let farm = rig.farm("main");
    let mut blobs = BTreeMap::new();
    let mut digests = Vec::new();
    for n in 0..4_u8 {
        let bytes = vec![n; 900 << 10];
        let digest = digest_of(&bytes);
        blobs.insert(digest.hash.clone(), bytes);
        digests.push(digest);
    }
    let large = vec![9_u8; (5 << 20) / 2];
    let large_digest = digest_of(&large);
    blobs.insert(large_digest.hash.clone(), large.clone());
    digests.push(large_digest.clone());
    let missing = rig
        .runtime
        .block_on(farm.missing(&digests))
        .expect("missing");
    assert_eq!(missing.len(), 5);
    rig.runtime
        .block_on(farm.upload(&blobs, &missing))
        .expect("upload");
    assert_eq!(rig.state.batches.load(Ordering::SeqCst), 2);
    let writes = rig.state.writes.lock().expect("writes").clone();
    assert_eq!(writes.len(), 1);
    let h = &large_digest.hash;
    assert_eq!(
        writes[0],
        (
            format!(
                "main/uploads/{}-{}-4{}-8{}-{}/blobs/{h}/{}",
                &h[..8],
                &h[8..12],
                &h[13..16],
                &h[17..20],
                &h[20..32],
                large.len()
            ),
            3
        )
    );
    assert_eq!(
        rig.runtime
            .block_on(farm.read(&large_digest))
            .expect("read"),
        large
    );
    let many: Vec<Digest> = (0..4097_u32).map(|n| digest_of(&n.to_le_bytes())).collect();
    let finds = rig.state.finds.load(Ordering::SeqCst);
    let missing = rig.runtime.block_on(farm.missing(&many)).expect("missing");
    assert_eq!(missing.len(), 4097);
    assert_eq!(rig.state.finds.load(Ordering::SeqCst), finds + 2);
}

/// Catches an upload the farm refused, one it committed short, and a digest the run has no
/// bytes for, each taken as uploaded.
#[test]
fn an_upload_the_farm_did_not_keep_fails() {
    let rig = Rig::new();
    let farm = rig.farm("");
    let small = b"small".to_vec();
    let small_digest = digest_of(&small);
    let large = vec![1_u8; (1 << 20) + 1];
    let large_digest = digest_of(&large);
    let blobs = BTreeMap::from([
        (small_digest.hash.clone(), small),
        (large_digest.hash.clone(), large),
    ]);
    *rig.state.refuse.lock().expect("refuse") = Some(small_digest.hash.clone());
    let refused = rig
        .runtime
        .block_on(farm.upload(&blobs, std::slice::from_ref(&small_digest)))
        .expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the upload of {} was refused: 8: the fake is full",
            small_digest.hash
        )
    );
    rig.state.short.store(true, Ordering::SeqCst);
    let short = rig
        .runtime
        .block_on(farm.upload(&blobs, std::slice::from_ref(&large_digest)))
        .expect_err("short");
    assert_eq!(
        short.to_string(),
        format!(
            "the upload of {} wrote {} of {} bytes",
            large_digest.hash,
            (1 << 20),
            (1 << 20) + 1
        )
    );
    let stranger = digest_of(b"not ours");
    let unknown = rig
        .runtime
        .block_on(farm.upload(&blobs, std::slice::from_ref(&stranger)))
        .expect_err("unknown");
    assert_eq!(
        unknown.to_string(),
        format!("{} is not among the blobs of the action", stranger.hash)
    );
}

/// Catches the empty blob asked of the farm (it is never stored), a read under the wrong
/// instance, and a missing blob read as empty.
#[test]
fn a_read_names_the_instance_and_a_missing_blob_fails() {
    let rig = Rig::new();
    let farm = rig.farm("main");
    let empty = rig
        .runtime
        .block_on(farm.read(&digest_of(b"")))
        .expect("empty");
    assert!(empty.is_empty());
    assert!(rig.state.reads.lock().expect("reads").is_empty());
    let gone = digest_of(b"gone");
    let missing = rig.runtime.block_on(farm.read(&gone)).expect_err("missing");
    assert!(
        missing
            .to_string()
            .starts_with("ByteStream.Read: NotFound: no main/blobs/"),
        "{missing}"
    );
}

/// Catches a farm that is not there reported as anything but a failed connect to its
/// address, and an address that is not a URL.
#[test]
fn a_farm_that_does_not_answer_is_a_failed_connect() {
    let runtime = runtime();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let refused = runtime
        .block_on(Farm::connect(&endpoint, false, ""))
        .err()
        .expect("refused");
    assert!(
        refused
            .to_string()
            .starts_with(&format!("the farm address {endpoint:?} does not connect: ")),
        "{refused}"
    );
    let not_url = runtime
        .block_on(Farm::connect("not a url\n", true, ""))
        .err()
        .expect("not a url");
    assert!(matches!(not_url, FarmError::Connect { .. }));
    let tls = runtime
        .block_on(Farm::connect(
            &format!("https://127.0.0.1:{port}"),
            true,
            "",
        ))
        .err()
        .expect("refused");
    assert!(matches!(tls, FarmError::Connect { .. }));
}

/// Catches a program that failed, or could not start, taken as an empty checkout or a fetch
/// that worked: the `xtask` binary stands in for git and Cargo, and refuses both commands.
#[test]
fn git_and_cargo_failing_fail_the_run_with_their_stderr() {
    let dir = scratch("programs");
    let root = dir.path();
    let failed = git_files(Path::new(XTASK), root).expect_err("refused");
    assert!(
        failed
            .to_string()
            .starts_with("git ls-files failed: error: "),
        "{failed}"
    );
    let failed = fetch_crates(Path::new(XTASK), root, &root.join("home")).expect_err("refused");
    assert!(
        failed
            .to_string()
            .starts_with("cargo fetch failed: error: "),
        "{failed}"
    );
    let missing = root.join("no-such-program");
    let failed = git_files(&missing, root).expect_err("no program");
    assert!(failed.to_string().starts_with("run "), "{failed}");
    let failed = fetch_crates(&missing, root, root).expect_err("no program");
    assert!(failed.to_string().starts_with("run "), "{failed}");
}

/// Catches the verb missing from the command line a workflow runs, a flag that is wrong not
/// ending the run with its message and exit 1, and no address not ending 0 with a notice.
#[test]
fn the_binary_ends_0_with_no_farm_and_1_for_a_wrong_flag() {
    let notice = Process::new(XTASK)
        .args(["mac-farm", "--address", "", "--scratch", "s", "--", "test"])
        .output()
        .expect("run");
    assert!(notice.status.success());
    assert_eq!(
        String::from_utf8_lossy(&notice.stdout),
        "::notice::mac-farm runs nothing: the repository variable FARM_ADDRESS is not set\n"
    );
    let wrong = Process::new(XTASK)
        .args([
            "mac-farm",
            "--address",
            "grpc://farm.example:1",
            "--tls",
            "maybe",
            "--scratch",
            "s",
            "--",
            "test",
        ])
        .output()
        .expect("run");
    assert_eq!(wrong.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&wrong.stderr),
        "xtask: --tls is \"maybe\": it must be `true` or `false`\n"
    );
    assert!(xtask::check_args(&["mac-farm", "--address", "x", "--scratch", "s"]).is_err());
}
