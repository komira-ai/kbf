//! The deploy against the update API of the frontend proxy: a fake of it (`Farm`) served
//! over TLS from a test CA, which the deploy reaches as it reaches the real port. The fake
//! answers with the real types of the API (`StatusBody`, `UpdateRequest`) and keeps its
//! rules: a request id it has seen gets the update it started then, and one update is open
//! at a time. Time is the farm's: it moves only when the deploy sleeps.

use super::tls::{TOKEN, with_tls};
use super::*;
use crate::deploy::summary::Frontend;

/// The deploy over TLS against `f` with `o`.
fn tls(f: &Farm, o: &Options) -> Summary {
    run_tls(f, o, SmokeResult::Pass, None)
}

fn with_fate(fate: FrontendFate) -> Farm {
    let mut f = farm(two_nodes());
    f.frontend_fate = fate;
    f
}

fn failure(step: FailedStep, reason: &str) -> Failure {
    Failure {
        step,
        reason: reason.to_owned(),
    }
}

/// The step `name` of a run.
fn step<'s>(s: &'s Summary, name: &str) -> &'s summary::Step {
    s.steps.iter().find(|x| x.name == name).unwrap()
}

/// Proves the whole deploy over TLS, as the gamma pipeline runs it: the bundle goes to the
/// store, the frontend is updated through the API and followed to `done`, the nodes are
/// watched, the farm is checked, and every call carries the operator token. Catches a deploy
/// whose transport or whose order differs from the design (frontend first, one POST, no
/// desired release posted, no node ordered).
///
/// Mutants planted: a `POST /v1/update/desired` added before the frontend step (the count of
/// POSTs and of desired posts go red); the nodes watched before `done` (the time of the first
/// nodes read goes red); the token left off the PUT.
#[test]
fn a_clean_deploy_runs_over_tls_frontend_first() {
    let f = farm(two_nodes());
    let bundle = tarball(&[(&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"x")]);
    let mut input: &[u8] = &bundle;
    let (s, auth) = with_tls(&f, |https, served| {
        let clock = Clk(f.now.clone());
        let (smoke, check) = (FixedSmoke(SmokeResult::Pass), FixedCheck(Ok(true)));
        let env = Env {
            api: https,
            clock: &clock,
            smoke: &smoke,
            check: &check,
        };
        let s = deploy(&env, &opts(true), Some(&mut input));
        (s, served.auth.lock().unwrap().clone())
    });
    assert_eq!(
        (s.exit, s.result.as_str(), s.reason.as_str()),
        (0, "ok", ""),
        "{}",
        s.line()
    );
    let names: Vec<_> = s.steps.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(
        names,
        ["bundle", "preflight", "baseline", "frontend", "nodes"]
    );
    let bearer = format!("Bearer {TOKEN}");
    assert!(auth.len() > 20, "{} calls", auth.len());
    assert!(auth.iter().all(|a| *a == bearer));
    // The upload, the one update request, and nothing else that writes.
    assert_eq!(f.requests("PUT"), 1);
    assert_eq!(f.requests("POST"), 1);
    assert_eq!(f.requests("POST /v1/update/frontend"), 1);
    assert_eq!(f.requests("POST /v1/update/desired"), 0);
    // `done` comes first, then the nodes.
    assert_eq!(step(&s, "frontend").ms, DONE_AT * 1000);
    assert_eq!(
        f.first_at("GET /v1/nodes", "POST /v1/update/frontend"),
        Some(DONE_AT)
    );
    assert_eq!(s.generation, Some(8));
    assert_eq!(s.frontend.unwrap().phase, Some(Phase::Done));
}

/// Proves the body of the one request is the API's own request type with this run's id, and
/// that nothing else is in it: the commit, the request id, `nodes: true`, `rollback: false`.
/// Catches a body the API's `deny_unknown_fields` would refuse, a missing request id (a 400),
/// and a deploy that asks to skip the nodes or to roll back.
///
/// Mutants planted: `nodes` false; `rollback` true; the request id left out.
#[test]
fn the_request_is_the_one_the_api_reads() {
    let f = farm(two_nodes());
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    let raw = f.raw.lock().unwrap().clone();
    assert_eq!(
        raw,
        [format!(
            r#"{{"commit":"{TARGET}","request_id":"{REQUEST_ID}","nodes":true,"rollback":false}}"#
        )
        .into_bytes()]
    );
}

