//! `GET /v1/proxy/swap`: the answer for each field, the state of an update for each phase and
//! failure, the refusal of the binary watch, the numbers of updates, and what is not the swap's
//! state (another method, another path). The swap is a fake that holds the snapshot a test
//! sets, and `releases.json` is a file in a scratch directory.
//!
//! Each "Mutants it catches" line names a change to the code that the test is written to turn
//! red. They are the reason the test exists, not a record that anyone ran them.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::*;
use crate::proxy::assemble::GONE;
use crate::proxy::model::{Halt, Mode, Plan, Timing};
use crate::proxy::releases::{Local, STATUS};
use crate::testkit::{A, B, C, c};

const BOOT: &str = "2026-10-22T08:00:00Z-4242";
const ID: &str = "pkg-bbbbbbbbbbbb-1000";

/// The start of the update the tests show: 2026-10-22T08:41:16Z.
const STARTED: u64 = 1_792_658_476;
const STARTED_AT: &str = "2026-10-22T08:41:16Z";
/// Its end, 125 s later.
const ENDED: u64 = 1_792_658_601;
const ENDED_AT: &str = "2026-10-22T08:43:21Z";

/// A swap that shows the snapshot the test last set.
struct FakeSource(Mutex<Snapshot>);

impl Source for FakeSource {
    fn snapshot(&self) -> Snapshot {
        self.0.lock().expect("lock").clone()
    }
}

/// The route over a fake swap and a state directory of its own.
struct World {
    state: PathBuf,
    watch: WatchStatus,
    source: Arc<FakeSource>,
    route: SwapStatus,
}

impl World {
    fn new(name: &str) -> World {
        let root = kbf_host::testkit::scratch(&format!("swap-status-{name}"));
        let state = root.join("state");
        fs::create_dir(&state).expect("state dir");
        let local = Local::open(root.join("package-kbf"), &state).expect("copies");
        let watch = WatchStatus::default();
        let source = Arc::new(FakeSource(Mutex::new(Snapshot::default())));
        let route = SwapStatus::new(
            source.clone(),
            Arc::clone(&local.orders),
            watch.clone(),
            BOOT.to_owned(),
        );
        World {
            state,
            watch,
            source,
            route,
        }
    }

    /// The swap shows `snapshot` from now on.
    fn show(&self, snapshot: Snapshot) {
        *self.source.0.lock().expect("lock") = snapshot;
    }

    /// `releases.json` says `commit` is installed.
    fn install(&self, commit: &str) {
        let text = format!("{{\"installed_commit\":\"{commit}\"}}\n");
        fs::write(self.state.join(STATUS), text).expect("releases.json");
    }

    async fn call(&self, method: &str, path: &str) -> Answered {
        let call = Call {
            method: method.to_owned(),
            path: path.to_owned(),
            body: Vec::new(),
        };
        self.route.answer(call).await
    }

    /// The answer of a `GET` as JSON.
    async fn get(&self) -> Value {
        let answered = self.call("GET", PATH).await;
        assert_eq!(answered.status, 200);
        assert!(answered.headers.is_empty());
        serde_json::from_slice(&answered.body).expect("JSON")
    }
}

/// The swap's record of the update the tests show, at `phase`, with `failure` if it failed.
fn intent(phase: Phase, failure: Option<(Step, &str)>) -> Intent {
    let plan = Plan {
        update_id: ID.to_owned(),
        request_id: ID.to_owned(),
        mode: Mode::Frontend,
        from: c(A),
        to: c(B),
        nodes: false,
        rollback: false,
        source: kbf_update::Source::Staged,
        old_slot: Slot::A,
        timing: Timing::frontend(),
    };
    let mut intent = Intent::begin(&plan, None, STARTED);
    intent.phase = phase;
    intent.phase_started = ENDED;
    intent.failure = failure.map(|(step, reason)| Failure {
        step,
        reason: reason.to_owned(),
    });
    intent
}

/// The snapshot of the new child serving, with the swap's record `intent`.
fn serving_b(intent: Intent) -> Snapshot {
    Snapshot {
        slot: Some(Slot::B),
        running: Some(c(B)),
        intent: Some(intent),
    }
}

/// The answer with every key, in the order the endpoint documents, for an update that is done.
fn golden() -> String {
    [
        r#"{"proxy_boot":"2026-10-22T08:00:00Z-4242","#,
        r#""installed_commit":"<B>","active_slot":"b","active_commit":"<B>","#,
        r#""update":{"id":"pkg-bbbbbbbbbbbb-1000","seq":1,"from":"<A>","to":"<B>","#,
        r#""phase":"done","state":"done","reason":null,"#,
        r#""started_at":"2026-10-22T08:41:16Z","ended_at":"2026-10-22T08:43:21Z"},"#,
        r#""pending_binary":null}"#,
    ]
    .concat()
    .replace("<A>", A)
    .replace("<B>", B)
}

