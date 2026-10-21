//! `kbf-installer update --frontend` against a played update API: the request it sends, and
//! the exit code and the one summary line for each way the update can end.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use clap::Parser;
use kbf_update::Source;
use serde_json::{Value, json};

use super::api::{Operator, Reply, Request};
use super::observe::{NoObserve, Observe};
use super::update::{finish, installer_exit, plan, request_id, run};
use super::{Check, Clock, Env, NoCheck, NoProbe, NoSmoke, Options};
use crate::cli::{Cli, Verb};
use crate::exit::Exit;
use crate::testkit::{A, B, c};

const ID: &str = "update-fixed-id";

/// A played API. POSTs are answered from `posts` (the last answer repeats); GETs of the status
/// likewise from `gets`.
struct Api {
    posts: RefCell<VecDeque<io::Result<Reply>>>,
    gets: RefCell<VecDeque<Reply>>,
    sent: RefCell<Vec<Request>>,
}

fn reply(status: u16, body: Value) -> Reply {
    Reply {
        status,
        body: serde_json::to_vec(&body).expect("json"),
    }
}

fn accepted() -> io::Result<Reply> {
    Ok(reply(
        202,
        json!({"update_id": "upd-1", "phase": "accepted"}),
    ))
}

fn refused(status: u16, code: &str) -> io::Result<Reply> {
    Ok(reply(status, json!({"error": code, "message": "words"})))
}

fn status(phase: &str) -> Reply {
    let mut body = json!({
        "update_id": "upd-1", "from": A, "to": B, "phase": phase, "since": 1,
        "active_slot": "b", "restart_pending": false, "rollback_allowed": false,
        "rollback_blocked_reason": null, "old": null, "new": null, "failure": null,
    });
    if phase == "failed" {
        body["failure"] = json!({"step": "switch", "reason": "takeover: refused"});
    }
    reply(200, body)
}

impl Api {
    fn new(posts: Vec<io::Result<Reply>>, gets: Vec<Reply>) -> Api {
        Api {
            posts: RefCell::new(posts.into()),
            gets: RefCell::new(gets.into()),
            sent: RefCell::new(Vec::new()),
        }
    }

    fn posted(&self) -> Vec<Value> {
        self.sent
            .borrow()
            .iter()
            .filter(|r| r.method == "POST")
            .map(|r| serde_json::from_slice(&r.body).expect("a JSON body"))
            .collect()
    }
}

fn take<T: Clone>(queue: &mut VecDeque<T>) -> T {
    if queue.len() > 1 {
        queue.pop_front().expect("one")
    } else {
        queue.front().cloned().expect("an answer was played")
    }
}

impl Operator for Api {
    fn send(&self, request: &Request) -> io::Result<Reply> {
        self.sent.borrow_mut().push(request.clone());
        if request.method == "POST" {
            let mut q = self.posts.borrow_mut();
            if q.len() > 1 {
                return q.pop_front().expect("one");
            }
            return match q.front() {
                Some(Ok(r)) => Ok(r.clone()),
                Some(Err(e)) => Err(io::Error::new(e.kind(), e.to_string())),
                None => Err(io::Error::other("nothing played")),
            };
        }
        Ok(take(&mut self.gets.borrow_mut()))
    }
}

struct Clk(Cell<u64>);

impl Clock for Clk {
    fn now(&self) -> u64 {
        1_790_000_000 + self.0.get()
    }

    fn sleep(&self, d: Duration) {
        self.0.set(self.0.get() + d.as_secs());
    }
}

fn options(nodes: bool) -> Options {
    let mut o = Options::new(c(B), "linux-x86_64", ID.to_owned());
    o.nodes = nodes;
    o.handover_timeout = Duration::from_secs(60);
    o
}

/// Run `update` against `api`; the exit and the one summary line.
fn go(api: &Api, nodes: bool) -> (Exit, Value) {
    go_with(api, &options(nodes))
}

