//! The Execute join end to end, in one process: a REAPI client, the server, its executor and a
//! fake job service. Each test drives the wire on one side and the job's life on the other.
//! `execute_direct.rs` holds the first Execute tests of the executor (a miss runs as one job and
//! is cached, a nonzero exit is not cached, a lost job runs again, a wall limit answers at
//! once, no executor refuses).

#[path = "execute/failure_log.rs"]
mod failure_log;
#[path = "execute/get_operation.rs"]
mod get_operation;
#[path = "execute/invocations.rs"]
mod invocations;
#[path = "execute/log.rs"]
mod log;
#[path = "execute/slow_store.rs"]
mod slow_store;
mod support;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{ExecuteResponse, WaitExecutionRequest};
use prost::Message;
use support::{Blob, Cell, Client, Job, Ran, done, done_within_quiet, response, stage};
use tonic::{Code, Streaming};

/// The name of the first operation `ops` streams.
async fn first_name(ops: &mut Streaming<Operation>) -> String {
    ops.message()
        .await
        .expect("a stream")
        .expect("an update")
        .name
}

/// WaitExecution on `name`: its stream, or the code.
async fn wait_stream(cell: &Client, name: &str) -> Result<Streaming<Operation>, Code> {
    let ops = cell
        .exec()
        .wait_execution(WaitExecutionRequest {
            name: name.to_owned(),
        })
        .await
        .map_err(|s| s.code())?;
    Ok(ops.into_inner())
}

/// WaitExecution on `name`: the name of the first operation it streams, or the code.
async fn wait_name(cell: &Client, name: &str) -> Result<String, Code> {
    let mut ops = wait_stream(cell, name).await?;
    Ok(ops
        .message()
        .await
        .expect("a healthy stream")
        .expect("an update")
        .name)
}

/// Reads `ops` until an update at `want`, which it returns.
async fn until_stage(ops: &mut Streaming<Operation>, want: ExecStage) -> Operation {
    loop {
        let op = tokio::time::timeout(support::PROMPT, ops.message())
            .await
            .expect("an update in time")
            .expect("a healthy stream")
            .expect("the stream ended before the stage");
        if stage(&op) == want as i32 {
            return op;
        }
        assert!(!op.done, "the operation finished before {want:?}");
    }
}

/// Asserts the stages of an operation: queued until its job is running, executing once the
/// executor reads the job running, then completed with the action's result, which is in the
/// action cache only once the operation is done; and that the result is the pack's, not
/// marked as cached.
#[tokio::test]
async fn an_operation_streams_queued_then_executing_then_completed() {
    let cell = Cell::start().await;
    let job = Job::new("build", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "the build output"));

    let mut ops = cell.execute(&job.action).await;
    let first = ops.message().await.expect("a stream").expect("an update");
    assert!(!first.done);
    assert_eq!(stage(&first), ExecStage::Queued as i32, "no job runs yet");
    let id = cell.direct.run(&job.action).await;
    until_stage(&mut ops, ExecStage::Executing).await;
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));

    assert!(cell.direct.fake.finish(&id, 0));
    let last = done(&mut ops).await;
    assert_eq!(stage(&last), ExecStage::Completed as i32);
    let answer = response(&last);
    let result = answer.result.expect("a result");
    assert_eq!(result.output_files[0].path, "out");
    assert!(!answer.cached_result);
    assert_eq!(answer.status.map(|s| s.code), Some(Code::Ok as i32));
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}

