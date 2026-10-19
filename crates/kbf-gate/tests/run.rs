//! The run recorder: RUNNING first, FINISHED on every exit it can see, and what is left when
//! it cannot see the exit.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_gate::{
    CommitId, GateError, GateResult, Header, Limit, ResultDoc, Run, Sample, SetHash, Stage, Status,
    Verdict, system_clock,
};

const ROLE: &str = "KBF_TEST_GATE_ROLE";
const PATH: &str = "KBF_TEST_GATE_PATH";

fn header() -> Header {
    Header {
        stage: Stage::Beta,
        commit: CommitId::parse(&"cd".repeat(20)).unwrap(),
        set_hash: SetHash::of_sums(b"sums"),
        run_id: "r1".to_owned(),
        attempt: 1,
    }
}

fn clock() -> u64 {
    5_000
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("result.json")
}

fn read(path: &Path) -> ResultDoc {
    ResultDoc::from_bytes(&std::fs::read(path).unwrap()).unwrap()
}

fn green_gate() -> GateResult {
    GateResult::decide("g", Sample::Value(0.0), Limit::AtMost(0.0), 1, 2)
}

#[test]
fn the_document_is_running_the_moment_a_run_starts() {
    let path = scratch("started");
    let run = Run::start(&path, header(), clock).unwrap();
    let d = read(&path);
    assert_eq!(
        (d.status, d.started, d.finished),
        (Status::Running, 5_000, None)
    );
    assert!(!d.passed());
    assert_eq!(run.doc().run_id, "r1");
    assert_eq!(run.now(), 5_000);
    drop(run);
}

#[test]
fn update_persists_each_change_and_finish_decides_from_the_gates() {
    let path = scratch("finish");
    let mut run = Run::start(&path, header(), clock).unwrap();
    run.update(|d| {
        d.desired_generation = Some(9);
        d.gates.push(green_gate());
    })
    .unwrap();
    let mid = read(&path);
    assert_eq!(
        (mid.status, mid.desired_generation, mid.gates.len()),
        (Status::Running, Some(9), 1)
    );
    assert_eq!(run.finish().unwrap(), Status::Green);
    let d = read(&path);
    assert_eq!((d.status, d.finished), (Status::Green, Some(5_000)));
    assert!(d.passed());
}

#[test]
fn a_red_gate_finishes_red_and_no_gate_finishes_red() {
    let path = scratch("red");
    let mut run = Run::start(&path, header(), clock).unwrap();
    run.update(|d| {
        d.gates.push(GateResult::decide(
            "g",
            Sample::Absent,
            Limit::AtMost(0.0),
            1,
            2,
        ))
    })
    .unwrap();
    assert_eq!(run.finish().unwrap(), Status::Red);
    assert_eq!(read(&path).gates[0].verdict, Verdict::Red);

    let run = Run::start(&path, header(), clock).unwrap();
    assert_eq!(run.finish().unwrap(), Status::Red);
}

#[test]
fn fail_and_extend_record_their_reason() {
    let path = scratch("fail");
    Run::start(&path, header(), clock)
        .unwrap()
        .fail("scrape refused")
        .unwrap();
    let d = read(&path);
    assert_eq!(
        (d.status, d.note.as_deref()),
        (Status::Red, Some("scrape refused"))
    );
    Run::start(&path, header(), clock)
        .unwrap()
        .extend("evaluator error")
        .unwrap();
    let d = read(&path);
    assert_eq!(
        (d.status, d.note.as_deref()),
        (Status::Extended, Some("evaluator error"))
    );
    assert!(!d.passed());
}

fn leaves_early(path: &Path) -> Result<(), GateError> {
    let mut run = Run::start(path, header(), clock)?;
    run.update(|d| d.gates.push(green_gate()))?;
    Err(GateError::Io {
        path: path.to_owned(),
        source: std::io::Error::other("stop"),
    })
}

/// Catches a drop guard that skips the write on the error path: a function that returns
/// early with `?` must leave FINISHED, and red, even though its one gate was green.
#[test]
fn an_early_return_writes_finished_red() {
    let path = scratch("early");
    assert!(leaves_early(&path).is_err());
    let d = read(&path);
    assert_eq!((d.status, d.finished), (Status::Red, Some(5_000)));
    assert_eq!(d.note.as_deref(), Some("the run ended without a result"));
    assert!(!d.passed());
}