/// Proves a failed update is a failure of the deploy that names the step and the reason the
/// API gave, in the error and in the JSON, stops there (no node is looked at, no post check
/// runs) and does not wait for the deadline. Catches a failed update read as a pending one,
/// the failure lost, and a deploy that goes on to the nodes of a farm whose frontend did not
/// change.
///
/// Mutants planted: `failed` treated as a phase to wait out; the failure's step dropped from
/// the detail; the nodes step run after a failed frontend.
#[test]
fn a_failed_update_names_its_step_and_reason() {
    let f = with_fate(FrontendFate::Failed);
    let s = tls(&f, &opts(true));
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_failed"),
        "{}",
        s.line()
    );
    assert_eq!(
        s.detail,
        "update u-1 ended failed at step start: the candidate exited"
    );
    assert_eq!(
        s.frontend,
        Some(Frontend {
            update_id: Some("u-1".to_owned()),
            phase: Some(Phase::Failed),
            failure: Some(failure(FailedStep::Start, "the candidate exited")),
            already_running: false,
        })
    );
    let json: Value = serde_json::from_str(&s.line()).unwrap();
    assert_eq!(json["frontend"]["phase"], "failed");
    assert_eq!(json["frontend"]["failure"]["step"], "start");
    assert_eq!(
        json["frontend"]["failure"]["reason"],
        "the candidate exited"
    );
    assert_eq!(json["steps"][3]["name"], "frontend");
    assert_eq!(json["steps"][3]["ok"], false);
    assert_eq!(json["steps"][3]["reason"], "frontend_failed");
    // Stopped when the API said so (10 s), at the nodes of the preflight and no later.
    assert_eq!(f.now.get(), 10);
    assert_eq!(f.requests("GET /v1/nodes"), 1);
    assert!(s.nodes.is_empty());
    assert_eq!(s.post, PostResult::default());
}

/// Proves a rolled-back update is a failure too, whose detail says there is no step when the
/// API names none. Catches `rolled_back` read as success (the old frontend is serving: the
/// commit is not deployed).
///
/// Mutants planted: `rolled_back` treated as `done`; treated as pending.
#[test]
fn a_rolled_back_update_is_a_failure_with_no_step_to_name() {
    let f = with_fate(FrontendFate::RolledBack);
    let s = tls(&f, &opts(true));
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_rolled_back"),
        "{}",
        s.line()
    );
    assert_eq!(
        s.detail,
        "update u-1 ended rolled_back (the API named no step)"
    );
    assert_eq!(s.frontend.as_ref().unwrap().phase, Some(Phase::RolledBack));
    let json: Value = serde_json::from_str(&s.line()).unwrap();
    assert!(json["frontend"].get("failure").is_none());
    assert_eq!(f.now.get(), 20);
    assert_eq!(f.requests("GET /v1/nodes"), 1);
}

/// Proves each answer to the POST that is not an update ends the deploy the way its code
/// says: a busy API is exit 4 and is not asked again, an incompatible or incomplete release
/// is a refusal (exit 3), a lost state or a reused request id is a failure, and so is a
/// status the API does not use for this. Catches a busy API read as a failure, an
/// incompatibility read as busy, and a retry of an answer that will not change.
///
/// Mutants planted: `incompatible` treated as busy; `update_open` treated as a failure;
/// 422 treated as a failure; a refusal retried.
#[test]
fn a_refusal_ends_the_deploy_by_its_code() {
    for (fate, exit, reason, said) in [
        (FrontendFate::Busy, 4, "frontend_busy", "update_open"),
        (
            FrontendFate::Refuses("old_child_draining"),
            4,
            "frontend_busy",
            "old_child_draining",
        ),
        (
            FrontendFate::Refuses("installer_busy"),
            4,
            "frontend_busy",
            "installer_busy",
        ),
        (
            FrontendFate::Refuses("incompatible"),
            3,
            "incompatible",
            "status 409 incompatible",
        ),
        (
            FrontendFate::Incomplete,
            3,
            "release_incomplete",
            "status 422 incomplete_release: not now",
        ),
        (
            FrontendFate::Refuses("state_lost"),
            1,
            "api_error",
            "state_lost",
        ),
        (
            FrontendFate::Refuses("request_id_reused"),
            1,
            "api_error",
            "request_id_reused",
        ),
        (
            FrontendFate::Refuses("request_id_retired"),
            1,
            "api_error",
            "request_id_retired",
        ),
    ] {
        let f = with_fate(fate);
        let s = tls(&f, &opts(true));
        assert_eq!(
            (s.exit, s.reason.as_str()),
            (exit, reason),
            "{reason} {said}"
        );
        assert!(s.detail.contains(said), "{said}: {}", s.detail);
        assert_eq!(
            f.requests("POST /v1/update/frontend"),
            1,
            "{said}: not retried"
        );
        assert_eq!(f.requests("GET /v1/nodes"), 1, "{said}");
        assert_eq!(s.post, PostResult::default());
    }
}

