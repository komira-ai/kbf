//! `deploy-farm` against a fake coordinator and a fake runner token service on loopback, over
//! TLS with a CA of the test's own, and the binary for its flags and exit codes.

use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use serde_json::{Value, json};
use xtask::deploy_farm::{Args, Error, Plan, Roots, check_version, pem_roots, run_with};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

#[path = "support/deploy_fake.rs"]
mod deploy_fake;
use deploy_fake::{
    Fake, Script, Time, args_for, deploy, fake_ca, farm, run, run_timed, serve, service_args,
};

#[test]
fn the_desired_version_is_posted_then_the_farm_is_polled_to_converged() {
    // Proves: one read of the farm, one POST of the version and the minimums, then reads until `converged`;
    // exit 0; each read's table is printed; every call carries a token of its own, asked
    // for the coordinator's audience and masked in the log.
    let mut first = farm("in_progress", 1, 0);
    first["quiet"] = json!(true);
    let fake = serve(Script::new(vec![
        Ok(first),
        Ok(farm("in_progress", 0, 0)),
        Ok(farm("converged", 0, 0)),
    ]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let posts = fake.requests("/v1/desired");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].method, "POST");
    let body: Value = serde_json::from_str(&posts[0].body).expect("json");
    assert_eq!(
        body,
        json!({"version": "2.0.0+g000000000002", "min_up": {"storage": 1, "frontend": 1}})
    );
    assert_eq!(fake.requests("/v1/convergence").len(), 4);
    let tokens: Vec<String> = ["/v1/desired", "/v1/convergence"]
        .iter()
        .flat_map(|p| fake.requests(p))
        .map(|r| r.authorization)
        .collect();
    assert_eq!(tokens.len(), 5);
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 5, "a token is used once: {tokens:?}");
    let asked = fake.requests("/token");
    assert_eq!(asked.len(), 5);
    let audience = format!(
        "audience=https%3A%2F%2F127.0.0.1%3A{}",
        fake.base.rsplit(':').next().expect("port")
    );
    assert!(
        asked.iter().all(|r| r.path.ends_with(&audience)),
        "{asked:?}"
    );
    for n in 0..5 {
        assert!(out.contains(&format!("::add-mask::tok-{n}")), "{out}");
    }
    assert!(
        out.contains("farm: in_progress, desired 2.0.0+g000000000002 (coordinator just started)"),
        "{out}"
    );
    assert!(
        out.contains("farm: converged, desired 2.0.0+g000000000002"),
        "{out}"
    );
    assert!(out.contains("storage"), "{out}");
    assert!(out.contains("daemon"), "{out}");
}

#[test]
fn a_blocked_farm_fails_at_once_with_its_table_printed() {
    // Proves: `blocked` ends the wait without further reads, as an error, after the table
    // that shows the failed host was printed.
    let fake = serve(Script::new(vec![
        Ok(farm("in_progress", 1, 0)),
        Ok(farm("blocked", 0, 1)),
        Ok(farm("converged", 0, 0)),
    ]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(
        result,
        Err(Error::Blocked("2.0.0+g000000000002".to_owned()))
    );
    assert_eq!(fake.requests("/v1/convergence").len(), 3);
    assert!(
        out.contains("farm: blocked, desired 2.0.0+g000000000002"),
        "{out}"
    );
    assert!(
        out.lines()
            .any(|l| l.starts_with("storage") && l.split_whitespace().nth(3) == Some("1")),
        "{out}"
    );
}

#[test]
fn a_farm_that_keeps_moving_but_does_not_converge_fails_at_the_deadline() {
    // Proves: counts that change on every read are progress, so only the deadline ends
    // the wait; it ends it as an error, after a read at the deadline, and the last sleep
    // is cut short to land on the deadline (3.5 s of deadline, 1 s between reads).
    let reads = (0..40).map(|n| Ok(farm("in_progress", n % 3, 0))).collect();
    let fake = serve(Script::new(reads));
    let mut plan = fake.plan();
    plan.deadline = Duration::from_millis(3500);
    let time = Time::default();
    let (result, out) = run_timed(&fake, &plan, &time);
    assert_eq!(result, Err(Error::Deadline(3)), "{out}");
    let second = Duration::from_secs(1);
    assert_eq!(
        *time.slept.borrow(),
        [second, second, second, Duration::from_millis(500)]
    );
    // The read before the deploy, then the reads at 0, 1, 2, 3 and 3.5 s.
    assert_eq!(fake.requests("/v1/convergence").len(), 6);
}

#[test]
fn counts_that_do_not_change_fail_as_a_stall_before_the_deadline() {
    // Proves: the same counts for the stall time end the wait as a stall, though the
    // deadline is far off: reads at 0, 1, 2 and 3 s, and the stall is judged at the
    // fourth.
    let fake = serve(Script::new(vec![Ok(farm("in_progress", 1, 0))]));
    let mut plan = fake.plan();
    plan.stall = Duration::from_secs(3);
    let time = Time::default();
    let (result, _) = run_timed(&fake, &plan, &time);
    assert_eq!(result, Err(Error::Stalled(3)));
    assert_eq!(time.slept.borrow().len(), 3);
    assert_eq!(fake.requests("/v1/convergence").len(), 5);
}

#[test]
fn a_read_that_fails_is_tried_again_and_a_refused_one_is_not() {
    // Proves: a 503 on a read is reported and the wait goes on to converged; a 401 on a read
    // ends the wait at once as a refusal, without a second read; a 401 on the read before the
    // deploy ends it before anything is posted.
    let fake_ok = serve(Script::new(vec![Err(503), Ok(farm("converged", 0, 0))]));
    let (result, out) = run(&fake_ok, &fake_ok.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("the read failed, trying again"), "{out}");
    let early = serve(Script::after(Err(401), vec![]));
    let (result, _) = run(&early, &early.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "read the farm before the deploy",
            status: 401
        })
    );
    assert!(early.requests("/v1/desired").is_empty());
    let fake_no = serve(Script::new(vec![Err(401), Ok(farm("converged", 0, 0))]));
    let (result, _) = run(&fake_no, &fake_no.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "read the convergence",
            status: 401
        })
    );
    assert_eq!(fake_no.requests("/v1/convergence").len(), 2);
}