/// [`go`] with the options the test chose.
fn go_with(api: &Api, options: &Options) -> (Exit, Value) {
    let clock = Clk(Cell::new(0));
    let check = NoCheck;
    let env = Env {
        api,
        observe: &NoObserve,
        clock: &clock,
        smoke: &NoSmoke,
        check: &check as &dyn Check,
        probe: &NoProbe,
    };
    let mut out = Vec::new();
    let exit = finish(&env, options, &mut out);
    let text = String::from_utf8(out).expect("text");
    assert_eq!(text.lines().count(), 1, "one summary line: {text:?}");
    (exit, serde_json::from_str(&text).expect("the line is JSON"))
}

#[test]
fn a_done_update_exits_zero_and_asks_for_the_frontend_only_by_default() {
    let api = Api::new(vec![accepted()], vec![status("switching"), status("done")]);
    let (exit, line) = go(&api, false);
    assert_eq!(exit, Exit::Ok);
    assert_eq!(line["frontend"]["phase"], "done");
    assert_eq!(line["frontend"]["update_id"], "upd-1");
    assert_eq!(
        api.posted(),
        [json!({"commit": B, "request_id": ID, "nodes": false, "rollback": false})]
    );
    assert_eq!(line["steps"].as_array().map(Vec::len), Some(1), "one step");
}

#[test]
fn nodes_are_asked_for_only_when_the_flag_says_so() {
    let api = Api::new(vec![accepted()], vec![status("done")]);
    go(&api, true);
    assert_eq!(api.posted()[0]["nodes"], true);
}

#[test]
fn a_frontend_that_already_runs_the_commit_exits_zero_and_says_so() {
    let api = Api::new(vec![refused(409, "same_commit")], vec![]);
    let (exit, line) = go(&api, false);
    assert_eq!(exit, Exit::Ok);
    assert_eq!(line["frontend"]["already_running"], true);
    assert_eq!(api.posted().len(), 1, "nothing is polled");
}

#[test]
fn a_release_the_api_refuses_exits_three() {
    for refusal in [
        refused(409, "incompatible"),
        refused(422, "incomplete_release"),
    ] {
        let api = Api::new(vec![refusal], vec![]);
        let (exit, line) = go(&api, false);
        assert_eq!(exit, Exit::Refused, "{line}");
        assert_eq!(line["exit"], 3);
    }
}

#[test]
fn an_update_that_cannot_start_now_exits_four() {
    for code in ["update_open", "old_child_draining", "installer_busy"] {
        let api = Api::new(vec![refused(409, code)], vec![]);
        let (exit, line) = go(&api, false);
        assert_eq!(exit, Exit::Busy, "{code}: {line}");
        assert_eq!(line["reason"], "frontend_busy");
    }
}

#[test]
fn a_failed_or_rolled_back_update_exits_one_with_the_step_the_api_named() {
    let api = Api::new(vec![accepted()], vec![status("failed")]);
    let (exit, line) = go(&api, false);
    assert_eq!(exit, Exit::Failed);
    assert_eq!(line["reason"], "frontend_failed");
    let detail = line["detail"].as_str().expect("detail");
    assert!(
        detail.contains("switch") && detail.contains("takeover"),
        "{detail}"
    );
    let api = Api::new(vec![accepted()], vec![status("rolled_back")]);
    let (exit, line) = go(&api, false);
    assert_eq!(
        (exit, &line["reason"]),
        (Exit::Failed, &json!("frontend_rolled_back"))
    );
}

#[test]
fn an_update_that_does_not_end_in_time_exits_one() {
    let api = Api::new(vec![accepted()], vec![status("baking")]);
    let (exit, line) = go(&api, false);
    assert_eq!(exit, Exit::Failed);
    assert_eq!(line["reason"], "frontend_timeout");
}