/// A clock that stands at one time.
struct At(u64);

impl Clock for At {
    fn now(&self) -> Duration {
        Duration::from_secs(self.0)
    }

    fn sleep(&self, _d: Duration) -> Result<(), Halt> {
        Ok(())
    }
}

/// Catches an answer whose shape has drifted: for an update that is done, the bytes are these,
/// with every key, in this order, the commits as strings, the times in RFC 3339 and the
/// numbers as numbers; the status is 200 and no header is added to the body's own.
///
/// Mutants it catches: a key renamed, dropped or reordered; a time in another format (a
/// number, an offset instead of `Z`); `seq` a string; `null` written as an empty string or
/// left out; the slot in upper case.
#[tokio::test]
async fn the_answer_for_a_finished_update_is_this_json_exactly() {
    let world = World::new("golden");
    world.install(B);
    world.show(serving_b(intent(Phase::Done, None)));

    let answered = world.call("GET", PATH).await;

    assert_eq!(answered.status, 200);
    assert!(answered.headers.is_empty());
    assert_eq!(String::from_utf8(answered.body).expect("UTF-8"), golden());
}

/// Catches fields read from the wrong place or given when nothing is known: with nothing known
/// every field but the boot id is `null`; the boot id is the proxy's.
///
/// Mutants it catches: a field that is not `null` when unknown (an empty string, a made-up
/// slot); the boot id left out or changed by a call.
#[tokio::test]
async fn with_nothing_known_every_field_but_the_boot_id_is_null() {
    let world = World::new("nothing");

    let first = world.get().await;
    let second = world.get().await;

    assert_eq!(first["proxy_boot"], BOOT);
    assert_eq!(first, second, "the boot id is fixed");
    for key in [
        "installed_commit",
        "active_slot",
        "active_commit",
        "update",
        "pending_binary",
    ] {
        assert!(first[key].is_null(), "{key}: {first}");
    }
}

/// Catches `active_commit` taken from where the installed release or the binary is, or the
/// slot and the commit mixed up: the installed commit is the one `releases.json` says (`B`),
/// the active slot and the commit that child runs are the swap's (`a` and `A`), and the kbf
/// waiting at the path is `pending_binary` (`C`). Each is its own field.
///
/// Mutants it catches: `active_commit` read from `installed_commit`, from the pending kbf or
/// from the file on disk (the installed release is `B`, and the kbf on disk is `C`);
/// `installed_commit` read from the swap; `pending_binary` read from the installed release;
/// the slot in the wrong field.
#[tokio::test]
async fn each_field_is_read_from_its_own_source() {
    let world = World::new("fields");
    world.install(B);
    world.watch.pend(Some(c(C)));
    world.show(Snapshot {
        slot: Some(Slot::A),
        running: Some(c(A)),
        intent: None,
    });

    let got = world.get().await;

    assert_eq!(got["installed_commit"], B);
    assert_eq!(got["active_slot"], "a");
    assert_eq!(got["active_commit"], A);
    assert_eq!(got["pending_binary"], C);
    assert!(got["update"].is_null());
}

/// Catches a status file that cannot be read being an error of the endpoint: the answer is
/// still 200, with `installed_commit` `null` (the other fields are the swap's).
///
/// Mutant it catches: the read error answered as a 500, or taken for the previous value.
#[tokio::test]
async fn an_unreadable_releases_file_makes_the_installed_commit_null() {
    let world = World::new("torn");
    fs::write(world.state.join(STATUS), b"{").expect("a torn releases.json");
    world.show(serving_b(intent(Phase::Done, None)));

    let got = world.get().await;

    assert!(got["installed_commit"].is_null());
    assert_eq!(got["active_commit"], B);
}

