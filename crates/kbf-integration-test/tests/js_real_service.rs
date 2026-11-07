//! `kbf-job-client` against the real job service: the `js-lease-server` of the jobscheduler
//! release that `kbf-js-version.toml` pins (`tests/js_real_support` says how it is fetched,
//! checked and started).
//!
//! The four cases that need the server are `#[ignore]`d with their reason: the program is a
//! download, which a pull request's tests do not make. beta.yml's `js-real-service` job fetches
//! and verifies it (`cargo xtask fetch-pinned-js`) and runs them with `--ignored`; without the
//! fetched archive each fails, naming the command to run. The rest of this file runs everywhere:
//! the harness's own parts against the fake and against bytes made here.
//!
//! - `submit_then_get_job_returns_the_submitted_spec_and_state`
//! - `cancel_moves_a_job_to_cancelled_and_a_second_cancel_is_no_error`
//! - `a_resubmit_after_a_dropped_answer_makes_one_job`
//! - `the_fake_and_the_real_server_answer_with_the_same_fields`: the same calls against
//!   `FakeJobServer` and the real server, each answer reduced to the set of fields it carries
//!   (or the class of its error); the two lists must be equal, so the fake cannot drift from
//!   the service the farm will call without this test going red.

mod js_real_support;

use std::collections::BTreeSet;
use std::time::Duration;

use js_proto::v1::{ExecMode, Job, JobSpec, JobState, ListJobsRequest, RefusalCode, Shape};
use kbf_job_client::fake::{FakeJobServer, Machine};
use kbf_job_client::{Client, Error};
use kbf_testkit::dir::TestDir;
use prost::Message as _;
use xtask::fetch::Sha256;
use xtask::js_pin::Pin;

use js_real_support::drop_answer::{DropFirstAnswer, answer_head, whole_frame};
use js_real_support::fields::Schema;
use js_real_support::{RealJobServer, as_read, job_addr_of, spec, unpack_pinned};

/// Asserts that a job submitted to the real server is answered queued, under its id, with the
/// spec as submitted less its file contents; that `GetJob` reads back the same job; and that a
/// job the server never had is `NOT_FOUND`. Catches a client that reads the answer from the
/// wrong field, sends a spec the service changes, or folds an unknown job into success.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned js-lease-server: beta.yml's js-real-service job fetches it (cargo xtask fetch-pinned-js) and runs these with --ignored"]
async fn submit_then_get_job_returns_the_submitted_spec_and_state() {
    let server = RealJobServer::start("js-real-submit").await;
    let client = server.client();
    let submitted = client.submit_job(spec("real-submit-1")).await.unwrap();
    assert_eq!(submitted.id, "real-submit-1");
    assert_eq!(submitted.state(), JobState::Queued);
    assert_eq!(submitted.spec, Some(as_read(spec("real-submit-1"))));
    assert!(submitted.host_id.is_empty(), "{submitted:?}");
    let queued_at = submitted.times.as_ref().map(|t| t.queued_at_ms);
    assert!(queued_at.is_some_and(|ms| ms > 0), "{submitted:?}");

    let read = client.get_job("real-submit-1").await.unwrap();
    assert_eq!(read, submitted);

    let unknown = client.get_job("real-submit-2").await.unwrap_err();
    assert!(
        matches!(
            unknown,
            Error::Status {
                code: tonic::Code::NotFound,
                ..
            }
        ),
        "{unknown:?}\n{}",
        server.log()
    );
}

/// Asserts that cancelling a queued job on the real server answers it cancelled at once, with
/// an end time; that cancelling it again is no error and answers the same job; that `GetJob`
/// then reads it cancelled; and that cancelling a job the server never had is `NOT_FOUND`.
/// Catches a client that treats a repeated cancel as a failure (the caller repeats a cancel
/// whose answer was lost) and a cancel that leaves a queued job queued.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned js-lease-server: beta.yml's js-real-service job fetches it (cargo xtask fetch-pinned-js) and runs these with --ignored"]
async fn cancel_moves_a_job_to_cancelled_and_a_second_cancel_is_no_error() {
    let server = RealJobServer::start("js-real-cancel").await;
    let client = server.client();
    client.submit_job(spec("real-cancel-1")).await.unwrap();

    let cancelled = client.cancel_job("real-cancel-1").await.unwrap();
    assert_eq!(cancelled.state(), JobState::Cancelled, "{cancelled:?}");
    let ended = cancelled.times.as_ref().and_then(|t| t.ended_at_ms);
    assert!(ended.is_some(), "{cancelled:?}");

    let again = client.cancel_job("real-cancel-1").await.unwrap();
    assert_eq!(again, cancelled);
    assert_eq!(client.get_job("real-cancel-1").await.unwrap(), cancelled);

    let unknown = client.cancel_job("real-cancel-2").await.unwrap_err();
    assert!(
        matches!(
            unknown,
            Error::Status {
                code: tonic::Code::NotFound,
                ..
            }
        ),
        "{unknown:?}\n{}",
        server.log()
    );
}