/// Proves an answer the API has no use for here is a failure at once, with its status: a bad
/// request, a refused token, a route an older proxy does not have, a body over the limit, an
/// accepted answer that names no update. A busy answer with no body is still busy. Catches a
/// deploy that retries what will not change or reads an empty refusal as success.
///
/// Mutants planted: 401 retried; a 202 with no update id taken for an update.
#[test]
fn any_other_answer_is_a_failure_with_its_status() {
    for status in [400, 401, 403, 404, 405, 413] {
        let mut f = farm(two_nodes());
        f.broken.push(Broken::new(
            "POST /v1/update/frontend",
            Breaks::Status(status),
        ));
        let s = tls(&f, &opts(true));
        assert_eq!((s.exit, s.reason.as_str()), (1, "api_error"), "{status}");
        assert!(
            s.detail.contains(&format!("status {status}")),
            "{}",
            s.detail
        );
        assert_eq!(f.requests("POST"), 1, "{status}");
    }
    let mut f = farm(two_nodes());
    f.broken.push(Broken::new(
        "POST /v1/update/frontend",
        Breaks::Raw(409, "an update is open"),
    ));
    let s = tls(&f, &opts(true));
    assert_eq!((s.exit, s.reason.as_str()), (4, "frontend_busy"));
    let mut f = farm(two_nodes());
    f.broken.push(Broken::new(
        "POST /v1/update/frontend",
        Breaks::Raw(202, "{}"),
    ));
    let s = tls(&f, &opts(true));
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "api_error"),
        "{}",
        s.line()
    );
    assert!(s.detail.contains("names no update"), "{}", s.detail);
    assert_eq!(f.requests("GET /v1/nodes"), 1);
}

/// Proves a 200 is an accepted update like a 202. Catches a client that insists on one
/// status of an API whose replay of a request id may answer another.
///
/// Mutants planted: 200 refused.
#[test]
fn a_200_is_an_accepted_update_too() {
    let mut f = farm(two_nodes());
    f.accepted_status = 200;
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
}

/// Proves a POST that did not get through is made again with the same request id, and that
/// the API then has one update: after a 503, a 429, a connection that failed, and an answer
/// that was lost after the API had accepted the request (the case the id is for). Catches a
/// retry with a new id (the API says an update is open: exit 4), no retry, and a retry that
/// starts a second update.
///
/// Mutants planted: a new request id for each attempt; 5xx and 429 not retried; the retry
/// loop ending after one attempt.
#[test]
fn a_post_that_did_not_get_through_is_retried_under_the_same_request_id() {
    for (how, times) in [
        (Breaks::Status(503), 2),
        (Breaks::Status(429), 1),
        (Breaks::Status(502), 1),
        (Breaks::Io, 2),
        (Breaks::Lost, 1),
        (Breaks::Lost, 3),
    ] {
        let mut f = farm(two_nodes());
        f.broken
            .push(Broken::new("POST /v1/update/frontend", how).times(times));
        let s = tls(&f, &opts(true));
        assert_eq!(s.exit, 0, "{times} times: {}", s.line());
        assert_eq!(f.requests("POST /v1/update/frontend"), times + 1);
        // What reached the API (a lost answer did; a refused call did not) names one id.
        let posted = f.posted.lock().unwrap().clone();
        assert!(!posted.is_empty());
        assert!(posted.iter().all(|p| p.request_id == REQUEST_ID));
        assert_eq!(s.frontend.unwrap().update_id.as_deref(), Some("u-1"));
        // The wait between attempts is the poll interval.
        let first = f.first_at("GET /v1/update/frontend", "POST /v1/update/frontend");
        assert_eq!(first, Some(5 * times as u64));
    }
}