/// Catches a guard that does not run while unwinding: after a panic the document says so.
#[test]
fn a_panic_writes_finished_red() {
    let path = scratch("panic");
    let p = path.clone();
    let result = std::panic::catch_unwind(move || {
        let _run = Run::start(&p, header(), clock).unwrap();
        panic!("boom");
    });
    assert!(result.is_err());
    let d = read(&path);
    assert_eq!(d.status, Status::Red);
    assert_eq!(
        d.note.as_deref(),
        Some("the run panicked before it finished")
    );
}

/// The test binary, run again as a child that does one thing to its run and then exits.
fn child(role: &str, path: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_role", "--nocapture"])
        .env(ROLE, role)
        .env(PATH, path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// Not a test by itself: the body of the children above. Without the role it does nothing.
#[test]
fn child_role() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let path = PathBuf::from(std::env::var_os(PATH).unwrap());
    let mut run = Run::start(&path, header(), clock).unwrap();
    run.update(|d| d.gates.push(green_gate())).unwrap();
    match role.to_str().unwrap() {
        // What a binary that calls `std::process::exit` on an error would do: no destructor.
        "exit" => std::process::exit(4),
        "fail_and_exit" => run.fail_and_exit("giving up", 3),
        "hang" => {
            println!("ready");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        other => panic!("unknown role {other}"),
    }
}

/// Catches a drop guard trusted to cover `process::exit`: it does not run, the file stays
/// RUNNING, and it is not a pass. That is the limit the crate documents, pinned.
#[test]
fn a_bare_process_exit_leaves_running_which_is_not_a_pass() {
    let path = scratch("exit");
    let status = child("exit", &path).wait().unwrap();
    assert_eq!(status.code(), Some(4));
    let d = read(&path);
    assert_eq!(d.status, Status::Running);
    assert!(!d.passed());
}

/// Catches `fail_and_exit` that exits without writing (a `process::exit` mutant): the child
/// must exit with its code and leave FINISHED, red, with the reason.
#[test]
fn fail_and_exit_writes_finished_red_then_exits_with_the_code() {
    let path = scratch("fail_and_exit");
    let status = child("fail_and_exit", &path).wait().unwrap();
    assert_eq!(status.code(), Some(3));
    let d = read(&path);
    assert_eq!(
        (d.status, d.note.as_deref()),
        (Status::Red, Some("giving up"))
    );
    assert!(d.finished.is_some());
}

/// A process killed by a signal while it runs: SIGKILL runs no code at all. The document
/// on disk is the whole RUNNING one, readable and not a pass.
#[test]
fn a_sigkill_leaves_a_whole_running_document() {
    use std::io::{BufRead, BufReader};
    let path = scratch("sigkill");
    let mut c = child("hang", &path);
    let mut out = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("ready") {
        line.clear();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the child ended early"
        );
    }
    c.kill().unwrap();
    c.wait().unwrap();
    let d = read(&path);
    assert_eq!((d.status, d.gates.len()), (Status::Running, 1));
    assert!(!d.passed());
}

#[test]
fn an_unwritable_path_is_an_error_naming_the_path() {
    let path = scratch("nodir")
        .parent()
        .unwrap()
        .join("missing/dir/result.json");
    let err = Run::start(&path, header(), clock).err().unwrap();
    assert!(matches!(err, GateError::Io { .. }));
    assert!(err.to_string().contains("missing/dir/result.json"), "{err}");
}

#[test]
fn a_document_that_will_not_serialise_is_an_error() {
    let path = scratch("nan");
    let mut run = Run::start(&path, header(), clock).unwrap();
    let nan = GateResult::decide("g", Sample::Value(0.0), Limit::AtMost(f64::NAN), 1, 2);
    let err = run.update(|d| d.gates.push(nan)).unwrap_err();
    assert!(matches!(err, GateError::Doc(_)), "{err}");
    assert!(err.to_string().contains("not a number"), "{err}");
    drop(run);
}

#[test]
fn the_system_clock_is_after_2024() {
    assert!(system_clock() > 1_700_000_000);
}