/// Asserts the retry rule against the real server: a submit whose answer a relay loses after
/// the server sent it is `Error::Transport` with `may_have_applied()` true; the server holds
/// the job; the same spec submitted again under the same id through the same client answers
/// that job; and a listing of every job holds that one job and no other. Catches a lost answer
/// classed as "nothing sent" (the caller would not retry, or would submit under a new id), and
/// a service that makes a second job for a repeated id.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned js-lease-server: beta.yml's js-real-service job fetches it (cargo xtask fetch-pinned-js) and runs these with --ignored"]
async fn a_resubmit_after_a_dropped_answer_makes_one_job() {
    let server = RealJobServer::start("js-real-retry").await;
    let relay = DropFirstAnswer::start(server.job_addr()).await;
    let through = RealJobServer::client_at(&relay.origin(), js_real_support::TOKEN);

    let lost = through.submit_job(spec("real-retry-1")).await.unwrap_err();
    assert!(relay.dropped(), "the relay lost no answer: {lost:?}");
    assert!(matches!(lost, Error::Transport(_)), "{lost:?}");
    assert!(lost.may_have_applied());

    let direct = server.client();
    let recorded = direct.get_job("real-retry-1").await.unwrap();
    let again = through.submit_job(spec("real-retry-1")).await.unwrap();
    assert_eq!(again, recorded);
    let all = direct
        .list_all_jobs(ListJobsRequest::default())
        .await
        .unwrap();
    assert_eq!(all, [recorded]);
}

/// What one call answered: the fields of its answer, or the class of its error.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    Fields(BTreeSet<String>),
    Refused(RefusalCode, BTreeSet<String>),
    Status(tonic::Code),
}

/// The answer of a call that returns `T`, a message of the full name `message`.
fn answer<T: prost::Message>(schema: &Schema, message: &str, result: Result<T, Error>) -> Answer {
    match result {
        Ok(value) => Answer::Fields(schema.fields(message, &value)),
        Err(Error::Refused(refusal)) => Answer::Refused(
            refusal.code(),
            schema.fields(&schema.message("Refusal"), &refusal),
        ),
        Err(Error::Status { code, .. }) => Answer::Status(code),
        Err(other) => panic!("{message}: an error that is no answer: {other:?}"),
    }
}

/// The same calls, in the same order, against the service `client` reaches, each with its
/// answer: submits (a first, a repeat, one under the id with another command), reads, a listing,
/// the result and logs of a live job, two cancels, the reads of the cancelled job, and the calls
/// on a job the service never had.
async fn the_calls(client: &Client) -> Vec<(&'static str, Answer)> {
    let schema = Schema::job_service();
    let job = schema.message("Job");
    let id = "drift-1";
    let mut other: JobSpec = spec(id);
    other.command = "/bin/other".to_owned();
    vec![
        (
            "submit",
            answer(&schema, &job, client.submit_job(spec(id)).await),
        ),
        (
            "submit again",
            answer(&schema, &job, client.submit_job(spec(id)).await),
        ),
        (
            "submit another job under the id",
            answer(&schema, &job, client.submit_job(other).await),
        ),
        ("get", answer(&schema, &job, client.get_job(id).await)),
        (
            "list",
            answer(
                &schema,
                &schema.message("ListJobsResponse"),
                client.list_jobs(ListJobsRequest::default()).await,
            ),
        ),
        (
            "result of a queued job",
            answer(
                &schema,
                &schema.message("JobResult"),
                client.get_result(id).await,
            ),
        ),
        (
            "logs of a queued job",
            answer(
                &schema,
                &schema.message("GetLogsResponse"),
                client.get_logs(id).await,
            ),
        ),
        ("cancel", answer(&schema, &job, client.cancel_job(id).await)),
        (
            "cancel again",
            answer(&schema, &job, client.cancel_job(id).await),
        ),
        (
            "get the cancelled job",
            answer(&schema, &job, client.get_job(id).await),
        ),
        (
            "result of the cancelled job",
            answer(
                &schema,
                &schema.message("JobResult"),
                client.get_result(id).await,
            ),
        ),
        (
            "get an unknown job",
            answer(&schema, &job, client.get_job("drift-none").await),
        ),
        (
            "cancel an unknown job",
            answer(&schema, &job, client.cancel_job("drift-none").await),
        ),
    ]
}