#[test]
fn an_api_that_never_answers_exits_one() {
    let api = Api::new(vec![Err(io::Error::other("connection refused"))], vec![]);
    let (exit, line) = go(&api, false);
    assert_eq!(exit, Exit::Failed);
    assert_eq!(line["reason"], "api_error");
}

#[test]
fn a_run_repeated_after_a_lost_connection_sends_the_same_request_id() {
    // The first POST is lost, the second is answered: both are the same update's request.
    let api = Api::new(
        vec![Err(io::Error::other("reset by peer")), accepted()],
        vec![status("done")],
    );
    let (exit, _) = go(&api, false);
    assert_eq!(exit, Exit::Ok);
    let bodies = api.posted();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[0]["request_id"], ID);
}

fn args(extra: &[&str]) -> crate::cli::UpdateArgs {
    let mut full = vec!["kbf-installer", "update", "--commit", B, "--frontend"];
    full.extend_from_slice(extra);
    match Cli::try_parse_from(full).expect("parses").verb {
        Verb::Update(a) => a,
        _ => panic!("not update"),
    }
}

fn usage(extra: &[&str]) -> String {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = run(&args(extra), &mut out, &mut err);
    assert_eq!(exit, Exit::Usage);
    assert!(out.is_empty(), "no summary for a usage error");
    String::from_utf8(err).expect("text")
}

#[test]
fn the_flags_default_to_the_frontend_only_and_the_setup_token_path() {
    let a = args(&["--api", "127.0.0.1:8443"]);
    assert!(!a.nodes);
    assert_eq!(a.token_file, std::path::Path::new("/etc/kbf/api-token"));
    assert_eq!(a.handover_timeout_s, 3600);
    assert_eq!(a.request_id, None);
    let a = args(&["--api", "127.0.0.1:8443", "--nodes", "--request-id", "x1"]);
    assert!(a.nodes);
    assert_eq!(a.request_id.as_deref(), Some("x1"));
}

#[test]
fn a_bad_address_or_token_is_a_usage_error_before_anything_is_sent() {
    assert!(usage(&["--api", "not-an-address"]).contains("host:port"));
    let dir = kbf_host::testkit::scratch("update-token");
    let token = dir.join("token");
    let path = token.to_str().expect("utf8");
    let why = usage(&["--api", "127.0.0.1:8443", "--token-file", path]);
    assert!(why.contains("token file"), "{why}");
    std::fs::write(&token, "x".repeat(40)).expect("write");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let why = usage(&["--api", "127.0.0.1:8443", "--token-file", path]);
    assert!(why.contains("0600"), "{why}");
}

#[test]
fn the_summarys_exit_is_the_installers_by_code() {
    use super::summary::Exit as Summary;
    for exit in [
        Summary::Ok,
        Summary::Plan,
        Summary::Failed,
        Summary::Usage,
        Summary::Refused,
        Summary::Busy,
        Summary::NeedsOperator,
    ] {
        assert_eq!(
            i32::from(installer_exit(exit).code()),
            exit.code(),
            "{exit:?}"
        );
    }
}

fn no_random(_: usize) -> Result<Vec<u8>, String> {
    Err("no entropy".into())
}

fn fixed_random(n: usize) -> Result<Vec<u8>, String> {
    Ok(vec![0xab; n])
}

#[test]
fn a_request_id_is_the_one_given_or_a_random_one_and_no_random_is_a_usage_error() {
    assert_eq!(request_id(Some("mine"), no_random), Ok("mine".to_owned()));
    let made = request_id(None, fixed_random).expect("made");
    assert_eq!(made, format!("update-{}", "ab".repeat(16)));
    let why = request_id(None, no_random).expect_err("refused");
    assert!(why.contains("no entropy"), "{why}");
}

fn token_in(name: &str) -> std::path::PathBuf {
    let dir = kbf_host::testkit::scratch(name);
    let path = dir.join("token");
    std::fs::write(&path, "t".repeat(40)).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    path
}