/// Proves an API that never accepts the request ends the deploy at the deadline, after the
/// attempts the interval allows, as an API error that says why. Catches a loop with no end
/// and an off-by-one in the deadline.
///
/// Mutants planted: `>=` for `>` in the deadline test; the last error dropped.
#[test]
fn an_api_that_never_accepts_fails_at_the_deadline() {
    let mut f = farm(two_nodes());
    f.broken
        .push(Broken::new("POST /v1/update/frontend", Breaks::Status(503)));
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(60);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "api_error"),
        "{}",
        s.line()
    );
    assert!(s.detail.contains("no update was accepted"), "{}", s.detail);
    assert!(s.detail.contains("status 503"), "{}", s.detail);
    // Every five seconds from 0 to 60 s.
    assert_eq!(f.requests("POST /v1/update/frontend"), 13);
    assert_eq!(f.now.get(), 60);
    assert!(s.frontend.is_none());
}

/// Proves an update that does not end is waited for until the deadline, which then names the
/// phase it was in, and that the nodes are not looked at meanwhile. Catches a wait with no
/// end, a deadline counted from the wrong moment, and a timeout that loses the phase.
///
/// Mutants planted: the deadline test `>` for `>=`; the phase left out of the detail.
#[test]
fn an_update_that_hangs_ends_at_the_deadline_naming_its_phase() {
    let f = with_fate(FrontendFate::Hang);
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(120);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_timeout"),
        "{}",
        s.line()
    );
    assert_eq!(
        s.detail,
        "update u-1 was in phase baking when the 120 s ran out"
    );
    assert_eq!(s.frontend.unwrap().phase, Some(Phase::Baking));
    assert_eq!(f.now.get(), 120);
    assert_eq!(f.requests("GET /v1/nodes"), 1);
    assert_eq!(s.post, PostResult::default());
}

/// Proves only `done` ends the step well: an update the API shows in any earlier phase,
/// the last one before `done` among them, is still going on. Catches a deploy that takes the
/// offer of the commit to the nodes (the `nodes` phase) or any other phase for the end, and
/// would check a farm whose old frontend still drains.
///
/// Mutants planted: `nodes`, `draining`, `switched` and `baking` each taken for success.
#[test]
fn nothing_but_done_is_success() {
    let f = with_fate(FrontendFate::StuckInNodes);
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(120);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_timeout"),
        "{}",
        s.line()
    );
    assert_eq!(s.frontend.unwrap().phase, Some(Phase::Nodes));
    // The commit was offered, and the nodes were still not looked at.
    assert!(f.desired_at().is_some());
    assert_eq!(f.requests("GET /v1/nodes"), 1);
    // The farm's own timeline reaches every phase before `done` on the way.
    let f = farm(two_nodes());
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(step(&s, "frontend").ms, DONE_AT * 1000);
    assert!(FRONTEND_TIMELINE.iter().all(|(at, _)| *at <= DONE_AT));
}

/// Proves a status of another update is not this deploy's: while the API shows the update
/// before ours as `done` (a status that lags the POST, or an update a person started), the
/// deploy waits for its own. Catches a deploy that takes any `done` for its own, which
/// returns at the first poll, and a deploy that never gives up on a status that is not ours.
///
/// Mutants planted: the update id not compared; compared with the wrong one.
#[test]
fn the_status_of_another_update_is_not_ours() {
    let f = with_fate(FrontendFate::Lagging(30));
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    // Not at the first poll, where the API still shows `u-0` as `done`, but at our own `done`.
    assert_eq!(step(&s, "frontend").ms, DONE_AT * 1000);
    assert_eq!(
        f.first_at("GET /v1/nodes", "POST /v1/update/frontend"),
        Some(DONE_AT)
    );

    let f = with_fate(FrontendFate::Lagging(1_000_000));
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(60);
    let s = tls(&f, &o);
    assert_eq!((s.exit, s.reason.as_str()), (1, "frontend_timeout"));
    assert!(
        s.detail
            .contains("was not shown (the API shows another update, not u-1)"),
        "{}",
        s.detail
    );
    assert!(s.frontend.is_none());
}