/// Asserts that a cached action is answered with no job (`cached_result` set, and the stream
/// ends with that one update), and that `skip_cache_lookup` runs it again as a new operation
/// whose answer is not marked as cached.
#[tokio::test]
async fn a_cached_action_is_answered_without_a_job_and_skip_cache_lookup_runs_it_again() {
    let cell = Cell::start().await;
    let job = Job::new("cached", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "built once"));
    cell.direct
        .will_run(&job.action, Ran::writes("out", "built again"));
    let mut first = cell.execute(&job.action).await;
    cell.direct.complete(&job.action).await;
    let built = response(&done(&mut first).await).result.expect("a result");

    let mut ops = cell.execute(&job.action).await;
    let only = ops.message().await.expect("a stream").expect("an update");
    let answer = response(&only);
    assert!(answer.cached_result);
    assert_eq!(answer.result.map(support::unstamped), Some(built));
    assert_eq!(
        ops.message().await.expect("a stream"),
        None,
        "more after done"
    );
    assert_eq!(cell.direct.submitted_jobs(), 1, "the hit ran nothing");

    let first_job = cell.direct.jobs_of(&job.action)[0].id.clone();
    let mut again = cell.execute_with(&job.action, true).await.expect("Execute");
    let fresh = job_other_than(&cell, &job, &first_job).await;
    cell.direct.run_job(&fresh);
    assert!(cell.direct.fake.finish(&fresh, 0));
    let answer = response(&done(&mut again).await);
    assert!(!answer.cached_result);
    assert_eq!(
        answer.result.expect("a result").output_files[0].digest,
        Some(Blob::new("built again").proto)
    );
}

