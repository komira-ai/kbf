use js_proto::refusal::refusal_of;
use js_proto::v1::{Constraints, JobFile, Needs, PortForward};
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
            ..Constraints::default()
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
    for id in ["exact", "out", "err"] {
        state.submit(spec(id)).unwrap();
        assert!(state.place(id, "h") && state.start(id) && state.run(id));
    }
    assert!(state.write_logs("exact", &[b'a'; TAIL], b""));
    assert!(state.write_logs("out", &[b'a'; TAIL], b""));
    assert!(state.write_logs("out", b"z", b""));
    assert!(state.write_logs("err", b"", &[b'e'; TAIL + 1]));
    // The tails show once the job has ended.
    for id in ["exact", "out", "err"] {
        assert!(state.finish(id, 0));
    }
    let logs = state.logs("exact").unwrap();
    assert_eq!((logs.stdout_tail.len(), logs.truncated), (TAIL, false));
    let logs = state.logs("out").unwrap();
    assert_eq!((logs.stdout_tail.len(), logs.truncated), (TAIL, true));
    assert_eq!(logs.stdout_tail.last(), Some(&b'z'));
    let logs = state.logs("err").unwrap();
    assert_eq!((logs.stderr_tail.len(), logs.truncated), (TAIL, true));
}

/// Asserts a listing's page token: a token the service did not write (a made-up word, or a
/// plausible `p999`) and a token given with other filters than the listing that issued it
/// are `INVALID_ARGUMENT`; a page size of 0 is the default of 100; the pages follow the order the
/// jobs were recorded in, not their ids. Catches a listing ordered by id, which a job
/// recorded during the listing could reorder, and a token accepted for any listing.
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
    let forged = ListJobsRequest {
        page_token: "p999".to_owned(),
        ..ListJobsRequest::default()
    };
    assert_eq!(
        state.list(&forged).unwrap_err().code(),
        Code::InvalidArgument
    );
    for n in (0..=DEFAULT_PAGE).rev() {
        state.submit(spec(&format!("job-{n:03}"))).unwrap();
    }
    let first = state.list(&ListJobsRequest::default()).unwrap();
    assert_eq!(first.jobs.len(), DEFAULT_PAGE);
    assert_eq!(first.jobs[0].id, "job-100");
    assert_eq!(first.next_page_token, "p1");
    let refiltered = ListJobsRequest {
        page_token: first.next_page_token.clone(),
        states: vec![JobState::Queued.into()],
        ..ListJobsRequest::default()
    };
    let status = state.list(&refiltered).unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
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

/// Asserts a job whose `constraints.exec` is a number the enum does not know (99, -1) reads
/// as Unspecified: a container host admits it exactly as it admits an explicit Unspecified,
/// and a container-only host still refuses `SandboxExec`. Catches an unknown exec value that
/// matches no host, or one that is read as a specific mode.
#[test]
fn an_unknown_exec_number_matches_any_host() {
    let with_exec = |id: &str, exec: i32| JobSpec {
        constraints: Some(Constraints {
            os: String::new(),
            arch: String::new(),
            exec,
            ..Constraints::default()
        }),
        ..spec(id)
    };
    let mut state = state();
    for (id, exec) in [
        ("unspecified", i32::from(ExecMode::Unspecified)),
        ("high", 99),
        ("negative", -1),
    ] {
        assert!(state.submit(with_exec(id, exec)).is_ok(), "{id}");
    }
    let sandbox = with_exec("sandbox", ExecMode::SandboxExec.into());
    assert_eq!(
        refusal(state.submit(sandbox)),
        Some(RefusalCode::NoCapableHost)
    );
    assert_eq!(state.jobs().len(), 3);
}

/// Asserts what a submit takes as an image, a file name and a port: an image without a
/// digest, without a name, with 63 hex digits, in upper case or not hex, a file name that
/// is empty, starts with a dot, holds a slash or is given twice, and a port of 0 or 65536
/// (host or container side) are each `INVALID_ARGUMENT` with no refusal details and record
/// nothing; ports 1 and 65535 and the name `a.b_c-1` are taken. Catches a pin check that
/// accepts a short or upper-case digest, and a port range off by one.
#[test]
fn a_submit_with_a_bad_image_file_or_port_is_invalid() {
    let file = |name: &str| JobFile {
        name: name.to_owned(),
        contents: String::new(),
    };
    let port = |host_port: u32, container_port: u32| PortForward {
        host_port,
        container_port,
    };
    let images = [
        "registry.example/app@sha256:".to_owned(),
        format!("@sha256:{}", "0".repeat(64)),
        format!("registry.example/app@sha256:{}", "0".repeat(63)),
        format!("registry.example/app@sha256:{}", "A".repeat(64)),
        format!("registry.example/app@sha256:{}", "g".repeat(64)),
    ];
    let mut bad: Vec<JobSpec> = images
        .into_iter()
        .map(|image| JobSpec {
            image,
            ..spec("bad")
        })
        .collect();
    for files in [
        vec![file("")],
        vec![file(".hidden")],
        vec![file("a/b")],
        vec![file("same"), file("same")],
    ] {
        bad.push(JobSpec {
            files,
            ..spec("bad")
        });
    }
    for forward in [port(0, 80), port(65_536, 80), port(80, 0), port(80, 65_536)] {
        bad.push(JobSpec {
            ports: vec![forward],
            ..spec("bad")
        });
    }
    let mut state = state();
    for job in bad {
        let status = state.submit(job.clone()).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{job:?}");
        assert!(refusal_of(&status).is_none(), "{job:?}");
        assert!(state.jobs().is_empty(), "{job:?}");
    }
    let edges = JobSpec {
        files: vec![file("a.b_c-1")],
        ports: vec![port(1, 65_535), port(65_535, 1)],
        ..spec("edges")
    };
    assert!(state.submit(edges).is_ok());
}