/// Proves a status that cannot be had right now is asked for again (the children behind the
/// proxy swap during the update), and one that is not an update status stops the deploy: no
/// wait will make it one. Catches a deploy that fails on one dropped status call, and one
/// that polls an hour for a body it cannot read.
///
/// Mutants planted: an unanswered status ending the deploy; an unreadable status retried.
#[test]
fn a_status_call_that_fails_is_repeated_and_one_that_cannot_be_read_stops_the_deploy() {
    for how in [Breaks::Io, Breaks::Status(503), Breaks::Raw(200, "")] {
        let mut f = farm(two_nodes());
        f.broken.push(
            Broken::new("GET /v1/update/frontend", how)
                .after_post()
                .times(3),
        );
        let s = tls(&f, &opts(true));
        // The empty 200 is the one that cannot be read; the others are asked for again.
        let readable = !matches!(how, Breaks::Raw(..));
        assert_eq!(s.exit == 0, readable, "{}", s.line());
        if !readable {
            assert_eq!(s.reason, "api_error");
            assert!(s.detail.contains("not an update status"), "{}", s.detail);
            // The preflight's read and the one that failed.
            assert_eq!(f.requests("GET /v1/update/frontend"), 2);
        }
    }
    let mut f = farm(two_nodes());
    f.broken
        .push(Broken::new("GET /v1/update/frontend", Breaks::Status(500)).after_post());
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(30);
    let s = tls(&f, &o);
    assert_eq!((s.exit, s.reason.as_str()), (1, "frontend_timeout"));
    assert!(
        s.detail
            .contains("was not shown (GET /v1/update/frontend: status 500)"),
        "{}",
        s.detail
    );
}

/// Proves the frontend update in flight before this deploy decides the preflight by its
/// phase: any phase before `done` is busy (exit 4, nothing posted), and `done`, `failed`,
/// `rolled_back` and no update at all are not. Catches a busy test that counts a failed
/// update as open, or a draining one as over.
///
/// Mutants planted: `is_terminal` replaced by `== Done`; the phase `draining` left out of the
/// busy ones.
#[test]
fn only_an_update_still_going_on_makes_the_preflight_busy() {
    use Phase::*;
    let open = [
        Accepted, Fetching, Verifying, Staged, Preflight, Starting, Ready, Holding, Quiescing,
        Switching, Switched, Baking, Nodes, Draining,
    ];
    for phase in open {
        let mut f = farm(two_nodes());
        f.previous = Some(phase);
        let s = run_with(&f, &opts(false), SmokeResult::Pass, None);
        assert_eq!(
            (s.exit, s.reason.as_str()),
            (4, "frontend_busy"),
            "{phase:?}"
        );
        assert_eq!(f.requests("POST"), 0);
    }
    for previous in [Some(Done), Some(Failed), Some(RolledBack), None] {
        let mut f = farm(two_nodes());
        f.previous = previous;
        let s = run_with(&f, &opts(false), SmokeResult::Pass, None);
        assert_eq!((s.exit, s.result.as_str()), (0, "plan"), "{previous:?}");
    }
}

/// Proves a frontend that already runs the commit (the API's `same_commit`) is no failure
/// and no update: the deploy goes on to watch the nodes and check the farm, so that a run
/// repeated after a node was fixed finishes. It still does not offer the commit itself: when
/// the desired release is another, the deploy says the nodes were never offered.
///
/// Mutants planted: `same_commit` treated as busy; as a failure; the offer posted by the deploy.
#[test]
fn a_frontend_that_already_runs_the_commit_goes_on_to_the_nodes() {
    let mut f = with_fate(FrontendFate::Refuses("same_commit"));
    f.offered_at_start = true;
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(
        s.frontend,
        Some(Frontend {
            already_running: true,
            ..Frontend::default()
        })
    );
    let json: Value = serde_json::from_str(&s.line()).unwrap();
    assert_eq!(json["frontend"]["already_running"], true);
    assert_eq!(json["frontend"]["update_id"], Value::Null);
    assert_eq!(f.requests("POST /v1/update/frontend"), 1);
    // Only the preflight asked for a status: there is no update to follow.
    assert_eq!(f.requests("GET /v1/update/frontend"), 1);
    assert_eq!(s.nodes.len(), 2);

    let f = with_fate(FrontendFate::Refuses("same_commit"));
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(60);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "nodes_not_offered"),
        "{}",
        s.line()
    );
    assert_eq!(f.requests("POST /v1/update/desired"), 0);
}