/// A fake that admits what the real server's pool admits: one machine the size of the pool's
/// shape that a provider could launch, and a clock moved off 0 (a time of 0 is the proto
/// default, which is not on the wire; the real server's clock is never there).
async fn fake_like_the_pool() -> FakeJobServer {
    let fake = FakeJobServer::start().await.unwrap();
    fake.add_offer(Machine {
        shape: Shape {
            cpu_millis: 8000,
            memory_bytes: 16 << 30,
            ..Shape::default()
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    });
    fake.advance(Duration::from_secs(1));
    fake
}

/// Asserts that the fake and the real server answer the same calls with the same fields: for
/// each call of [`the_calls`], the set of fields the answer carries, or the class of its error
/// (the refusal code and its fields, or the status code), is the same against both. Catches the
/// fake setting a field the service leaves empty or leaving one empty that the service sets, a
/// value carried under another field, and a call the fake answers OK where the service refuses
/// (or the other way), each of which would let the farm's tests pass on the fake and fail on
/// the service.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the pinned js-lease-server: beta.yml's js-real-service job fetches it (cargo xtask fetch-pinned-js) and runs these with --ignored"]
async fn the_fake_and_the_real_server_answer_with_the_same_fields() {
    let server = RealJobServer::start("js-real-drift").await;
    let real = the_calls(&server.client()).await;
    let fake = fake_like_the_pool().await;
    let faked = the_calls(&fake.client().unwrap()).await;
    assert_eq!(real.len(), faked.len());
    let differ: Vec<String> = real
        .iter()
        .zip(&faked)
        .filter(|((_, real), (_, faked))| real != faked)
        .map(|((call, real), (_, faked))| format!("{call}:\n  fake: {faked:?}\n  real: {real:?}"))
        .collect();
    assert!(
        differ.is_empty(),
        "the fake and the real server differ:\n{}",
        differ.join("\n")
    );
}

// ---- The harness's own parts, which need no download.

/// Asserts that the relay loses exactly the first answer, after the service applied it: in
/// front of the fake, the first submit is `Error::Transport` with `may_have_applied()` true and
/// the fake holds the job; the repeat goes through and answers that job. Catches a relay that
/// cuts before the request is applied (the real case would then test nothing about a lost
/// answer) or that cuts every connection.
#[tokio::test(flavor = "multi_thread")]
async fn the_relay_loses_the_first_answer_after_the_call_applied() {
    let fake = fake_like_the_pool().await;
    let addr = fake
        .origin()
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let relay = DropFirstAnswer::start(addr).await;
    let through = RealJobServer::client_at(&relay.origin(), kbf_job_client::fake::TOKEN);
    assert!(!relay.dropped());
    let lost = through.submit_job(spec("relay-1")).await.unwrap_err();
    assert!(relay.dropped());
    assert!(matches!(lost, Error::Transport(_)), "{lost:?}");
    assert_eq!(fake.jobs().len(), 1, "the lost submit was applied");
    let again = through.submit_job(spec("relay-1")).await.unwrap();
    assert_eq!(fake.jobs(), [again]);
}

/// Asserts that a held relay passes every answer until it is armed, then loses exactly the
/// next one, after the service applied it, and passes the answers after that: in front of the
/// fake, a submit before the arming is answered, the first after it is `Error::Transport` with
/// the job made, and the repeat is answered from that job. Catches a relay that loses an answer
/// a test meant to let through (the executor's first calls), or more than one.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_relay_loses_only_the_answer_after_it_is_armed() {
    let fake = fake_like_the_pool().await;
    let addr = fake
        .origin()
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    let relay = DropFirstAnswer::held(addr).await;
    let through = RealJobServer::client_at(&relay.origin(), kbf_job_client::fake::TOKEN);
    let before = through.submit_job(spec("held-1")).await.unwrap();
    assert!(!relay.dropped());
    relay.arm();
    let lost = through.submit_job(spec("held-2")).await.unwrap_err();
    assert!(relay.dropped());
    assert!(matches!(lost, Error::Transport(_)), "{lost:?}");
    assert_eq!(fake.jobs().len(), 2, "the lost submit was applied");
    let again = through.submit_job(spec("held-2")).await.unwrap();
    let after = through.submit_job(spec("held-3")).await.unwrap();
    let ids: Vec<String> = fake.jobs().into_iter().map(|job| job.id).collect();
    assert_eq!(ids, [before.id, again.id, after.id]);
}