/// The id of a job of `job` other than `known`, once the executor has submitted it.
async fn job_other_than(cell: &Cell, job: &Job, known: &str) -> String {
    let found = async {
        loop {
            let jobs = cell.direct.jobs_of(&job.action);
            if let Some(other) = jobs.iter().find(|j| j.id != known) {
                return other.id.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(support::PROMPT, found)
        .await
        .expect("a second job is submitted")
}

/// Asserts that identical Executes that overlap run one job: the second joins the open
/// operation (it is shown executing at once, under the same name) and both callers are answered
/// with the one result.
#[tokio::test]
async fn identical_concurrent_executes_run_one_job() {
    let cell = Cell::start().await;
    let job = Job::new("shared", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "built for both"));

    let mut one = cell.execute(&job.action).await;
    let id = cell.direct.run(&job.action).await;
    let mut two = cell.execute(&job.action).await;
    assert_eq!(first_name(&mut one).await, first_name(&mut two).await);
    assert!(cell.direct.fake.finish(&id, 0));

    let (a, b) = (
        response(&done(&mut one).await).result.expect("a result"),
        response(&done(&mut two).await).result.expect("a result"),
    );
    assert_eq!(a, b);
    assert_eq!(a.output_files[0].path, "out");
    assert_eq!(cell.direct.jobs_of(&job.action).len(), 1, "one job");
}

/// Ends attempt `attempt` of `job` the way `ending` says, the job having been placed and run.
async fn end_attempt(cell: &Cell, job: &Job, attempt: u32, ending: &str) {
    let id = cell.direct.job(&job.action, attempt).await;
    cell.direct.run_job(&id);
    match ending {
        "exit" => assert!(cell.direct.fake.finish(&id, 7)),
        "lost" => assert!(cell.direct.fake.lose(&id)),
        other => panic!("no ending named {other}"),
    }
}

/// Asserts the answers of the endings of a job that is not a result, which the table in
/// `executor/outcome.rs` names: the task tool exiting nonzero and a lost host are the farm's, so
/// each is run again as the next attempt and, with all three attempts used, answered UNAVAILABLE;
/// a host that refuses a file of the job is the action's own fault, so it is answered
/// INVALID_ARGUMENT at once, with one job. None is cached, and none answers with a result.
#[tokio::test]
async fn job_endings_that_are_not_a_result_answer_with_the_documented_codes() {
    let cell = Cell::start().await;
    let cases = [
        ("the task tool exits nonzero at every attempt", "exit", 3),
        ("the host is lost at every attempt", "lost", 3),
        ("the host refuses a file of the job", "refused", 1),
    ];
    for (name, ending, jobs) in cases {
        let job = Job::new(name, &[]);
        cell.upload(&job.blobs()).await;
        let mut ops = cell.execute(&job.action).await;
        if ending == "refused" {
            let id = cell.direct.job(&job.action, 0).await;
            assert!(cell.direct.fake.place(&id, support::HOST));
            assert!(cell.direct.fake.refuse_start(&id, "invalid-file"));
        } else {
            for attempt in 0..jobs {
                end_attempt(&cell, &job, attempt, ending).await;
            }
        }
        let answer = response(&done(&mut ops).await);
        let status = answer.status.expect("a status");
        let want = if ending == "refused" {
            Code::InvalidArgument
        } else {
            Code::Unavailable
        };
        assert_eq!(status.code, want as i32, "{name}: {status:?}");
        assert_eq!(answer.result, None, "{name}");
        assert_eq!(
            cell.direct.jobs_of(&job.action).len(),
            jobs as usize,
            "{name}"
        );
        assert_eq!(
            cell.cached(&job.action).await,
            Err(Code::NotFound),
            "{name}"
        );
    }
}

/// Asserts that the output of a job that ended badly reaches the caller with the status and
/// not as a result: the end of its standard output and error ride in an `ErrorInfo` detail of
/// the status (`JOB_OUTPUT_TAIL`), the status code is the ending's, nothing is cached, and
/// there is no result.
#[tokio::test]
async fn a_job_that_ended_badly_answers_with_its_output_in_the_status() {
    let cell = Cell::start().await;
    let job = Job::new("writes then dies", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    for attempt in 0..3 {
        let id = cell.direct.job(&job.action, attempt).await;
        cell.direct.run_job(&id);
        assert!(
            cell.direct
                .fake
                .write_logs(&id, b"started\n", b"warming up\n")
        );
        assert!(cell.direct.fake.finish(&id, 7));
    }

    let answer: ExecuteResponse = response(&done(&mut ops).await);
    let status = answer.status.expect("a status");
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(answer.result, None);
    let info = status
        .details
        .iter()
        .find(|any| any.type_url.ends_with("google.rpc.ErrorInfo"))
        .map(|any| ErrorInfo::decode(any.value.as_slice()).expect("an ErrorInfo"))
        .expect("the output rides in the status");
    assert_eq!(info.reason, "JOB_OUTPUT_TAIL");
    assert_eq!(info.metadata["stdout"], "started\n");
    assert_eq!(info.metadata["stderr"], "warming up\n");
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

/// Asserts that callers are answered only after the accepted result's action-cache entry is
/// written, so a client that looks the action up as soon as its Execute finishes finds the
/// result. The write is held at a gate: no answer arrives while it is held, and once it is let
/// through the answer and the entry are both there.
#[tokio::test]
async fn the_cache_entry_is_written_before_the_callers_are_answered() {
    let cell = Cell::start().await;
    let job = Job::new("written first", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "in the cache first"));
    let mut ops = cell.execute(&job.action).await;

    let gate = &cell.cache.meta().action_write;
    gate.close_next();
    cell.direct.complete(&job.action).await;
    gate.held().await;
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered before the action-cache write"
    );
    gate.open();
    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}

/// Asserts that a pack whose action-cache write the store refuses is not an answer: the pack
/// could not be taken in, so the operation runs again as the next attempt, whose pack is taken
/// in and answers the callers.
#[tokio::test]
async fn a_pack_the_store_could_not_take_in_runs_again_as_the_next_attempt() {
    let cell = Cell::start().await;
    let job = Job::new("unwritten", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "first pack"));
    cell.direct
        .will_run(&job.action, Ran::writes("out", "second pack"));
    let mut ops = cell.execute(&job.action).await;

    cell.cache.meta().fail_next_action_write();
    cell.direct.complete(&job.action).await;
    let second = cell.direct.job(&job.action, 1).await;
    cell.direct.run_job(&second);
    assert!(cell.direct.fake.finish(&second, 0));

    let result = response(&done(&mut ops).await).result.expect("a result");
    assert_eq!(
        result.output_files[0].digest,
        Some(Blob::new("second pack").proto)
    );
    assert_eq!(cell.direct.ingestions().len(), 2);
    assert_eq!(cell.cached(&job.action).await, Ok(result));
}

/// Asserts that WaitExecution finds a running operation by the name Execute gave and delivers
/// its result under the same name.
#[tokio::test]
async fn wait_execution_follows_a_running_operation() {
    let cell = Cell::start().await;
    let job = Job::new("waited", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "for the waiter"));
    let mut ops = cell.execute(&job.action).await;
    let name = first_name(&mut ops).await;
    let mut waited = wait_stream(&cell, &name).await.expect("WaitExecution");

    cell.direct.complete(&job.action).await;
    let last = done(&mut waited).await;
    assert_eq!(last.name, name);
    let result = response(&last).result.expect("a result");
    assert_eq!(result.output_files[0].path, "out");
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}