#[test]
fn a_refused_post_reads_nothing() {
    // Proves: when the coordinator refuses the token on the POST (403), the verb fails with
    // the refusal and makes no read but the baseline.
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post = 403;
    let fake = serve(script);
    let (result, _) = run(&fake, &fake.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "set the desired version",
            status: 403
        })
    );
    assert_eq!(fake.requests("/v1/convergence").len(), 1);
    let mut script = Script::new(vec![]);
    script.post = 400;
    let bad = serve(script);
    let (result, _) = run(&bad, &bad.plan());
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "set the desired version",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn a_post_the_coordinator_could_not_serve_is_tried_again_and_a_final_one_is_not() {
    // Proves: a 503 on the POST (the coordinator could not reach the token issuer) is
    // reported and tried again, and the deploy goes on to converged; a 503 that never ends
    // fails after a few tries, not at once and not forever.
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 2;
    let fake = serve(script);
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(fake.requests("/v1/desired").len(), 3);
    assert!(
        out.contains("the call failed, trying again: set the desired version"),
        "{out}"
    );
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 100;
    let down = serve(script);
    let (result, _) = run(&down, &down.plan());
    assert!(matches!(result, Err(Error::Call { .. })), "{result:?}");
    assert_eq!(down.requests("/v1/desired").len(), 4);
    assert_eq!(down.requests("/v1/convergence").len(), 1);
}

#[test]
fn a_convergence_on_another_version_is_not_this_deploys() {
    // Proves: when the coordinator's desired version is not the one this deploy posted
    // (another writer replaced it), a `converged` answer ends the deploy as an error naming
    // the other version, and so does an in-progress one.
    for state in ["converged", "in_progress"] {
        let mut other = farm(state, 0, 0);
        other["desired"] = json!("3.0.0+g000000000003");
        let fake = serve(Script::new(vec![Ok(other)]));
        let (result, _) = run(&fake, &fake.plan());
        assert_eq!(
            result,
            Err(Error::Superseded("3.0.0+g000000000003".to_owned()))
        );
        assert_eq!(fake.requests("/v1/convergence").len(), 2);
    }
}

