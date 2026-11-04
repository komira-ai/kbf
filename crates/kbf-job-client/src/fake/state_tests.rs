use js_proto::refusal::refusal_of;
use js_proto::v1::{Constraints, JobFile, Needs};
use tonic::Code;

use super::*;

/// A change made to a job spec.
type Change = fn(&mut JobSpec);

const IMAGE: &str =
    "registry.example/app@sha256:0000000000000000000000000000000000000000000000000000000000000000";
const GIB: u64 = 1 << 30;

fn machine() -> Machine {
    Machine {
        shape: Shape {
            cpu_millis: 8000,
            memory_bytes: 32 * GIB,
            gpus: 1,
            gpu_class: "nvidia-a100".to_owned(),
            disk_bytes: Some(100 * GIB),
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    }
}

fn spec(id: &str) -> JobSpec {
    JobSpec {
        job_id: id.to_owned(),
        image: IMAGE.to_owned(),
        wall_limit_seconds: Some(600),
        needs: Some(Needs {
            cpu_millis: 1000,
            memory_bytes: GIB,
            ..Needs::default()
        }),
        ..JobSpec::default()
    }
}

fn state() -> State {
    let mut state = State::new();
    state.add_host("h", machine());
    state
}

/// The refusal code of a submit's answer, or `None` for an answer that is no refusal.
fn refusal(answer: Result<Job, Status>) -> Option<RefusalCode> {
    answer
        .err()
        .and_then(|status| refusal_of(&status))
        .map(|r| r.code())
}

/// Asserts each rule of fit on its own: a job is refused `no-capable-host` when one need or
/// constraint alone is past the only host (CPU, memory, disk, disk on a host that reports
/// none, GPUs, GPU class, OS, architecture, exec mode), and admitted when each is at the
/// host's own value or left empty. Catches a rule dropped from the fit, compared the wrong
/// way, or an empty constraint that matches nothing.
#[test]
fn each_rule_of_fit_refuses_on_its_own() {
    let at_edge = JobSpec {
        needs: Some(Needs {
            cpu_millis: 8000,
            memory_bytes: 32 * GIB,
            disk_bytes: Some(100 * GIB),
            gpus: 1,
            gpu_class: "nvidia-a100".to_owned(),
            whole_host: true,
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container.into(),
        }),
        ..spec("edge")
    };
    assert!(state().submit(at_edge.clone()).is_ok());
    let past: [(&str, Change); 8] = [
        ("cpu", |s| s.needs.as_mut().unwrap().cpu_millis += 1),
        ("memory", |s| s.needs.as_mut().unwrap().memory_bytes += 1),
        ("disk", |s| {
            s.needs.as_mut().unwrap().disk_bytes = Some(100 * GIB + 1)
        }),
        ("gpus", |s| s.needs.as_mut().unwrap().gpus = 2),
        ("class", |s| {
            s.needs.as_mut().unwrap().gpu_class = "other".to_owned()
        }),
        ("os", |s| {
            s.constraints.as_mut().unwrap().os = "macos".to_owned()
        }),
        ("arch", |s| {
            s.constraints.as_mut().unwrap().arch = "arm64".to_owned()
        }),
        ("exec", |s| {
            s.constraints.as_mut().unwrap().exec = ExecMode::SandboxExec.into()
        }),
    ];
    for (rule, change) in past {
        let mut job = at_edge.clone();
        change(&mut job);
        assert_eq!(
            refusal(state().submit(job)),
            Some(RefusalCode::NoCapableHost),
            "{rule}"
        );
    }
    let mut no_disk = State::new();
    let mut diskless = machine();
    diskless.shape.disk_bytes = None;
    no_disk.add_host("h", diskless);
    assert_eq!(
        refusal(no_disk.submit(at_edge)),
        Some(RefusalCode::NoCapableHost)
    );
    assert!(no_disk.submit(spec("no-disk-asked")).is_ok());
}

/// Asserts what a submit checks before it records anything, in the design's order: an id
/// the service would not take and an image without a digest are `INVALID_ARGUMENT` without
/// a refusal; a wall limit absent or 0 is `wall-time-not-positive`; one at the ceiling is
/// taken and one past it is `wall-time-above-ceiling`; with no host and no offer, the job is
/// `no-capable-host`, and an offer that fits admits it. Nothing refused is recorded.
/// Catches a refusal that records the job, and a ceiling check off by one.
#[test]
fn a_submit_is_checked_before_it_is_recorded() {
    let mut state = state();
    for bad in [
        JobSpec {
            job_id: "a/b".to_owned(),
            ..spec("")
        },
        JobSpec {
            image: "registry.example/app:latest".to_owned(),
            ..spec("tagged")
        },
    ] {
        let status = state.submit(bad).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(refusal_of(&status).is_none());
    }
    for wall in [None, Some(0)] {
        let job = JobSpec {
            wall_limit_seconds: wall,
            ..spec("wall")
        };
        assert_eq!(
            refusal(state.submit(job)),
            Some(RefusalCode::WallTimeNotPositive)
        );
    }
    state.set_wall_ceiling_s(3600);
    let at = JobSpec {
        wall_limit_seconds: Some(3600),
        ..spec("at-ceiling")
    };
    assert!(state.submit(at).is_ok());
    let past = JobSpec {
        wall_limit_seconds: Some(3601),
        ..spec("past-ceiling")
    };
    assert_eq!(
        refusal(state.submit(past)),
        Some(RefusalCode::WallTimeAboveCeiling)
    );
    let ids: Vec<String> = state.jobs().into_iter().map(|job| job.id).collect();
    assert_eq!(ids, ["at-ceiling"]);

    let mut empty = State::new();
    assert_eq!(
        refusal(empty.submit(spec("x"))),
        Some(RefusalCode::NoCapableHost)
    );
    empty.add_offer(machine());
    assert!(empty.submit(spec("x")).is_ok());
    assert!(empty.jobs()[0].host_id.is_empty(), "an offer holds no job");
}

/// Asserts that a job without an id gets `j-<counter>`, that a counter value a client has
/// already taken as its own id is passed over, and that two submits without an id make two
/// jobs. Catches a minted id that overwrites a job.
#[test]
fn a_job_without_an_id_gets_a_free_one() {
    let mut state = state();
    state.submit(spec("j-1")).unwrap();
    assert_eq!(state.submit(spec("")).unwrap().id, "j-2");
    assert_eq!(state.submit(spec("")).unwrap().id, "j-3");
    assert_eq!(state.jobs().len(), 3);
    assert_eq!(
        state.submitted("j-2").unwrap().job_id,
        "",
        "kept as submitted"
    );
}

/// Asserts that each step a host takes refuses a job not in the state it needs, a job the
/// fake does not have, and (for placing) a host it does not have, and changes nothing then.
/// Catches a step that moves a job out of order, such as running a job never started.
#[test]
fn each_step_needs_its_state() {
    let mut state = state();
    state.submit(spec("a")).unwrap();
    assert!(!state.place("a", "nowhere"));
    assert!(!state.place("missing", "h"));
    assert!(!state.start("a"), "not placed");
    assert!(!state.run("a"), "not starting");
    assert!(!state.write_logs("a", b"x", b""), "not running");
    assert!(!state.finish("a", 0), "not running");
    assert!(!state.lose("a"), "on no host");
    assert!(!state.host_stopped("a"), "on no host");
    assert!(state.place("a", "h"));
    assert!(!state.place("a", "h"), "placed already");
    assert!(!state.host_stopped("a"), "no cancel asked for");
    assert!(state.start("a"));
    assert!(state.run("a"));
    assert!(
        !state.fail_to_start("a", StartFailureCode::StartTimeout),
        "running"
    );
    assert!(state.finish("a", 3));
    let job = state.get("a").unwrap();
    assert_eq!(job.state(), JobState::Failed);
    assert!(!state.lose("a"), "ended");
    assert!(!state.silence_host("nowhere", true));
    assert!(!state.silence_host("nowhere", false));
}

/// Asserts the end each step gives: exit 0 succeeds, another code fails, both with reason
/// exit and the code; a job failing to start keeps its code and has no exit code; a lost job
/// keeps a cancel that was asked for, and a finished one does not. Catches a result whose
/// state and reason disagree.
#[test]
fn each_end_has_its_state_and_reason() {
    let mut state = state();
    for (id, exit, end) in [("ok", 0, JobState::Succeeded), ("bad", 2, JobState::Failed)] {
        state.submit(spec(id)).unwrap();
        assert!(state.place(id, "h") && state.start(id) && state.run(id));
        state.cancel(id).unwrap();
        assert!(state.finish(id, exit));
        let result = state.result(id).unwrap();
        assert_eq!(
            (result.state(), result.exit_code, result.reason()),
            (end, Some(exit), ResultReason::Exit)
        );
        assert!(!state.get(id).unwrap().cancel_requested);
    }
    state.submit(spec("never")).unwrap();
    let queued = state.result("never").unwrap_err();
    assert_eq!(
        queued.code(),
        Code::FailedPrecondition,
        "a queued job has no result"
    );
    assert!(state.fail_to_start("never", StartFailureCode::NoCapacity));
    let job = state.get("never").unwrap();
    assert_eq!(job.start_failure(), StartFailureCode::NoCapacity);
    let result = state.result("never").unwrap();
    assert_eq!(
        (result.state(), result.exit_code, result.reason()),
        (JobState::FailedToStart, None, ResultReason::Unspecified)
    );
    state.submit(spec("gone")).unwrap();
    assert!(state.place("gone", "h"));
    state.cancel("gone").unwrap();
    assert!(state.lose("gone"));
    let job = state.get("gone").unwrap();
    assert_eq!(job.state(), JobState::Lost);
    assert!(
        job.cancel_requested,
        "a lost job keeps the cancel it was asked"
    );
}

/// Asserts the tail of each stream is its last 8 KiB: 8192 bytes are kept whole and not
/// marked cut; one byte more drops the first byte and marks the tail cut, on either stream.
/// Catches a cap off by one and a tail that keeps the head instead of the end.
#[test]
fn a_tail_keeps_the_last_8_kib() {
    let mut state = state();
    for id in ["out", "err"] {
        state.submit(spec(id)).unwrap();
        assert!(state.place(id, "h") && state.start(id) && state.run(id));
    }
    assert!(state.write_logs("out", &[b'a'; TAIL], b""));
    let logs = state.logs("out").unwrap();
    assert_eq!((logs.stdout_tail.len(), logs.truncated), (TAIL, false));
    assert!(state.write_logs("out", b"z", b""));
    let logs = state.logs("out").unwrap();
    assert_eq!((logs.stdout_tail.len(), logs.truncated), (TAIL, true));
    assert_eq!(logs.stdout_tail.last(), Some(&b'z'));
    assert!(state.write_logs("err", b"", &[b'e'; TAIL + 1]));
    let logs = state.logs("err").unwrap();
    assert_eq!((logs.stderr_tail.len(), logs.truncated), (TAIL, true));
}

/// Asserts a listing's page token: a token the service did not write is
/// `INVALID_ARGUMENT`; a page size of 0 is the default of 100; the pages follow the order the
/// jobs were recorded in, not their ids. Catches a listing ordered by id, which a job
/// recorded during the listing could reorder.
#[test]
fn a_listing_follows_the_order_of_recording() {
    let mut state = state();
    let token = ListJobsRequest {
        page_token: "not-a-token".to_owned(),
        ..ListJobsRequest::default()
    };
    assert_eq!(
        state.list(&token).unwrap_err().code(),
        Code::InvalidArgument
    );
    for n in (0..=DEFAULT_PAGE).rev() {
        state.submit(spec(&format!("job-{n:03}"))).unwrap();
    }
    let first = state.list(&ListJobsRequest::default()).unwrap();
    assert_eq!(first.jobs.len(), DEFAULT_PAGE);
    assert_eq!(first.jobs[0].id, "job-100");
    assert_eq!(first.next_page_token, DEFAULT_PAGE.to_string());
    let rest = ListJobsRequest {
        page_token: first.next_page_token,
        ..ListJobsRequest::default()
    };
    let last = state.list(&rest).unwrap();
    let ids: Vec<&str> = last.jobs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!((ids, last.next_page_token.as_str()), (vec!["job-000"], ""));
}

/// Asserts the clock: silence is stamped at the time it began and kept when the host is
/// silenced again; a retention window shortened after a job ended forgets it at once; and
/// the clock saturates instead of wrapping. Catches a silence that moves forward on every
/// call and a retention read only at the end of a job.
#[test]
fn the_clock_moves_only_when_told() {
    let mut state = state();
    state.submit(spec("a")).unwrap();
    assert!(state.place("a", "h"));
    state.advance(5);
    assert!(state.silence_host("h", true));
    state.advance(5);
    assert!(state.silence_host("h", true));
    assert_eq!(state.get("a").unwrap().host_silent_since_ms, Some(5));
    assert!(state.lose("a"));
    assert_eq!(state.get("a").unwrap().host_silent_since_ms, None, "ended");
    state.advance(1000);
    state.set_retention_ms(1000);
    assert_eq!(state.get("a").unwrap_err().code(), Code::NotFound);
    assert!(state.submitted("a").is_none());
    state.advance(u64::MAX);
    assert_eq!(state.now_ms(), u64::MAX);
}

/// Asserts that a read never answers file contents while the fake keeps them: the job a
/// submit, a read and a listing answer has each file with its name and no contents, and
/// `submitted` has them. Catches the fake echoing a presigned URL.
#[test]
fn file_contents_are_kept_and_never_read_back() {
    let mut state = state();
    let job = JobSpec {
        files: vec![JobFile {
            name: "upload-url".to_owned(),
            contents: "https://bucket.example/put?sig=secret".to_owned(),
        }],
        ..spec("f")
    };
    let answered = state.submit(job).unwrap();
    let read = state.get("f").unwrap();
    let listed = state.list(&ListJobsRequest::default()).unwrap().jobs;
    for job in [answered, read, listed[0].clone()] {
        let files = job.spec.unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(
            (files[0].name.as_str(), files[0].contents.as_str()),
            ("upload-url", "")
        );
    }
    let kept = state.submitted("f").unwrap();
    assert_eq!(
        kept.files[0].contents,
        "https://bucket.example/put?sig=secret"
    );
}