/// Asserts that a finished operation is waited on within the retention (WaitExecution streams
/// the done operation with its result, once, for an action whose result the cache does not keep)
/// and not after: with no retention a finished operation is forgotten at once and its name is
/// NOT_FOUND.
#[tokio::test]
async fn a_finished_operation_is_waited_on_within_the_retention_and_forgotten_with_none() {
    for retention in [None, Some(std::time::Duration::ZERO)] {
        let cell = match retention {
            None => Cell::start().await,
            Some(none) => Cell::start_with_retention(none).await,
        };
        let job = Job::new("exits 1", &[]);
        cell.upload(&job.blobs()).await;
        cell.direct.will_run(&job.action, Ran::exits(1));
        let mut ops = cell.execute(&job.action).await;
        cell.direct.complete(&job.action).await;
        let finished = done(&mut ops).await;
        let result = response(&finished).result.expect("a result");
        assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));

        match retention {
            None => {
                let mut waited = wait_stream(&cell, &finished.name).await.expect("kept");
                let first = waited
                    .message()
                    .await
                    .expect("a stream")
                    .expect("an update");
                assert_eq!(first.name, finished.name);
                assert_eq!(response(&first).result, Some(result));
                assert!(
                    waited.message().await.expect("a stream").is_none(),
                    "more after done"
                );
            }
            Some(_) => assert_eq!(
                wait_name(&cell, &finished.name).await,
                Err(Code::NotFound),
                "forgotten at once"
            ),
        }
    }
}

/// Asserts that forgetting one finished operation keeps the others: three operations run at
/// once, the middle one finishes and, with no retention, is forgotten. The operations numbered
/// below and above it are still found by WaitExecution, and both their Execute and WaitExecution
/// streams get their results.
#[tokio::test]
async fn forgetting_one_operation_keeps_the_lower_and_higher_ones() {
    let cell = Cell::start_with_retention(std::time::Duration::ZERO).await;
    let jobs = ["lower", "middle", "higher"].map(|argv| Job::new(argv, &[]));
    let mut ops = Vec::new();
    let mut names = Vec::new();
    for job in &jobs {
        cell.upload(&job.blobs()).await;
        cell.direct
            .will_run(&job.action, Ran::writes(&job.input.proto.hash[..8], "out"));
        let mut stream = cell.execute(&job.action).await;
        names.push(first_name(&mut stream).await);
        ops.push(stream);
    }
    let mut ids = Vec::new();
    for job in &jobs {
        ids.push(cell.direct.run(&job.action).await);
    }

    assert!(cell.direct.fake.finish(&ids[1], 0));
    let middle = response(&done(&mut ops[1]).await).result.expect("a result");
    assert_eq!(middle.output_files.len(), 1);
    assert_eq!(wait_name(&cell, &names[1]).await, Err(Code::NotFound));

    for kept in [0, 2] {
        let mut waited = wait_stream(&cell, &names[kept]).await.expect("still kept");
        assert!(cell.direct.fake.finish(&ids[kept], 0));
        let through_execute = response(&done(&mut ops[kept]).await).result;
        let through_wait = response(&done(&mut waited).await).result;
        assert!(through_execute.is_some());
        assert_eq!(through_execute, through_wait);
    }
}