/// Proves the nodes are rolled by the new frontend and only watched: when the update is
/// `done` and the desired release is still not the commit (the post of the offer failed,
/// which the update does not count against it) the deploy waits for the offer for a node's
/// budget and then fails saying what the desired release is; an unreadable or empty answer is
/// said so. It posts no desired release itself, whatever the state of the farm. Catches a
/// deploy that offers the commit on the frontend's behalf, and one that waits for nodes that
/// were never offered anything.
///
/// Mutants planted: `POST /v1/update/desired` made when the offer is missing; the wait for
/// the offer dropped.
#[test]
fn a_commit_the_new_frontend_never_offered_fails_and_is_not_offered_by_the_deploy() {
    let mut f = with_fate(FrontendFate::NoOffer);
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(600);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "nodes_not_offered"),
        "{}",
        s.line()
    );
    assert_eq!(
        s.detail,
        format!(
            "the new frontend did not offer the commit to the nodes: the desired release is {OLD}"
        )
    );
    assert_eq!(s.generation, None);
    assert!(s.nodes.is_empty());
    assert_eq!(s.post, PostResult::default());
    // It waited a node's budget from `done`.
    assert_eq!(f.now.get(), DONE_AT + 600);
    assert_eq!(f.requests("POST /v1/update/desired"), 0);
    assert_eq!(f.requests("POST"), 1);

    for (how, said) in [
        (Breaks::Body("{}"), "no desired release is set"),
        (Breaks::Io, "GET /v1/update: "),
        (Breaks::Status(500), "GET /v1/update: status 500"),
    ] {
        f = farm(two_nodes());
        f.broken
            .push(Broken::new("GET /v1/update", how).after_post());
        let s = tls(&f, &o);
        assert_eq!((s.exit, s.reason.as_str()), (1, "nodes_not_offered"));
        assert!(s.detail.contains(said), "{said}: {}", s.detail);
        assert_eq!(f.requests("POST /v1/update/desired"), 0);
    }
}

/// Proves the deploy never sets the desired release and never orders a node, whatever the
/// API does: not on a clean run, not when the update fails, hangs, is refused or finishes
/// without the offer. Catches the old nodes-first path coming back in any branch.
///
/// Mutants planted: `POST /v1/update/desired` added to the clean path, to the failure path and
/// to the no-offer path (each makes one of these runs red).
#[test]
fn the_deploy_never_posts_the_desired_release_or_a_node_order() {
    for fate in [
        FrontendFate::Done,
        FrontendFate::Failed,
        FrontendFate::RolledBack,
        FrontendFate::Hang,
        FrontendFate::Busy,
        FrontendFate::Refuses("same_commit"),
        FrontendFate::NoOffer,
        FrontendFate::Lagging(10),
        FrontendFate::StuckInNodes,
    ] {
        let f = with_fate(fate);
        let mut o = opts(true);
        o.handover_timeout = Duration::from_secs(60);
        o.node_timeout = Duration::from_secs(60);
        let s = tls(&f, &o);
        assert_eq!(f.requests("POST /v1/update/desired"), 0, "{}", s.line());
        assert_eq!(f.requests("POST /v1/nodes"), 0);
        assert_eq!(f.requests("POST /v1/update/pause"), 0);
        assert!(f.requests("POST") <= 1);
    }
}