/// A step a host takes on the job `j`.
type Step = fn(&mut State) -> bool;

fn place(s: &mut State) -> bool {
    s.place("j", "h")
}
fn start(s: &mut State) -> bool {
    s.start("j")
}
fn run(s: &mut State) -> bool {
    s.run("j")
}
fn write_logs(s: &mut State) -> bool {
    s.write_logs("j", b"o", b"e")
}
fn finish(s: &mut State) -> bool {
    s.finish("j", 0)
}
fn finish_failing(s: &mut State) -> bool {
    s.finish("j", 1)
}
fn fail_to_start(s: &mut State) -> bool {
    s.fail_to_start("j", StartFailureCode::NoCapacity)
}
fn lose(s: &mut State) -> bool {
    s.lose("j")
}
fn host_stopped(s: &mut State) -> bool {
    s.host_stopped("j")
}

/// A state holding the job `j` in `want`, by the steps that reach it.
fn job_in(want: JobState) -> State {
    let mut s = state();
    s.submit(spec("j")).unwrap();
    let steps: &[Step] = match want {
        JobState::Queued => &[],
        JobState::Placed => &[place],
        JobState::Starting => &[place, start],
        JobState::Running => &[place, start, run],
        JobState::Succeeded => &[place, start, run, finish],
        JobState::Failed => &[place, start, run, finish_failing],
        JobState::Lost => &[place, lose],
        JobState::FailedToStart => &[fail_to_start],
        JobState::Cancelled => {
            s.cancel("j").unwrap();
            &[]
        }
        JobState::Unspecified => unreachable!("a job is never in no state"),
    };
    for step in steps {
        assert!(step(&mut s));
    }
    assert_eq!(s.get("j").unwrap().state(), want);
    s
}

/// Asserts every step against every state: `place` is taken from Queued; `start` from
/// Placed; `run` from Starting; `write_logs` and `finish` from Running; `fail_to_start` from
/// Queued, Placed and Starting; `lose` from Placed, Starting and Running; `host_stopped` from
/// those three only when a cancel was asked. Each step a state refuses answers false and
/// leaves the whole job and its logs as they were; each step on an unknown id is false.
/// Catches a step allowed from a state the design does not give it, and a refused step that
/// still changes the job.
#[test]
fn every_step_is_allowed_from_exactly_its_states() {
    use JobState::{Cancelled, Failed, FailedToStart, Lost, Placed, Queued, Running};
    use JobState::{Starting, Succeeded};
    let steps: [(&str, Step, &[JobState]); 9] = [
        ("place", place, &[Queued]),
        ("start", start, &[Placed]),
        ("run", run, &[Starting]),
        ("write_logs", write_logs, &[Running]),
        ("finish", finish, &[Running]),
        ("fail_to_start", fail_to_start, &[Queued, Placed, Starting]),
        ("lose", lose, &[Placed, Starting, Running]),
        ("host_stopped", host_stopped, &[]),
        (
            "host_stopped, cancel asked",
            host_stopped,
            &[Placed, Starting, Running],
        ),
    ];
    let every = [
        Queued,
        Placed,
        Starting,
        Running,
        Succeeded,
        Failed,
        Cancelled,
        Lost,
        FailedToStart,
    ];
    for (name, step, allowed) in steps {
        let asked = name.contains("cancel asked");
        for from in every {
            let mut s = job_in(from);
            if asked {
                s.cancel("j").unwrap();
            }
            let (job, logs) = (s.get("j").unwrap(), s.logs("j").unwrap());
            let taken = step(&mut s);
            assert_eq!(taken, allowed.contains(&from), "{name} from {from:?}");
            if !taken {
                assert_eq!(s.get("j").unwrap(), job, "{name} from {from:?}");
                assert_eq!(s.logs("j").unwrap(), logs, "{name} from {from:?}");
            }
        }
    }
    let mut s = state();
    assert!(!s.place("missing", "h"));
    assert!(!s.start("missing"));
    assert!(!s.run("missing"));
    assert!(!s.write_logs("missing", b"", b""));
    assert!(!s.finish("missing", 0));
    assert!(!s.fail_to_start("missing", StartFailureCode::NoCapacity));
    assert!(!s.lose("missing"));
    assert!(!s.host_stopped("missing"));
}