/// Asserts that WaitExecution answers the exact spelling Execute gave and no other: a name with
/// a leading zero, a sign, a space or a cached result's spelling is NOT_FOUND, so a client's
/// name from another spelling never attaches it to the operation that has the number.
#[tokio::test]
async fn wait_execution_refuses_a_name_spelled_other_than_the_one_execute_gave() {
    let cell = Cell::start().await;
    let job = Job::new("a name", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let name = first_name(&mut ops).await;
    cell.direct.run(&job.action).await;
    let number = name.strip_prefix("operations/").expect("a stored name");
    assert!(number.bytes().all(|b| b.is_ascii_digit()), "{name}");

    for other in [
        format!("operations/0{number}"),
        format!("operations/+{number}"),
        format!("operations/{number} "),
        "operations/".to_owned(),
        format!("operations/cached/{number}"),
        number.to_owned(),
    ] {
        assert_eq!(
            wait_name(&cell, &other).await,
            Err(Code::NotFound),
            "{other:?}"
        );
    }
    assert_eq!(wait_name(&cell, &name).await, Ok(name));
}

/// Asserts that an Execute whose inputs are not in the CAS is refused FAILED_PRECONDITION
/// naming each missing blob, and runs no job: the client uploads them and executes again.
#[tokio::test]
async fn an_execute_with_a_missing_input_is_refused_naming_the_blob_and_runs_no_job() {
    let cell = Cell::start().await;
    let job = Job::new("its input is not stored", &[]);
    cell.upload(&[&job.action, &job.command]).await;

    let refused = cell
        .execute_with(&job.action, false)
        .await
        .expect_err("the Execute is refused");

    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
    let status = kbf_proto::google::rpc::Status::decode(refused.details()).expect("rpc.Status");
    let detail = status
        .details
        .iter()
        .find(|any| any.type_url.ends_with("google.rpc.PreconditionFailure"))
        .map(|any| {
            kbf_proto::google::rpc::PreconditionFailure::decode(any.value.as_slice())
                .expect("a PreconditionFailure")
        })
        .expect("the missing blobs are listed");
    let subjects: Vec<_> = detail
        .violations
        .iter()
        .map(|v| v.subject.clone())
        .collect();
    assert_eq!(
        subjects,
        [format!(
            "blobs/{}/{}",
            job.root.digest.hash_hex(),
            job.root.digest.size_bytes
        )]
    );
    assert!(detail.violations.iter().all(|v| v.r#type == "MISSING"));
    assert_eq!(cell.direct.submitted_jobs(), 0);
}

/// Asserts that the platform's `kbf-lease` is read: an action that asks for a whole machine
/// cannot commit into a put directory (the job service gives a whole-host job no mount), so it
/// is answered INVALID_ARGUMENT, naming the lease, with no job made; one that does not is
/// submitted as a job that does not need the whole host.
#[tokio::test]
async fn a_whole_machine_action_is_refused_and_one_that_is_not_needs_no_whole_host() {
    let cell = Cell::start().await;
    let whole = Job::new("whole", &[("kbf-lease", "whole_machine")]);
    let shared = Job::new("shared", &[]);
    cell.upload(&whole.blobs()).await;
    cell.upload(&shared.blobs()).await;
    let mut whole_ops = cell.execute(&whole.action).await;
    let _shared_ops = cell.execute(&shared.action).await;

    let answer = response(&done(&mut whole_ops).await);
    let status = answer.status.expect("a status");
    assert_eq!(status.code, Code::InvalidArgument as i32, "{status:?}");
    assert!(status.message.contains("whole_machine"), "{status:?}");
    assert!(cell.direct.jobs_of(&whole.action).is_empty());

    let shared_job = cell.direct.job(&shared.action, 0).await;
    let needs = cell
        .direct
        .fake
        .submitted(&shared_job)
        .expect("a submitted job")
        .needs
        .map(|needs| needs.whole_host);
    assert!(!needs.unwrap_or(false));
}