/// Proves the roll is waited for on every node the preflight saw, listed or not: the new
/// frontend lists only the nodes that have reconnected to it, so a list of the nodes that
/// have confirmed is not a roll that has finished. A node that does not come back times out
/// with the platform the preflight knew, and one allowed to be offline does not hold the
/// deploy. Catches a wait that ends when the nodes it can see are done.
///
/// Mutants planted: the preflight's nodes not waited for; the platform of an unlisted node
/// lost; an unlisted node that may be offline waited for.
#[test]
fn every_node_the_preflight_saw_is_waited_for_listed_or_not() {
    let mut late = node("b-mac", "darwin-arm64", Fate::Confirm(300));
    late.relisted_after = Some(150);
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        late.clone(),
    ]);
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.state.as_str(), n.secs))
        .collect();
    // `a` confirms 100 s after the offer, 80 s into the wait; `b` 300 s after, 200 s later.
    assert_eq!(
        by,
        [("a-store", "confirmed", 80), ("b-mac", "confirmed", 200)]
    );
    assert_eq!(f.now.get(), OFFER_AT + 300);

    // A node that never comes back to the new frontend.
    late.relisted_after = Some(1_000_000);
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        late.clone(),
    ]);
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(600);
    let s = tls(&f, &o);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );
    assert_eq!(
        s.detail,
        "b-mac: timed_out; the farm is left mixed and the host held"
    );
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.platform.as_str(), n.state.as_str()))
        .collect();
    assert_eq!(
        by,
        [
            ("a-store", "linux-x86_64", "confirmed"),
            ("b-mac", "darwin-arm64", "timed_out")
        ]
    );

    // The same node may be offline: it is not waited for.
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        late.clone(),
    ]);
    let mut o = opts(true);
    o.allow_offline = vec!["b-mac".to_owned()];
    let s = tls(&f, &o);
    assert_eq!(s.exit, 0, "{}", s.line());
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.platform.as_str(), n.state.as_str()))
        .collect();
    assert_eq!(
        by,
        [
            ("a-store", "linux-x86_64", "confirmed"),
            ("b-mac", "darwin-arm64", "offline")
        ]
    );

    // A node that is not unlisted and not first to fail is `pending`, with its platform.
    let mut third = late;
    third.id = "c-store";
    third.platform = "linux-x86_64";
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Revert(50)),
        third,
    ]);
    let s = tls(&f, &opts(true));
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.platform.as_str(), n.state.as_str()))
        .collect();
    assert_eq!(
        by,
        [
            ("a-store", "linux-x86_64", "reverted"),
            ("c-store", "linux-x86_64", "pending")
        ]
    );

    // A node that may be offline but is listed and connected is waited for like the others.
    let f = farm(vec![node("a-store", "linux-x86_64", Fate::Confirm(100))]);
    let mut o = opts(true);
    o.allow_offline = vec!["a-store".to_owned()];
    let s = tls(&f, &o);
    assert_eq!(s.exit, 0, "{}", s.line());
    assert_eq!(
        (s.nodes[0].state.as_str(), s.nodes[0].secs),
        ("confirmed", 80)
    );
}

/// Proves the contract with the API's own types in both directions: the status the fake
/// builds from `StatusBody` (and a field this client does not know) is read, and the request
/// it parses is `UpdateRequest`. A key renamed on either side is a failure here.
///
/// Mutants planted: `Deserialize` of `StatusBody` with `deny_unknown_fields`.
#[test]
fn the_deploy_and_the_api_share_their_types() {
    let f = farm(two_nodes());
    let s = tls(&f, &opts(true));
    assert_eq!(s.exit, 0, "{}", s.line());
    let whole = StatusBody {
        update_id: Some(UpdateId("u-9".to_owned())),
        from: Some(c(OLD)),
        to: Some(c(TARGET)),
        phase: Some(Phase::Draining),
        since: Some(7),
        active_slot: Some(crate::proxy::model::Slot::B),
        restart_pending: true,
        rollback_allowed: false,
        rollback_blocked_reason: Some(RollbackBlocked::NodesUpdated),
        old: Some(crate::proxy::api::model::OldChild::default()),
        new: Some(crate::proxy::api::model::NewChild::default()),
        failure: Some(failure(FailedStep::Confirm, "x")),
    };
    let text = serde_json::to_string(&whole).unwrap();
    assert_eq!(serde_json::from_str::<StatusBody>(&text).unwrap(), whole);
    // An API that has no update says so with nulls.
    let idle = r#"{"update_id":null,"from":null,"to":null,"phase":null,"since":null,
        "active_slot":null,"restart_pending":false,"rollback_allowed":false,
        "rollback_blocked_reason":"no_update","old":null,"new":null,"failure":null}"#;
    let idle: StatusBody = serde_json::from_str(idle).unwrap();
    assert_eq!(idle.phase, None);
}
