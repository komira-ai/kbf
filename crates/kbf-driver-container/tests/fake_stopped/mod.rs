//! How the driver reads a container's exit from Podman's record, against the stand-in
//! `podman` (`support/fake.rs`). `podman start --attach` can return before that record
//! is final (issue #658): on a loaded node Podman's clean-up landed 24 to 41 seconds
//! after the container died. The driver takes an exit only from a record that says one
//! was recorded (`FinishedAt` set), waits for one that does not, and never starts the
//! container again: the action ran, and it is not idempotent. The fake's
//! `unrecorded-inspects` stands in for a record read before it holds the exit, and
//! `status-override` for the record once it does.
//!
//! The deadline on that wait (two minutes) is tested in `src/exit_record.rs`, with a
//! short one: no test here waits it out.
//!
//! A module of `fake_podman.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts the driver's lines per test binary.

use kbf_daemon::RuntimeError;

use crate::support::fake::{Fake, image};
use crate::support::{Spec, blob};

/// The act steps of an action that records each run of it in `ran`, prints `said`
/// and exits `code`.
fn action(ran: &std::path::Path, said: &str, code: u8) -> Vec<String> {
    let ran = ran.to_string_lossy();
    let code = code.to_string();
    [
        "append",
        ran.as_ref(),
        "ran",
        ";",
        "echo",
        said,
        ";",
        "exit",
        code.as_str(),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// How many times the action recorded in `ran` has run.
fn runs(ran: &std::path::Path) -> usize {
    std::fs::read_to_string(ran).map_or(0, |text| text.lines().count())
}

/// How many times the driver called `verb`.
fn count(fake: &Fake, verb: &str) -> usize {
    fake.calls().iter().filter(|c| *c == verb).count()
}

/// Catches an exit taken from a record Podman has not finished (`stopped 0`, no
/// `FinishedAt`) as the action's (issue #658): an action that exited 3 reported as
/// exit 0, a failure cached as a success. Also catches the record treated as a
/// container that never ran (the farm's `INTERNAL`), and the container started again
/// for it. The fake answers the first three inspects as unrecorded, then with the
/// action's own `exited 3`: the lease must report 3, after four inspects and one start.
#[tokio::test]
async fn an_exit_not_yet_recorded_is_waited_for_not_run_again() {
    let fake = Fake::new("unrecorded");
    let spec = Spec::new(&image(), "unused");
    let ran = fake.state.join("ran");

    fake.knob("unrecorded-inspects", "3\n");
    let result = fake
        .run(1, &spec, &action(&ran, "failed", 3))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 3, "the exit Podman recorded");
    assert_eq!(blob(&fake.cas, result.stdout_digest.as_ref()), b"failed\n");
    assert_eq!(runs(&ran), 1, "the action ran once");
    assert_eq!(count(&fake, "start"), 1, "started once: {:?}", fake.calls());
    assert_eq!(
        count(&fake, "inspect"),
        4,
        "read until recorded: {:?}",
        fake.calls()
    );
    fake.assert_clean(1);
}

/// Catches a record that already holds the exit being waited on or read again: an
/// `exited` or `stopped` record with `FinishedAt` set is final (Podman reads the exit
/// file and stores code and time together), so the driver takes its code from the
/// first inspect, and `stopped 3` is exit 3, not the 0 an unset code would read as.
/// `stopped -1`, Podman's record of an exit it never read, is the farm's failure,
/// also at once and without a second start.
#[tokio::test]
async fn a_recorded_exit_is_taken_at_once() {
    let fake = Fake::new("recorded");
    let spec = Spec::new(&image(), "unused");

    let ran = fake.state.join("ran-1");
    let result = fake
        .run(1, &spec, &action(&ran, "built", 0))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 0);
    assert_eq!(blob(&fake.cas, result.stdout_digest.as_ref()), b"built\n");
    assert_eq!(runs(&ran), 1, "the action ran once");
    assert_eq!(count(&fake, "inspect"), 1, "one read: {:?}", fake.calls());
    fake.assert_clean(1);

    let ran = fake.state.join("ran-2");
    fake.knob("status-override", "stopped 3\n");
    let result = fake
        .run(2, &spec, &action(&ran, "failed", 3))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 3);
    assert_eq!(runs(&ran), 1, "the action ran once");
    assert_eq!(count(&fake, "inspect"), 2, "one read: {:?}", fake.calls());
    fake.assert_clean(2);

    let ran = fake.state.join("ran-3");
    fake.knob("status-override", "stopped -1\n");
    let outcome = fake.run(3, &spec, &action(&ran, "lost", 0)).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.contains("did not run to an exit: podman reports \"stopped -1\"")),
        "{outcome:?}"
    );
    assert_eq!(runs(&ran), 1, "never started again");
    assert_eq!(
        count(&fake, "start"),
        3,
        "one start per lease: {:?}",
        fake.calls()
    );
    assert_eq!(count(&fake, "inspect"), 3, "one read: {:?}", fake.calls());
    fake.assert_clean(3);
}

/// Catches a farm failure whose reason ends at `podman start:` with nothing after it
/// (issue #658): when neither Podman nor the action wrote anything to stderr, the
/// reason says so; when Podman did, the reason ends with its words; and when crun
/// said the program is not there, it is the action's exit 127, not the farm's. (The
/// last is `tests/program.rs`'s subject; it is here too so the build of the library
/// this binary links runs every arm of `program::not_run`, which the coverage job
/// counts per build.)
#[tokio::test]
async fn a_start_that_said_nothing_says_so() {
    let fake = Fake::new("said-nothing");
    let spec = Spec::new(&image(), "unused");

    fake.knob("status-override", "created 0\n");
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.ends_with("podman reports \"created 0\"; podman start: (nothing on stderr)")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let said = "Error: crun: write `pids.max`: Operation not permitted: OCI permission denied";
    let steps = ["echo", "--stderr", said, ";", "exit", "125"];
    let outcome = fake.run(2, &spec, &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.ends_with(&format!("podman reports \"created 0\"; podman start: {said}"))),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    let mut spec = spec;
    spec.argv = vec!["env".to_owned()];
    let said = "Error: unable to start container \"0123456789ab\": crun: executable file \
                `env` not found in $PATH: No such file or directory";
    let steps = ["echo", "--stderr", said, ";", "exit", "125"];
    let result = fake
        .run(3, &spec, &steps)
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    fake.assert_clean(3);
}