#[test]
fn what_the_coordinator_prints_cannot_start_a_workflow_command() {
    // Proves: a version or role that starts with `::` or holds a line break reaches the log
    // as one plain line, so no line of the baseline or the table is a workflow command.
    let mut hostile = farm("converged", 0, 0);
    hostile["desired"] = json!("2.0.0+g000000000002");
    hostile["roles"][0]["role"] = json!("::error::boom\n::add-mask::x");
    let fake = serve(Script::after(Ok(hostile.clone()), vec![Ok(hostile)]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("error::boom::add-mask::x"), "{out}");
    assert!(
        out.contains("baseline: hosts live before the deploy: error::boom::add-mask::x 2"),
        "{out}"
    );
    let masks = out.lines().filter(|l| l.starts_with("::")).count();
    assert_eq!(masks, 3, "only the client's own masks: {out}");
    assert!(!out.lines().any(|l| l.starts_with("::error")), "{out}");
}

#[test]
fn a_redirect_from_the_coordinator_is_not_followed() {
    // Proves: a 302 on the read is a failed read, never a second request to the place it
    // names; the wait ends as a stall.
    let fake = serve(Script::new(vec![Err(302)]));
    let mut plan = fake.plan();
    plan.stall = Duration::from_secs(3);
    let (result, out) = run(&fake, &plan);
    assert_eq!(result, Err(Error::Stalled(3)), "{out}");
    assert!(fake.requests("/v1/elsewhere").is_empty());
}

#[test]
fn a_runner_with_no_token_and_an_answer_that_is_no_table_fail_with_reasons() {
    // Proves: a token service that fails ends the verb before anything is sent to the
    // coordinator; an answer without the fields of the table is an error that says so, not a
    // panic or a silent wait.
    let mut script = Script::new(vec![Ok(json!({}))]);
    script.token = 500;
    let down = serve(script);
    let (result, _) = run(&down, &down.plan());
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "ask the runner for an OIDC token",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(down.requests("/v1/desired").is_empty());
    for answer in [
        json!({}),
        json!({"state": "converged"}),
        json!({"state": "converged", "roles": [{"role": "storage"}]}),
        json!({"state": "converged", "roles": [{"at_desired": 1, "updating": 0, "failed": 0, "down": 0}]}),
    ] {
        let odd = serve(Script::new(vec![Ok(answer)]));
        let (result, _) = run(&odd, &odd.plan());
        assert!(matches!(result, Err(Error::Answer(_))), "{result:?}");
    }
    for answer in [
        json!({}),
        json!({"state": "converged", "roles": [{"role": "storage"}]}),
    ] {
        let odd = serve(Script::after(Ok(answer), vec![]));
        let (result, _) = run(&odd, &odd.plan());
        assert!(matches!(result, Err(Error::Answer(_))), "{result:?}");
        assert!(odd.requests("/v1/desired").is_empty());
    }
    let empty = serve(Script::new(vec![Ok(
        json!({"state": "in_progress", "roles": []}),
    )]));
    let mut plan = empty.plan();
    plan.stall = Duration::from_secs(3);
    let (_, out) = run(&empty, &plan);
    assert!(out.contains("desired (none)"), "{out}");
}

/// A writer that fails once more than `room` bytes have been written.
struct Room {
    room: usize,
}

impl Write for Room {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.room {
            return Err(io::Error::other("full"));
        }
        self.room -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_token_the_service_does_not_give_is_an_error_whatever_it_says() {
    // Proves: an answer with no `value`, an empty one, one that is not JSON and one that is
    // not text each end the deploy at the token, before the coordinator is called; a token
    // service address with no query gets its `audience` after a `?`.
    for raw in [&b"{}"[..], br#"{"value":""}"#, b"not json", b"\xff\xfe"] {
        let mut script = Script::new(vec![]);
        script.token_answer = Some(raw.to_vec());
        let down = serve(script);
        let mut service = down.service();
        service.url = format!("{}/token", down.base);
        let mut out = Vec::new();
        let ca = [down.ca.clone()];
        let roots = Roots {
            runner: &ca,
            coordinator: &ca,
        };
        let result = deploy(&down.plan(), &service, &roots, &mut out);
        assert!(
            matches!(
                result,
                Err(Error::Call {
                    what: "ask the runner for an OIDC token",
                    ..
                })
            ),
            "{raw:?}: {result:?}"
        );
        assert!(down.requests("/v1/desired").is_empty());
        assert!(
            down.requests("/token?audience=").len() == 1,
            "{:?}",
            down.requests("/token")
        );
    }
}

#[test]
fn a_log_that_cannot_be_written_ends_the_deploy_with_an_error() {
    // Proves: a failed write of the log (a closed pipe) is an error, not a panic or a silent
    // wait, at each line of the deploy: the masking of a token, the baseline, the notice that
    // the version is set, a table, and the note that a call is tried again.
    let mask = "::add-mask::tok-0\n".len();
    let baseline = "baseline: hosts live before the deploy: storage 2, daemon 1\n".len();
    let desired = "desired version 2.0.0+g000000000002 set\n".len();
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    for room in [
        0,
        mask,
        mask + baseline,
        mask + baseline + mask,
        mask + baseline + mask + desired,
        mask + baseline + mask + desired + mask,
    ] {
        let result = deploy(&fake.plan(), &fake.service(), &roots, &mut Room { room });
        assert!(
            matches!(result, Err(Error::Output(_))),
            "{room}: {result:?}"
        );
    }
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 1;
    let retried = serve(script);
    let ca = [retried.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let result = deploy(
        &retried.plan(),
        &retried.service(),
        &roots,
        &mut Room {
            room: mask + baseline + mask,
        },
    );
    assert!(matches!(result, Err(Error::Output(_))), "{result:?}");
}

/// A farm answer with the given counts per role: (role, at_desired, updating, failed, down).
fn counts(state: &str, desired: &str, roles: &[(&str, u64, u64, u64, u64)]) -> Value {
    let rows: Vec<Value> = roles
        .iter()
        .map(|(role, at, updating, failed, down)| {
            json!({"role": role, "at_desired": at, "updating": updating, "failed": failed, "down": down})
        })
        .collect();
    json!({"desired": desired, "quiet": false, "state": state, "roles": rows})
}

#[test]
fn a_host_that_goes_down_during_the_deploy_fails_it_though_the_farm_says_converged() {
    // Proves: three storage hosts live before the deploy, one stops heartbeating, and the
    // coordinator (which answers for the hosts it hears from) says `converged`: the deploy
    // fails naming the role, the 3 expected, the 2 reached and the 1 that went down.
    let fake = serve(Script::after(
        Ok(counts(
            "in_progress",
            "1.0.0+g000000000001",
            &[("storage", 3, 0, 0, 0)],
        )),
        vec![
            Ok(counts(
                "in_progress",
                "2.0.0+g000000000002",
                &[("storage", 1, 2, 0, 0)],
            )),
            Ok(counts(
                "converged",
                "2.0.0+g000000000002",
                &[("storage", 2, 0, 0, 1)],
            )),
        ],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(
        result,
        Err(Error::Lost {
            role: "storage".to_owned(),
            expected: 3,
            reached: 2,
            version: "2.0.0+g000000000002".to_owned()
        }),
        "{out}"
    );
    let message = result.expect_err("lost").to_string();
    assert!(message.contains("storage"), "{message}");
    assert!(message.contains("3 hosts were live"), "{message}");
    assert!(message.contains("2 run 2.0.0+g000000000002"), "{message}");
    assert!(message.contains("1 went down"), "{message}");
}

#[test]
fn a_host_that_was_down_before_the_deploy_does_not_block_it() {
    // Proves: a host already down at the baseline is not expected: two storage hosts live and
    // one down before, two at the version after, passes (and the daemon role, with its one
    // host down at both reads and none live, is not expected either).
    let fake = serve(Script::after(
        Ok(counts(
            "in_progress",
            "1.0.0+g000000000001",
            &[("storage", 2, 0, 0, 1), ("daemon", 0, 0, 0, 1)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0.0+g000000000002",
            &[("storage", 2, 0, 0, 1), ("daemon", 0, 0, 0, 1)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
}

#[test]
fn a_host_that_joins_during_the_deploy_is_fine() {
    // Proves: more hosts at the version than were live at the baseline, and a role the
    // baseline did not list at all, pass; so does a baseline with no host at all (printed as
    // "none").
    let fake = serve(Script::after(
        Ok(counts(
            "in_progress",
            "1.0.0+g000000000001",
            &[("storage", 1, 0, 0, 0)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0.0+g000000000002",
            &[("storage", 3, 0, 0, 0), ("frontend", 1, 0, 0, 0)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let fake = serve(Script::after(
        Ok(counts("in_progress", "1.0.0+g000000000001", &[])),
        vec![Ok(counts(
            "converged",
            "2.0.0+g000000000002",
            &[("storage", 1, 0, 0, 0)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("before the deploy: none"), "{out}");
}

#[test]
fn a_role_that_vanishes_from_the_answer_counts_as_all_its_hosts_lost() {
    // Proves: a baseline role the final answer does not list has no host at the version.
    let fake = serve(Script::after(
        Ok(counts(
            "in_progress",
            "1.0.0+g000000000001",
            &[("storage", 2, 0, 0, 0)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0.0+g000000000002",
            &[("frontend", 1, 0, 0, 0)],
        ))],
    ));
    let (result, _) = run(&fake, &fake.plan());
    assert!(
        matches!(
            result,
            Err(Error::Lost {
                expected: 2,
                reached: 0,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn the_hosts_live_before_the_deploy_are_printed_before_the_post() {
    // Proves: updating and failed hosts count as live and down ones do not, per role, in a
    // line printed before the desired version is set.
    let fake = serve(Script::after(
        Ok(counts(
            "blocked",
            "1.0.0+g000000000001",
            &[("storage", 2, 1, 1, 5), ("frontend", 1, 0, 0, 0)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0.0+g000000000002",
            &[("storage", 4, 0, 0, 5), ("frontend", 1, 0, 0, 0)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let line = "baseline: hosts live before the deploy: storage 4, frontend 1";
    let at = out.find(line).unwrap_or_else(|| panic!("{out}"));
    let posted = out
        .find("desired version 2.0.0+g000000000002 set")
        .expect("posted");
    assert!(at < posted, "{out}");
}

#[test]
fn a_503_on_the_read_before_the_deploy_is_tried_again() {
    // Proves: the baseline read is retried on a 503 like the POST, and the deploy goes on.
    let fake = serve(Script::after(Err(503), vec![Ok(farm("converged", 0, 0))]));
    // The first read is a 503, the second answers; the baseline is the second.
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.contains("the call failed, trying again: read the farm before the deploy"),
        "{out}"
    );
}

#[test]
fn the_verb_trusts_the_system_roots_or_the_ca_it_is_given() {
    // Proves: with no `--ca-pem` the coordinator is trusted by the system roots given; with one
    // it is trusted by that CA instead (a CA that did not sign its certificate refuses it
    // though the system roots would have trusted it), and the verb's line says what converged.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let system = [fake.ca.clone()];
    let mut out = Vec::new();
    let lines = run_with(&args_for(&fake, ""), &fake.service(), &system, &mut out).expect("run");
    assert_eq!(
        lines,
        [format!(
            "ok: every live host runs 2.0.0+g000000000002 ({})",
            fake.base
        )]
    );
    let lines = run_with(
        &args_for(&fake, &fake.ca_pem),
        &fake.service(),
        &system,
        &mut out,
    )
    .expect("pinned to its own CA");
    assert_eq!(lines.len(), 1);
    let (_, other_pem) = fake_ca();
    let refused = run_with(
        &args_for(&fake, &other_pem),
        &fake.service(),
        &system,
        &mut out,
    );
    let Err(xtask::Error::Failed(why)) = refused else {
        panic!("a certificate of another CA was trusted: {refused:?}");
    };
    assert!(why.contains("read the farm before the deploy"), "{why}");
    let posts = fake.requests("/v1/desired").len();
    assert_eq!(posts, 2, "the pinned-to-another-CA run sent nothing");
}

#[test]
fn only_an_https_address_without_a_path_is_used() {
    // Proves: a plain-http address, a path and an empty host are refused before any
    // token is asked for.
    let fake = serve(Script::new(vec![]));
    for url in [
        "http://127.0.0.1:1",
        "https://127.0.0.1:1/v1",
        "127.0.0.1:1",
    ] {
        let mut plan = fake.plan();
        plan.url = url.to_owned();
        let (result, out) = run(&fake, &plan);
        assert_eq!(result, Err(Error::Url(url.to_owned())));
        assert!(out.is_empty());
    }
    assert!(fake.requests("/token").is_empty());
}

#[test]
fn a_certificate_the_roots_do_not_hold_is_not_trusted() {
    // Proves: a coordinator whose certificate does not chain to the roots given for it is
    // refused at the handshake and nothing is sent to it, though the runner's token service
    // (its own roots) is reached and a token is asked for; and the other way, a token
    // service the roots given for it do not hold ends the deploy before any call to the
    // coordinator.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let (other, _) = fake_ca();
    let ca = [fake.ca.clone()];
    let mut out = Vec::new();
    let roots = Roots {
        runner: &ca,
        coordinator: &other,
    };
    let result = deploy(&fake.plan(), &fake.service(), &roots, &mut out);
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "read the farm before the deploy",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(fake.requests("/v1/desired").is_empty());
    assert_eq!(fake.requests("/token").len(), 1);
    let roots = Roots {
        runner: &other,
        coordinator: &ca,
    };
    let result = deploy(&fake.plan(), &fake.service(), &roots, &mut out);
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "ask the runner for an OIDC token",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(fake.requests("/v1/desired").is_empty());
}

#[test]
fn a_ca_in_pem_text_is_read_and_text_without_one_is_refused() {
    // Proves: the PEM of `--ca-pem` gives its certificates (one or several), and text that
    // holds none, or is not PEM, is an error rather than an empty set that trusts nothing.
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let one = ca.pem();
    assert_eq!(pem_roots(&one).expect("one"), [ca.der().clone()]);
    assert_eq!(pem_roots(&format!("{one}\n{one}")).expect("two").len(), 2);
    for text in [
        "",
        "not pem",
        "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
    ] {
        assert_eq!(pem_roots(text), Err(Error::Ca), "{text:?}");
    }
}

#[test]
fn the_verb_with_an_empty_url_prints_a_notice_and_exits_0() {
    // Proves: a repository whose variable is unset (an empty `--url`, or blanks) deploys
    // nothing and passes, with a notice the log shows.
    for url in ["", "  "] {
        let output = Command::new(XTASK)
            .args([
                "deploy-farm",
                "--url",
                url,
                "--version",
                "2.0.0+g000000000002",
            ])
            .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
            .output()
            .expect("run");
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.starts_with("::notice::the deploy step is off"),
            "{text}"
        );
    }
}

#[test]
fn the_verb_with_an_address_and_no_token_service_fails_and_names_the_permission() {
    // Proves: a job without `id-token: write` fails with a message that names it, and
    // sends nothing; flags out of range are usage errors (exit 2).
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "https://127.0.0.1:9",
            "--version",
            "2.0.0+g000000000002",
        ])
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("id-token: write"));
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "u",
            "--version",
            "2.0.0+g000000000002",
            "--deadline-min",
            "0",
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn the_verb_with_a_ca_that_is_no_pem_fails_before_any_call() {
    // Proves: a `--ca-pem` that holds no certificate fails the verb (exit 1) and names the
    // flag, instead of trusting nothing and failing at a handshake.
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "https://127.0.0.1:9",
            "--version",
            "2.0.0+g000000000002",
            "--ca-pem",
            "junk",
        ])
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "https://127.0.0.1:9/token")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "k")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--ca-pem holds no certificate"));
}

#[test]
fn the_plan_of_the_flags_is_in_seconds_and_defaults_the_audience_to_the_address() {
    // Proves: the defaults (60 minutes, 20 minutes, 30 seconds, the address as audience) and
    // that a trailing slash of the address is not part of a request path.
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: xtask::deploy_farm::Args,
    }
    let cli = Cli::parse_from([
        "x",
        "--url",
        "https://c.example:8443/",
        "--version",
        "2.0.0+g000000000002",
    ]);
    let plan = Plan::of(&cli.args);
    assert_eq!(plan.url, "https://c.example:8443");
    assert_eq!(plan.audience, "https://c.example:8443/");
    assert_eq!(plan.deadline, Duration::from_secs(3600));
    assert_eq!(plan.stall, Duration::from_secs(1200));
    assert_eq!(plan.every, Duration::from_secs(30));
    assert_eq!((plan.min_up_storage, plan.min_up_frontend), (1, 1));
    let cli = Cli::parse_from([
        "x",
        "--url",
        "u",
        "--version",
        "2.0.0+g000000000002",
        "--audience",
        "aud",
    ]);
    assert_eq!(Plan::of(&cli.args).audience, "aud");
}

#[test]
fn only_the_form_the_hosts_report_is_a_version_to_deploy() {
    // Proves: `<semver>+g<12 lower-case hex>` is accepted, and the release tag (what the
    // workflow once passed, which no host reports and no installer can find a commit in), a
    // short or long or upper-case commit, a missing `g`, a revision suffix and a text after
    // the commit are each refused with the error that names the form.
    for good in [
        "0.1.0+g0123456789ab",
        "10.20.30+gffffffffffff",
        "1.0.0-rc.1+g000000000000",
        "0.0.0-a-b.1+g0123456789ab",
    ] {
        assert_eq!(check_version(good), Ok(()), "{good}");
    }
    for bad in [
        "0.0.13",
        "2.0",
        "",
        "0.1.0+",
        "0.1.0+g",
        "0.1.0+g0123456789a",
        "0.1.0+g0123456789abc",
        "0.1.0+g0123456789AB",
        "0.1.0+0123456789ab",
        "0.1.0+g0123456789ab-1",
        "0.1.0+g0123456789abx",
        "0.1+g0123456789ab",
        "01.1.0+g0123456789ab",
        "+g0123456789ab",
        "v0.1.0+g0123456789ab",
        "0.1.0-+g0123456789ab",
        "0.1.x+g0123456789ab",
        "0.01.0+g0123456789ab",
        "1..0+g0123456789ab",
        "1.0.0-a..b+g0123456789ab",
        "1.0.0-a_b+g0123456789ab",
    ] {
        let error = check_version(bad).expect_err(bad);
        assert_eq!(error, Error::Version(bad.to_owned()));
        let said = error.to_string();
        assert!(
            said.contains("<semver>+g<12 lower-case hex digits of the commit>"),
            "{said}"
        );
    }
}

#[test]
fn the_verb_refuses_a_tag_for_a_version_before_any_call() {
    // Proves: `deploy-farm --version 0.0.13` (the tag form) exits 1 and names the form it
    // needs, whether or not the coordinator's address is set, and sends nothing (the address
    // is one nothing listens on, and there is no token service).
    for url in ["https://127.0.0.1:9", ""] {
        let output = Command::new(XTASK)
            .args(["deploy-farm", "--url", url, "--version", "0.0.13"])
            .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
            .output()
            .expect("run");
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains("<semver>+g<12 lower-case hex digits of the commit>"),
            "{said}"
        );
    }
}

#[test]
fn run_with_refuses_a_tag_version_without_calling_the_coordinator() {
    // Proves: the library entry refuses too, before the first request.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let mut args = args_for(&fake, "");
    args.version = "0.0.13".to_owned();
    let mut out = Vec::new();
    let result = run_with(
        &args,
        &fake.service(),
        std::slice::from_ref(&fake.ca),
        &mut out,
    );
    assert!(result.is_err());
    assert!(fake.requests("/").is_empty(), "a request was sent");
}

#[test]
fn another_service_is_posted_by_name_and_waited_on_by_its_own_convergence() {
    // Proves: with `--service js-server` the POST body is the version and the service name
    // (the `min_up` keys are kbf's; the service's definition says its own), the baseline
    // read and every poll ask `/v1/convergence?service=js-server` (never the bare path), a
    // version text of the service's own form is accepted, and the run ends when that
    // service's convergence says converged.
    let seven = |state, updating| {
        let mut answer = farm(state, updating, 0);
        answer["desired"] = json!("7");
        Ok(answer)
    };
    let fake = serve(Script::after(
        seven("converged", 0),
        vec![seven("in_progress", 1), seven("converged", 0)],
    ));
    let args = service_args(&fake, "js-server", "7");
    let mut out = Vec::new();
    let ca = [fake.ca.clone()];
    let lines = run_with(&args, &fake.service(), &ca, &mut out).expect("converged");
    assert_eq!(lines.len(), 1);
    let posts = fake.requests("/v1/desired");
    assert_eq!(posts.len(), 1);
    let body: Value = serde_json::from_str(&posts[0].body).expect("json");
    assert_eq!(
        body,
        json!({
            "service": "js-server",
            "version": "7"
        })
    );
    let reads = fake.requests("/v1/convergence");
    assert_eq!(reads.len(), 3);
    assert!(
        reads
            .iter()
            .all(|r| r.path == "/v1/convergence?service=js-server"),
        "{reads:?}"
    );
}

#[test]
fn kbf_sends_the_request_it_always_sent() {
    // Proves: `--service kbf`, given or defaulted, posts exactly the bytes deploy-farm
    // posted before the flag existed (no `service` key) and reads the bare convergence path.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let mut args = args_for(&fake, &fake.ca_pem);
    assert_eq!(args.service, "kbf");
    args.service = "kbf".to_owned();
    let mut out = Vec::new();
    let ca = [fake.ca.clone()];
    run_with(&args, &fake.service(), &ca, &mut out).expect("converged");
    assert_eq!(
        fake.requests("/v1/desired")[0].body,
        r#"{"min_up":{"frontend":1,"storage":1},"version":"2.0.0+g000000000002"}"#
    );
    assert!(
        fake.requests("/v1/convergence")
            .iter()
            .all(|r| r.path == "/v1/convergence")
    );
}

#[test]
fn a_service_name_the_coordinator_would_refuse_is_refused_before_any_call() {
    // Proves: an empty name, upper case, a slash, a query character, a space and a name over
    // the coordinator's 64 bytes are refused by the client, so the name can go in the query
    // unescaped, and nothing is sent; and only kbf's version has to be of kbf's form.
    let fake = serve(Script::new(vec![]));
    let ca = [fake.ca.clone()];
    let long = "a".repeat(65);
    for name in ["", "JS", "a/b", "a&b", "a b", "a_b", long.as_str()] {
        let args = service_args(&fake, name, "7");
        let refused = run_with(&args, &fake.service(), &ca, &mut Vec::new());
        let Err(xtask::Error::Failed(why)) = refused else {
            panic!("{name:?} was accepted: {refused:?}");
        };
        assert!(why.contains("is not a service name"), "{why}");
    }
    let kbf = service_args(&fake, "kbf", "7");
    let refused = run_with(&kbf, &fake.service(), &ca, &mut Vec::new());
    assert!(matches!(refused, Err(xtask::Error::Failed(why)) if why.contains("is not the form")));
    assert!(fake.requests("/token").is_empty());
    assert!(fake.requests("/v1").is_empty());
}

/// The baseline read of a farm whose coordinator says it was built from `commit`.
fn reporting(commit: Option<&str>) -> Result<Value, u16> {
    let mut read = farm("converged", 0, 0);
    if let Some(commit) = commit {
        read["commit"] = json!(commit);
    }
    Ok(read)
}

/// The deploy with `require_coordinator_current` as given, against a coordinator whose
/// baseline read reports `commit`; the release is `...0002`.
fn with_coordinator(commit: Option<&str>, require: bool) -> (Fake, Result<(), Error>, String) {
    let fake = serve(Script::after(
        reporting(commit),
        vec![Ok(farm("converged", 0, 0))],
    ));
    let mut plan = fake.plan();
    plan.require_coordinator_current = require;
    let (result, out) = run(&fake, &plan);
    (fake, result, out)
}

#[test]
fn a_coordinator_on_another_commit_stops_a_deploy_that_requires_it_current_before_the_post() {
    // Proves: with the requirement on, a coordinator that reports another commit than the
    // release's fails with both commits and the pointer to the docs, and the desired version
    // is never posted.
    let (fake, result, out) = with_coordinator(Some("0123456789ab"), true);
    let error = result.expect_err(&out);
    assert_eq!(
        error,
        Error::CoordinatorBehind {
            coordinator: "0123456789ab".to_owned(),
            release: "000000000002".to_owned(),
        }
    );
    assert_eq!(
        error.to_string(),
        "the coordinator runs 0123456789ab, the release is 000000000002: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)"
    );
    assert!(fake.requests("/v1/desired").is_empty(), "{out}");
}

#[test]
fn a_coordinator_on_the_release_commit_proceeds_when_it_is_required_current() {
    // Proves: the same commit passes the requirement, is said in the log, and the deploy
    // runs to convergence.
    let (fake, result, out) = with_coordinator(Some("000000000002"), true);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.contains("the coordinator runs the release's commit 000000000002"),
        "{out}"
    );
    assert_eq!(fake.requests("/v1/desired").len(), 1);
}

#[test]
fn a_coordinator_that_reports_no_commit_gets_a_notice_and_the_deploy_proceeds() {
    // Proves: an older coordinator (no `commit` key) and one built without a checkout
    // (`unknown`) are noted when the requirement is on, and neither stops the deploy.
    for reported in [None, Some("unknown")] {
        let (fake, result, out) = with_coordinator(reported, true);
        assert_eq!(result, Ok(()), "{reported:?}: {out}");
        assert!(
            out.contains("::notice::the coordinator reports no build commit"),
            "{reported:?}: {out}"
        );
        assert_eq!(fake.requests("/v1/desired").len(), 1);
    }
}

#[test]
fn without_the_requirement_a_coordinator_on_another_commit_warns_and_the_deploy_proceeds() {
    // Proves: with the requirement off a different commit is a GitHub warning annotation
    // line and the deploy proceeds, and a coordinator that reports no commit adds no notice.
    let (fake, result, out) = with_coordinator(Some("0123456789ab"), false);
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.lines().any(|l| l
            == "::warning title=coordinator not current::the coordinator runs 0123456789ab, the release is 000000000002: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)"),
        "{out}"
    );
    assert_eq!(fake.requests("/v1/desired").len(), 1);
    let (_, result, out) = with_coordinator(None, false);
    assert_eq!(result, Ok(()), "{out}");
    assert!(!out.contains("build commit"), "{out}");
}

#[test]
fn the_flag_turns_the_requirement_on_and_is_off_by_default() {
    // Proves: `--require-coordinator-current` reaches the plan, and without it the plan does
    // not require it.
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }
    let fake = serve(Script::new(vec![]));
    let off = args_for(&fake, "");
    assert!(!Plan::of(&off).require_coordinator_current);
    let on = Cli::parse_from([
        "x",
        "--url",
        &fake.base,
        "--version",
        "2.0.0+g000000000002",
        "--require-coordinator-current",
    ]);
    assert!(Plan::of(&on.args).require_coordinator_current);
    // The workflow passes the variable's text: `true` is on; empty (unset) and `false` are off.
    for (text, want) in [("true", true), ("false", false), ("", false)] {
        let cli = Cli::parse_from([
            "x",
            "--url",
            &fake.base,
            "--version",
            "2.0.0+g000000000002",
            "--require-coordinator-current",
            text,
        ]);
        assert_eq!(
            Plan::of(&cli.args).require_coordinator_current,
            want,
            "{text:?}"
        );
    }
    let bad = Cli::try_parse_from([
        "x",
        "--url",
        &fake.base,
        "--version",
        "2.0.0+g000000000002",
        "--require-coordinator-current",
        "yes",
    ]);
    assert!(bad.is_err());
}