/// Asserts the network level the released service records: a job submitted without one is
/// answered, read and handed to its host with `none`; a repeat that names `none` is the same
/// job, and one that names `open` is `job-id-conflict`; a level given is kept; and a value the
/// enum does not have is kept as sent, not read as unspecified. Catches a fake that echoes an
/// unspecified level the service would answer as `none` (the real-service comparison in
/// kbf-integration-test's tests/js_real_service.rs), and one that takes `none` and unspecified
/// for two jobs.
#[test]
fn an_unspecified_network_level_is_recorded_as_none() {
    let mut state = state();
    let job = state.submit(spec("quiet")).unwrap();
    let none = i32::from(NetworkLevel::None);
    assert_eq!(job.spec.unwrap().network, none);
    assert_eq!(state.get("quiet").unwrap().spec.unwrap().network, none);
    assert_eq!(state.submitted("quiet").unwrap().network, none);

    let mut named = spec("quiet");
    named.set_network(NetworkLevel::None);
    assert_eq!(state.submit(named).unwrap(), state.get("quiet").unwrap());
    let mut open = spec("quiet");
    open.set_network(NetworkLevel::Open);
    assert_eq!(
        refusal(state.submit(open)),
        Some(RefusalCode::JobIdConflict)
    );

    let mut open = spec("open");
    open.set_network(NetworkLevel::Open);
    let job = state.submit(open).unwrap();
    assert_eq!(job.spec.unwrap().network(), NetworkLevel::Open);
    let mut unknown = spec("unknown");
    unknown.network = 99;
    let job = state.submit(unknown).unwrap();
    assert_eq!(job.spec.unwrap().network, 99);
}

/// Asserts the cancel intent on each way a cancel ends a job: a queued job cancelled has
/// ended and answers without it, on the answer and on every later read, as the service
/// answers; a job cancelled on its host has it until the host reports it stopped, and loses it
/// then. Catches a fake that keeps the intent on a queued job the service clears (the
/// real-service comparison), and one that keeps it on a job its host stopped.
#[test]
fn a_cancelled_job_has_the_intent_only_until_it_ends() {
    let mut state = state();
    state.submit(spec("queued")).unwrap();
    let cancelled = state.cancel("queued").unwrap();
    assert_eq!(
        (cancelled.state(), cancelled.cancel_requested),
        (JobState::Cancelled, false)
    );
    assert_eq!(state.cancel("queued").unwrap(), cancelled);
    assert_eq!(state.get("queued").unwrap(), cancelled);

    state.submit(spec("placed")).unwrap();
    assert!(state.place("placed", "h"));
    assert!(state.cancel("placed").unwrap().cancel_requested);
    assert!(state.host_stopped("placed"));
    let stopped = state.get("placed").unwrap();
    assert_eq!(
        (stopped.state(), stopped.cancel_requested),
        (JobState::Cancelled, false)
    );
}

/// Asserts the rule for a job whose root is its layers, as the service has it: no image is
/// taken with a layer and an absolute command, and refused (`INVALID_ARGUMENT`, nothing
/// recorded) with no layer or with a relative or empty command.
#[test]
fn a_job_with_no_image_needs_a_layer_and_an_absolute_command() {
    let tree = js_proto::v1::Layer {
        source: Some(js_proto::v1::layer::Source::HostPath(
            "/mnt/ts/trees/abc".to_owned(),
        )),
    };
    let imageless = |name: &str, command: &str, layers: Vec<js_proto::v1::Layer>| JobSpec {
        image: String::new(),
        command: command.to_owned(),
        layers,
        ..spec(name)
    };
    let mut state = state();
    for bad in [
        imageless("no-layer", "/bin/tool", Vec::new()),
        imageless("relative", "bin/tool", vec![tree.clone()]),
        imageless("empty", "", vec![tree.clone()]),
    ] {
        let status = state.submit(bad.clone()).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{bad:?}");
        assert!(refusal_of(&status).is_none(), "{bad:?}");
        assert!(state.jobs().is_empty(), "{bad:?}");
    }
    let taken = state
        .submit(imageless("layered", "/bin/tool", vec![tree]))
        .expect("taken");
    assert_eq!(taken.spec.expect("the spec").image, "");
}
