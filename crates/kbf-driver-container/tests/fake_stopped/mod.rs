//! A container Podman has recorded as `stopped`, against the stand-in `podman`
//! (`support/fake.rs`): the action ran to the exit Podman read; only Podman's own
//! clean-up of it (`podman container cleanup`, which makes it `exited`) is still to
//! come. On a loaded node that clean-up can land well after `podman start --attach`
//! returns (issue #658: up to about 45 seconds). The fake's `status-override` stands
//! in for Podman's record when the driver reads it, after the action has run.
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

/// Catches a container Podman has read the exit of but not yet cleaned up (`stopped
/// 0`) failed as the farm's `INTERNAL` ("the container did not run to an exit") when
/// its action ran and exited 0 (issue #658), its exit code not taken from the record
/// (`stopped 3` must be exit 3, not 0), and the action run more than once: it is not
/// idempotent, so a run that happened is never started again. `stopped -1`, Podman's
/// record of an exit it never read, still fails as the farm's.
#[tokio::test]
async fn a_stopped_container_ran_to_the_exit_podman_read() {
    let fake = Fake::new("stopped");
    let spec = Spec::new(&image(), "unused");
    let ran = fake.state.join("ran-1");

    fake.knob("status-override", "stopped 0\n");
    let result = fake
        .run(1, &spec, &action(&ran, "built", 0))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 0);
    assert_eq!(blob(&fake.cas, result.stdout_digest.as_ref()), b"built\n");
    assert_eq!(runs(&ran), 1, "the action ran once");
    let starts = fake.calls().iter().filter(|c| *c == "start").count();
    assert_eq!(starts, 1, "started once: {:?}", fake.calls());
    fake.assert_clean(1);

    let ran = fake.state.join("ran-2");
    fake.knob("status-override", "stopped 3\n");
    let result = fake
        .run(2, &spec, &action(&ran, "failed", 3))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 3);
    assert_eq!(runs(&ran), 1, "the action ran once");
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
    fake.assert_clean(3);
}

/// Catches a farm failure whose reason ends at `podman start:` with nothing after it
/// (issue #658): when neither Podman nor the action wrote anything to stderr, the
/// reason says so.
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
}