/// Catches a state read off the wrong phase: only a recorded `done` is `done` (which the swap
/// records after the confirm and the drain), every phase before it is `running` (`baking`,
/// where the confirm is still to come, and `draining`, where it has been sent, included), a
/// rollback is `abandoned`, and a failure is `refused` at a step before any candidate process
/// and `abandoned` at any later one. A failure recorded while the update still runs (a confirm
/// that is sent again) does not end it.
///
/// Mutants it catches: `done` before the confirm (at `baking`, `nodes` or `draining`); `done`
/// for a failed update; a step moved between `refused` and `abandoned`; the failure of a
/// running update ending it.
#[test]
fn the_state_is_done_only_for_a_recorded_done_and_a_failure_is_told_by_its_step() {
    let running = [
        Phase::Accepted,
        Phase::Fetching,
        Phase::Verifying,
        Phase::Staged,
        Phase::Preflight,
        Phase::Starting,
        Phase::Ready,
        Phase::Holding,
        Phase::Quiescing,
        Phase::Switching,
        Phase::Switched,
        Phase::Baking,
        Phase::Nodes,
        Phase::Draining,
    ];
    for phase in running {
        assert_eq!(State::of(phase, None), State::Running, "{phase:?}");
    }
    assert_eq!(State::of(Phase::Done, None), State::Done);
    assert_eq!(State::of(Phase::RolledBack, None), State::Abandoned);
    for step in REFUSED_AT {
        let state = State::of(Phase::Failed, Some(step));
        assert_eq!(state, State::Refused, "{step:?}");
    }
    let abandoned = [
        Step::Start,
        Step::Ready,
        Step::Hold,
        Step::Switch,
        Step::Abort,
        Step::Rollback,
        Step::Restart,
        Step::Confirm,
        Step::Nodes,
    ];
    for step in abandoned {
        let state = State::of(Phase::Failed, Some(step));
        assert_eq!(state, State::Abandoned, "{step:?}");
    }
    assert_eq!(State::of(Phase::Failed, None), State::Abandoned);
    let confirming = State::of(Phase::Baking, Some(Step::Confirm));
    assert_eq!(confirming, State::Running);
}

/// Catches an update told wrong end to end: a running update has no end and no reason; a
/// failure has its step and reason and an end; a failure before any candidate is `refused`; a
/// rollback has its words; a failure recorded while the update runs is told as it is.
///
/// Mutants it catches: an end time on a running update; the step left out of the reason; the
/// state, phase or reason of one of these taken from another.
#[tokio::test]
async fn an_update_is_told_with_its_phase_state_reason_and_end() {
    let world = World::new("told");
    let cases = [
        (
            intent(Phase::Baking, None),
            "baking",
            "running",
            Value::Null,
        ),
        (
            intent(Phase::Failed, Some((Step::Switch, "takeover: refused"))),
            "failed",
            "abandoned",
            Value::from("switch: takeover: refused"),
        ),
        (
            intent(Phase::Failed, Some((Step::Migrate, "installer_too_old: x"))),
            "failed",
            "refused",
            Value::from("migrate: installer_too_old: x"),
        ),
        (
            intent(Phase::RolledBack, None),
            "rolled_back",
            "abandoned",
            Value::from("rolled back by the operator"),
        ),
        (
            intent(Phase::Baking, Some((Step::Confirm, "no (try 1 of 6)"))),
            "baking",
            "running",
            Value::from("confirm: no (try 1 of 6)"),
        ),
    ];
    for (record, phase, state, reason) in cases {
        let ends = record.phase.is_terminal();
        world.show(serving_b(record));

        let got = world.get().await;

        let update = &got["update"];
        assert_eq!(update["phase"], phase);
        assert_eq!(update["state"], state, "{phase}");
        assert_eq!(update["reason"], reason, "{phase}");
        assert_eq!(update["started_at"], STARTED_AT, "{phase}");
        let ended = &update["ended_at"];
        if ends {
            assert_eq!(*ended, ENDED_AT, "{phase}");
        } else {
            assert!(ended.is_null(), "{phase}: {update}");
        }
    }
}

/// Catches a start time that is lost for an update an older build recorded (no start time): the
/// time of its newest phase stands for it, and one that has a start time keeps it.
///
/// Mutants it catches: the start time of the record ignored (the newest phase's time given
/// for every update); an absent start time shown as 1970.
#[tokio::test]
async fn an_update_without_a_start_time_starts_when_its_newest_phase_did() {
    let world = World::new("old-record");
    let mut record = intent(Phase::Done, None);
    record.started_at = 0;
    world.show(serving_b(record));

    let got = world.get().await;

    assert_eq!(got["update"]["started_at"], ENDED_AT);
    world.show(serving_b(intent(Phase::Done, None)));
    let got = world.get().await;
    assert_eq!(got["update"]["started_at"], STARTED_AT);
}

/// Catches updates that are not numbered once each: the same update asked about twice has the
/// same number, the next update has the next, and an update the watch started was numbered
/// when it started, so it keeps its place when it is first asked about.
///
/// Mutants it catches: a number that goes up with every request; a number that is always 1;
/// the numbers of two updates equal.
#[tokio::test]
async fn an_update_keeps_its_number_and_the_next_update_takes_the_next() {
    let world = World::new("seq");
    world.show(serving_b(intent(Phase::Baking, None)));

    let first = world.get().await;
    let again = world.get().await;

    assert_eq!(first["update"]["seq"], 1);
    assert_eq!(again["update"]["seq"], 1);
    let mut next = intent(Phase::Baking, None);
    next.update_id = "pkg-cccccccccccc-2000".to_owned();
    world.show(serving_b(next));
    let second = world.get().await;
    assert_eq!(second["update"]["seq"], 2);
    assert_eq!(world.watch.seq_of(ID), 1, "the first keeps its number");
}