#[test]
fn an_unreadable_or_empty_ca_file_is_a_usage_error() {
    let token = token_in("update-ca");
    let token = token.to_str().expect("utf8");
    let why = usage(&[
        "--api",
        "127.0.0.1:8443",
        "--token-file",
        token,
        "--api-ca",
        "/no/such/ca.pem",
    ]);
    assert!(why.contains("--api-ca"), "{why}");
    let empty = std::path::Path::new(token).with_file_name("ca.pem");
    std::fs::write(&empty, "not a certificate").expect("write");
    let why = usage(&[
        "--api",
        "127.0.0.1:8443",
        "--token-file",
        token,
        "--api-ca",
        empty.to_str().expect("utf8"),
    ]);
    assert!(why.contains("--api"), "{why}");
}

#[test]
fn a_proxy_that_does_not_answer_ends_the_run_with_one_summary_line_and_exit_one() {
    // Nothing listens on port 1: every call is refused, and the time given runs out.
    let token = token_in("update-refused");
    let a = args(&[
        "--api",
        "127.0.0.1:1",
        "--token-file",
        token.to_str().expect("utf8"),
        "--handover-timeout-s",
        "1",
        "--request-id",
        ID,
    ]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(run(&a, &mut out, &mut err), Exit::Failed);
    let line: Value = serde_json::from_slice(&out).expect("one JSON line");
    assert_eq!(line["reason"], "api_error");
    assert_eq!(line["commit"], B);
    assert!(String::from_utf8(err).expect("text").contains(B));
}

/// `update` runs no gate, so it is given no frontend to read: asking for one is an error, not a
/// read of whichever frontend happens to answer.
#[test]
fn the_update_verb_has_no_frontend_to_observe() {
    let why = NoObserve.resolve(None).err().expect("no frontend");
    assert_eq!(why, "this verb observes no frontend");
}

/// The options of an update whose release is the one the host's package carries.
fn staged(nodes: bool) -> Options {
    Options {
        source: Source::Staged,
        ..options(nodes)
    }
}

/// The status of update `upd-1`, failed at `step` for `reason`.
fn failed_at(step: &str, reason: &str) -> Reply {
    reply(
        200,
        json!({
            "update_id": "upd-1", "from": A, "to": B, "phase": "failed", "since": 1,
            "active_slot": "b", "restart_pending": false, "rollback_allowed": false,
            "rollback_blocked_reason": null, "old": null, "new": null,
            "failure": {"step": step, "reason": reason},
        }),
    )
}

/// Catches a staged update whose request does not say so, which the proxy would take for a
/// fetch and answer by fetching. The request of an update that fetches is held to its four
/// keys by `a_done_update_exits_zero_and_asks_for_the_frontend_only_by_default`, which a
/// client that always sent `source` breaks (and so would every proxy that has not restarted
/// since its package was upgraded).
///
/// Mutants planted: the source never sent; `fetch` sent as a word.
#[test]
fn a_staged_update_names_its_source_in_the_request() {
    let api = Api::new(vec![accepted()], vec![status("done")]);
    let (exit, _) = go_with(&api, &staged(false));
    assert_eq!(exit, Exit::Ok);
    let want = json!({
        "commit": B, "request_id": ID, "nodes": false, "rollback": false, "source": "staged",
    });
    assert_eq!(api.posted(), [want]);
}

/// Catches the new refusals of the API falling into the arms made for the old ones: today any
/// other 409 is a busy frontend (exit 4, try again) and any 422 an incomplete release. A
/// source the host refuses, or a package that carries no such release, is not something a
/// second try fixes: exit 3, under the API's own code.
///
/// Mutants planted: the 409 arm deleted (exit 4); the 422 arm deleted (reason
/// `release_incomplete`).
#[test]
fn a_source_the_api_refuses_exits_three_under_its_own_reason() {
    for (code, reason) in [(409, "source_refused"), (422, "not_staged")] {
        let api = Api::new(vec![refused(code, reason)], vec![]);
        let (exit, line) = go_with(&api, &staged(false));
        assert_eq!(exit, Exit::Refused, "{reason}: {line}");
        assert_eq!(line["exit"], 3, "{reason}");
        assert_eq!(line["reason"], reason);
    }
}

/// Catches an update that ended `failed` at the order for the nodes exiting as any failure:
/// the frontend was swapped and the nodes were not told, and the child said why. It is exit 3
/// with the step's own reason and the child's words in the detail; any other step is still
/// exit 1.
///
/// Mutants planted: the step not looked at (exit 1, `frontend_failed`); the step taken for a
/// success (exit 0).
#[test]
fn an_update_that_failed_at_the_order_for_the_nodes_exits_three_with_the_childs_words() {
    let api = Api::new(
        vec![accepted()],
        vec![failed_at("nodes", "not_own: not the child's own release")],
    );
    let (exit, line) = go_with(&api, &options(true));
    assert_eq!(exit, Exit::Refused, "{line}");
    assert_eq!(line["exit"], 3);
    assert_eq!(line["reason"], "nodes_refused");
    let detail = line["detail"].as_str().expect("detail");
    assert!(detail.contains("not_own"), "{detail}");

    let api = Api::new(vec![accepted()], vec![failed_at("confirm", "x")]);
    let (exit, line) = go_with(&api, &options(true));
    assert_eq!(
        (exit, &line["reason"]),
        (Exit::Failed, &json!("frontend_failed"))
    );
}

/// Catches `--source` read as anything but the two words, or dropped on the way to the
/// request: the flag defaults to `fetch`, takes `fetch` and `staged`, and no other word.
///
/// Mutants planted: the default `staged`; any word read as `fetch`.
#[test]
fn the_source_flag_is_fetch_or_staged_and_defaults_to_fetch() {
    assert_eq!(args(&["--api", "127.0.0.1:8443"]).source, Source::Fetch);
    let staged = args(&["--api", "127.0.0.1:8443", "--source", "staged"]);
    assert_eq!(staged.source, Source::Staged);
    let fetch = args(&["--api", "127.0.0.1:8443", "--source", "fetch"]);
    assert_eq!(fetch.source, Source::Fetch);
    for bad in ["bogus", "Staged", ""] {
        let mut full = vec!["kbf-installer", "update", "--commit", B, "--frontend"];
        full.extend_from_slice(&["--api", "127.0.0.1:8443", "--source", bad]);
        assert!(Cli::try_parse_from(full).is_err(), "{bad:?}");
    }
}

/// Catches the flag stopping short of the request: the options the verb builds carry the
/// source the flag named.
///
/// Mutant planted: `options.source` left at its default.
#[test]
fn the_source_of_the_flags_is_the_source_of_the_options() {
    let token = token_in("update-source");
    let token = token.to_str().expect("utf8");
    let base = ["--api", "127.0.0.1:8443", "--token-file", token];
    let (_, fetching) = plan(&args(&base), fixed_random).expect("a plan");
    assert_eq!(fetching.source, Source::Fetch);
    let mut named = base.to_vec();
    named.extend_from_slice(&["--source", "staged"]);
    let (_, packaged) = plan(&args(&named), fixed_random).expect("a plan");
    assert_eq!(packaged.source, Source::Staged);
}

/// Catches a staged update that asks for the nodes getting as far as the proxy: the order for
/// the nodes does not name its source yet, so the flags are refused (exit 2) before the token
/// is read or anything is sent, naming both flags.
///
/// Mutant planted: the refusal dropped (the run goes on to read the token file, which is not
/// there).
#[test]
fn a_staged_update_takes_no_nodes_yet_and_the_flags_say_so_before_anything_is_read() {
    let why = usage(&["--api", "127.0.0.1:8443", "--source", "staged", "--nodes"]);
    assert!(why.contains("--source staged"), "{why}");
    assert!(why.contains("--nodes"), "{why}");
}