/// Asserts the frame reading of the relay: a frame is whole only with its 9-byte head and all of
/// its payload, and only a `HEADERS` frame of a stream other than 0 is the head of an answer.
#[test]
fn the_relay_reads_frames_and_finds_the_head_of_an_answer() {
    let frame = |kind: u8, stream: u32, payload: &[u8]| {
        let len = u32::try_from(payload.len()).unwrap().to_be_bytes();
        let mut bytes = vec![len[1], len[2], len[3], kind, 0];
        bytes.extend_from_slice(&stream.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    };
    let settings = frame(0x4, 0, &[0; 6]);
    assert_eq!(whole_frame(&settings), Some(15));
    assert_eq!(whole_frame(&settings[..14]), None);
    assert_eq!(whole_frame(&settings[..8]), None);
    assert!(!answer_head(&settings));
    assert!(answer_head(&frame(0x1, 1, b"head")));
    assert!(!answer_head(&frame(0x1, 0, b"")));
    assert!(
        !answer_head(&frame(0x8, 1, &[0; 4])),
        "a window update is no answer"
    );
    assert!(!answer_head(&frame(0x0, 1, b"data")));
}

/// Asserts that the job port is read from the server's log line, and from no other line.
#[test]
fn the_job_port_is_read_from_its_log_line() {
    let line = "2026-11-06T18:43:14.196377Z  INFO js_lease_server::server: listening for the job \
                service addr=127.0.0.1:38361 tls=false";
    assert_eq!(job_addr_of(line), Some("127.0.0.1:38361".parse().unwrap()));
    let lease = "INFO js_lease_server::server: listening addr=127.0.0.1:36721 provider=layered";
    assert_eq!(job_addr_of(lease), None);
    assert_eq!(job_addr_of("listening for the job service tls=false"), None);
    assert_eq!(
        job_addr_of("listening for the job service addr=nowhere"),
        None
    );
}

/// Bytes standing in for an archive of the release: a gzip tar holding `member`.
fn archive(member: &str, program: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut header = tar::Header::new_ustar();
    header.set_path(member).unwrap();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(program.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder.append(&header, program).unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&builder.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

/// The checked-in pin with its `js-lease-server` row for this target pinned to `bytes`.
fn pinned_to(bytes: &[u8]) -> Pin {
    let mut pin = js_real_support::pin();
    let target = js_real_support::target();
    for row in &mut pin.archives {
        if row.program == "js-lease-server" && row.target == target {
            row.sha256 = Sha256::of(bytes);
        }
    }
    pin
}

/// Asserts that the harness runs only bytes the pin names: an archive pinned by its own digest
/// is unpacked and its program is the archive's; the same archive with one byte changed is
/// refused and nothing is unpacked; a pin whose digest differs by one digit from the archive's
/// is refused; a missing archive is refused naming the fetch to run; a program with no row is
/// refused. Catches a harness that runs a program left in the directory without checking the
/// bytes it came from, and one that skips when the fetch did not run.
#[test]
fn the_harness_unpacks_only_the_pinned_bytes() {
    let fetched = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-real-pin");
    let into = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "js-real-unpack");
    let probe = pinned_to(b"");
    let row = probe
        .archive("js-lease-server", &js_real_support::target())
        .unwrap();
    let name = probe.file_name(row);
    let member = probe.member(row);
    let bytes = archive(member.to_str().unwrap(), b"the server's bytes");
    std::fs::write(fetched.join(&name), &bytes).unwrap();

    let pin = pinned_to(&bytes);
    let program = unpack_pinned(&pin, "js-lease-server", &fetched, &into).unwrap();
    assert_eq!(std::fs::read(&program).unwrap(), b"the server's bytes");
    std::fs::remove_file(&program).unwrap();

    let mut changed = bytes.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    std::fs::write(fetched.join(&name), &changed).unwrap();
    let error = unpack_pinned(&pin, "js-lease-server", &fetched, &into).unwrap_err();
    assert!(error.contains("not the pinned"), "{error}");
    assert!(
        !program.exists(),
        "nothing is unpacked from a changed archive"
    );

    std::fs::write(fetched.join(&name), &bytes).unwrap();
    let mut off_by_one = pin.clone();
    for row in &mut off_by_one.archives {
        row.sha256.0[0] ^= 1;
    }
    let error = unpack_pinned(&off_by_one, "js-lease-server", &fetched, &into).unwrap_err();
    assert!(error.contains("not the pinned"), "{error}");
    assert!(!program.exists());

    std::fs::remove_file(fetched.join(&name)).unwrap();
    let error = unpack_pinned(&pin, "js-lease-server", &fetched, &into).unwrap_err();
    assert!(
        error.contains(
            "cargo xtask fetch-pinned-js --pin kbf-js-version.toml --program js-lease-server"
        ),
        "{error}"
    );

    let error = unpack_pinned(&pin, "js-other", &fetched, &into).unwrap_err();
    assert!(
        error.starts_with("kbf-js-version.toml: no [[archive]] row for js-other"),
        "{error}"
    );
}

/// Asserts the field sets the comparison is made of, on messages built here: a queued job's
/// fields by path (nested messages walked into, a map named once, a repeated field with `[]`,
/// an `optional` field present at 0), and that moving a value to another field, or leaving
/// one out, changes the set. Catches a reader that names fields by the wrong number, stops at
/// the top message, or counts a field the wire does not carry.
#[test]
fn the_field_set_names_each_field_on_the_wire() {
    let schema = Schema::job_service();
    let name = schema.message("Job");
    let job = Job {
        id: "j".to_owned(),
        spec: Some(as_read(spec("j"))),
        state: JobState::Queued.into(),
        times: Some(js_proto::v1::JobTimes {
            queued_at_ms: 1,
            placed_at_ms: Some(0),
            ..Default::default()
        }),
        ..Job::default()
    };
    let fields = schema.fields(&name, &job);
    let expected: BTreeSet<String> = [
        "id",
        "spec",
        "spec.job_id",
        "spec.image",
        "spec.command",
        "spec.args[]",
        "spec.env{}",
        "spec.needs",
        "spec.needs.cpu_millis",
        "spec.needs.memory_bytes",
        "spec.constraints",
        "spec.constraints.os",
        "spec.wall_limit_seconds",
        "spec.network",
        "spec.files[]",
        "spec.files[].name",
        "spec.priority",
        "spec.labels{}",
        "state",
        "times",
        "times.queued_at_ms",
        "times.placed_at_ms",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(fields, expected);

    let mut moved = job.clone();
    moved.host_id = moved.id.clone();
    moved.id.clear();
    let moved = schema.fields(&name, &moved);
    assert_ne!(moved, fields);
    assert!(moved.contains("host_id") && !moved.contains("id"));

    let mut unknown = job.encode_to_vec();
    unknown.extend_from_slice(&[0xf8, 0x07, 0x01]); // field 127, varint 1
    assert!(schema.fields_of(&name, &unknown).contains("#127"));
}

/// Asserts that a fake whose answer differs from the service's in one field is caught by the
/// comparison: the fake's own answer to a submit, and the same answer with the host set as a
/// different service would set it, give different answers. Catches a comparison that looks at
/// the error class only, or at the top message only.
#[tokio::test(flavor = "multi_thread")]
async fn the_comparison_names_a_field_the_fake_sets_differently() {
    let fake = fake_like_the_pool().await;
    let client = fake.client().unwrap();
    let schema = Schema::job_service();
    let name = schema.message("Job");
    let job = client.submit_job(spec("compare-1")).await.unwrap();
    let mut drifted = job.clone();
    if let Some(needs) = drifted.spec.as_mut().and_then(|s| s.needs.as_mut()) {
        needs.disk_bytes = Some(0);
    }
    let ours = answer(&schema, &name, Ok::<_, Error>(job));
    let theirs = answer(&schema, &name, Ok::<_, Error>(drifted));
    assert_ne!(ours, theirs);
    let Answer::Fields(theirs) = theirs else {
        panic!("fields")
    };
    assert!(theirs.contains("spec.needs.disk_bytes"));
    let refused = answer::<Job>(
        &schema,
        &name,
        client
            .submit_job(JobSpec {
                command: "/bin/other".to_owned(),
                ..spec("compare-1")
            })
            .await,
    );
    assert!(
        matches!(refused, Answer::Refused(RefusalCode::JobIdConflict, _)),
        "{refused:?}"
    );
    let missing = answer::<Job>(&schema, &name, client.get_job("compare-2").await);
    assert_eq!(missing, Answer::Status(tonic::Code::NotFound));
}