/// Catches a refusal of the binary watch that is not shown, or shown over the wrong update:
/// while it stands it is the update (state `refused`, phase `accepted`, its reason, its times
/// both the time of the failure, `from` as the watch read it, its own id and number); once the
/// watch drops it, the swap's own record is shown again.
///
/// Mutants it catches: the refusal ignored; the refusal shown after it is dropped; a refusal
/// shown as `running` or `abandoned`; its time not the failure's.
#[tokio::test]
async fn a_refusal_of_the_watch_is_the_update_while_it_stands() {
    let world = World::new("refusal");
    world.show(serving_b(intent(Phase::Done, None)));
    let id = "pkg-cccccccccccc-3000".to_owned();
    let reason = "copy: commit_mismatch: not a release build".to_owned();
    let watch = &world.watch;
    watch.refuse(id.clone(), Some(c(B)), c(C), reason.clone(), ENDED);

    let got = world.get().await;

    let update = &got["update"];
    assert_eq!(update["id"], id.as_str());
    assert_eq!(update["seq"], 1);
    assert_eq!(update["from"], B);
    assert_eq!(update["to"], C);
    assert_eq!(update["phase"], "accepted");
    assert_eq!(update["state"], "refused");
    assert_eq!(update["reason"], reason.as_str());
    assert_eq!(update["started_at"], ENDED_AT);
    assert_eq!(update["ended_at"], ENDED_AT);
    assert_eq!(got["pending_binary"], Value::Null);

    world.watch.clear();

    let got = world.get().await;

    assert_eq!(got["update"]["id"], ID);
    assert_eq!(got["update"]["state"], "done");
}

/// Catches a refusal that has no serving release shown as one: `from` is `null`.
///
/// Mutant it catches: an empty string or the target for an unknown `from`.
#[tokio::test]
async fn a_refusal_without_a_serving_release_has_a_null_from() {
    let world = World::new("refusal-unserved");
    let reason = "no installed release".to_owned();
    world.watch.refuse(ID.to_owned(), None, c(B), reason, 5);

    let got = world.get().await;

    assert!(got["update"]["from"].is_null());
    assert_eq!(got["update"]["to"], B);
}

/// Catches an endpoint that answers what is not its to answer: another method on the swap's
/// state is 405 with `Allow: GET` and a JSON error, and any other path (the update API's) is
/// the 410 of a retired API, whatever the method, and neither reads the swap.
///
/// Mutants it catches: a `POST` served as a `GET`; the update API's paths answered with the
/// swap's state; the 410 lost.
#[tokio::test]
async fn only_get_on_the_state_is_served_and_the_retired_api_stays_gone() {
    let world = World::new("methods");
    world.show(serving_b(intent(Phase::Done, None)));

    for method in ["POST", "PUT", "DELETE", "HEAD"] {
        let answered = world.call(method, PATH).await;
        assert_eq!(answered.status, 405, "{method}");
        let allow = answered.headers.iter().find(|(name, _)| *name == ALLOW);
        let value = allow.map(|(_, value)| value.clone());
        assert_eq!(value, Some(HeaderValue::from_static("GET")), "{method}");
        let body: Value = serde_json::from_slice(&answered.body).expect("JSON");
        assert!(body["error"].is_string(), "{method}: {body}");
    }
    for (method, path) in [
        ("GET", "/v1/update/frontend"),
        ("POST", "/v1/update/frontend"),
        ("POST", "/v1/update/frontend/abort"),
        ("GET", "/v1/proxy/swap/x"),
        ("GET", "/v1/proxy"),
    ] {
        let answered = world.call(method, path).await;
        assert_eq!(answered.status, 410, "{method} {path}");
        assert_eq!(answered.body, GONE.as_bytes(), "{method} {path}");
    }
}

/// Catches an id that is not the start time and the pid: the id is RFC 3339 UTC of the clock's
/// second, a dash and the pid, and two processes started in the same second have ids that
/// differ by their pid.
///
/// Mutants it catches: the pid left out or put first; the time in another form; the time of
/// the call and not of the start (it is fixed when the route is made, and the test asks the
/// clock once).
#[test]
fn the_boot_id_is_the_start_time_and_the_pid() {
    let first = boot_id(&At(STARTED), 1234);
    let second = boot_id(&At(STARTED), 1235);
    assert_eq!(first, "2026-10-22T08:41:16Z-1234");
    assert_ne!(first, second);
    assert_eq!(boot_id(&At(0), 1), "1970-01-01T00:00:00Z-1");
}
