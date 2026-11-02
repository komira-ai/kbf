//! `cargo xtask deploy-farm restore` against a fake coordinator that keeps a desired state as
//! the real one does (`support/coordinator_fake.rs`): the deploy of a release, then the
//! restore that the job runs when an earlier step failed. Every fake is driven by handshakes
//! (the number of a request), and the clock is virtual.

#[path = "support/coordinator_fake.rs"]
mod coordinator_fake;
#[path = "support/deploy_fake.rs"]
mod deploy_fake;

use std::time::Duration;

use coordinator_fake::{BEFORE, Competing, Fake, Fate, RELEASE, Script, State, serve};
use deploy_fake::{RUNNER_KEY, Time};
use serde_json::{Value, json};
use xtask::deploy_farm::restore::{self, Error, FORCED_ROLLBACK, restore_on};
use xtask::deploy_farm::{
    Command, Error as Deploy, Plan, Roots, Then, TokenService, deploy_on, run_command,
};

const RUN: &str = "4711-2";
const WRITE_ID: &str = "restore-4711-2";

fn token_service(base: &str) -> TokenService {
    TokenService {
        url: format!("{base}/token?api-version=2.0"),
        key: RUNNER_KEY.to_owned(),
    }
}

fn plan(fake: &Fake) -> Plan {
    Plan {
        url: fake.base.clone(),
        service: "kbf".to_owned(),
        audience: fake.base.clone(),
        version: RELEASE.to_owned(),
        min_up_storage: 1,
        min_up_frontend: 1,
        require_coordinator_current: false,
        deadline: Duration::from_secs(30),
        stall: Duration::from_secs(30),
        every: Duration::from_secs(1),
    }
}

/// The deploy step of the job, by `plan`.
fn deploy_with(fake: &Fake, plan: &Plan) -> Result<(), Deploy> {
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let mut out = Vec::new();
    deploy_on(
        plan,
        &token_service(&fake.base),
        &roots,
        &Time::default().clock(),
        &mut out,
    )
}

fn deploy(fake: &Fake) -> Result<(), Deploy> {
    deploy_with(fake, &plan(fake))
}

fn args(base: &str) -> restore::Args {
    restore::Args {
        url: base.to_owned(),
        version: RELEASE.to_owned(),
        service: "kbf".to_owned(),
        ca_pem: String::new(),
        audience: None,
        run: RUN.to_owned(),
        every_secs: 1,
    }
}

/// The restore step of the job: its result, its log, and what it slept.
fn restore_timed(fake: &Fake, args: &restore::Args) -> (Result<String, Error>, String, Time) {
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let time = Time::default();
    let mut out = Vec::new();
    let result = restore_on(
        args,
        &token_service(&fake.base),
        &roots,
        &time.clock(),
        &mut out,
    );
    (result, String::from_utf8(out).expect("utf8"), time)
}

fn restore(fake: &Fake) -> (Result<String, Error>, String) {
    let (result, out, _) = restore_timed(fake, &args(&fake.base));
    (result, out)
}

fn restored() -> Result<String, Error> {
    Err(Error::Restored {
        version: BEFORE.to_owned(),
        release: RELEASE.to_owned(),
    })
}

/// The body of the restore's POST of `BEFORE` and its minimums, on `generation`.
fn restore_body(generation: u64) -> Value {
    json!({
        "version": BEFORE,
        "min_up": {"storage": 2, "frontend": 1},
        "if_generation": generation,
        "write_id": WRITE_ID,
    })
}

/// The farm set back to `BEFORE` by the restore, on top of `generation`.
fn set_back(generation: u64) -> State {
    State {
        version: BEFORE.to_owned(),
        min_up: json!({"storage": 2, "frontend": 1}),
        generation: generation + 1,
        replaced: Some((RELEASE.to_owned(), json!({"storage": 1, "frontend": 1}))),
        write_id: WRITE_ID.to_owned(),
    }
}

fn versions(fake: &Fake) -> Vec<String> {
    fake.posts()
        .iter()
        .map(|p| p.body["version"].as_str().expect("version").to_owned())
        .collect()
}

/// Catches: a red proof (the farm converged on the release, then `prove-farm` failed) that
/// leaves the release desired. The restore makes exactly one POST after the release's own:
/// the version the release replaced, with its minimums, on the generation it read, as its own
/// write; it prints what it set and exits 1.
#[test]
fn a_failed_proof_after_a_green_convergence_sets_replaced_back_once() {
    let fake = serve(Script::new());
    assert_eq!(deploy(&fake), Ok(()));
    let (result, out) = restore(&fake);
    assert_eq!(result, restored());
    let posts = fake.posts();
    assert_eq!(versions(&fake), [RELEASE, BEFORE]);
    assert_eq!(posts[1].body, restore_body(2));
    assert_eq!(fake.state(), Some(set_back(2)));
    assert!(
        out.contains(&format!(
            "set back: desired version {BEFORE}, min_up {{\"frontend\":1,\"storage\":2}}, on generation 2, as {WRITE_ID}"
        )),
        "{out}"
    );
}

/// A farm's reads, the deploy's deadline and stall in seconds, and the error it must end with.
type Ending = (Vec<(&'static str, u64)>, u64, u64, fn(&Deploy) -> bool);

/// Catches: a deploy that ends red for any of its own reasons left with the release desired:
/// a `blocked` farm, a stall, the deadline and a host lost each end the deploy with its error,
/// and the restore then sets the version before back, once.
#[test]
fn a_blocked_a_stalled_an_overdue_and_a_lost_deploy_each_set_replaced_back() {
    let cases: [Ending; 4] = [
        (vec![("converged", 2), ("blocked", 2)], 30, 30, |e| {
            matches!(e, Deploy::Blocked(_))
        }),
        (vec![("converged", 2), ("in_progress", 2)], 30, 3, |e| {
            matches!(e, Deploy::Stalled(3))
        }),
        (
            vec![("converged", 2), ("in_progress", 1), ("in_progress", 2)],
            3,
            30,
            |e| matches!(e, Deploy::Deadline(3)),
        ),
        (vec![("converged", 2), ("converged", 1)], 30, 30, |e| {
            matches!(e, Deploy::Lost { reached: 1, .. })
        }),
    ];
    for (farm, deadline, stall, ended) in cases {
        let fake = serve(Script {
            farm: farm.clone(),
            ..Script::new()
        });
        let mut plan = plan(&fake);
        plan.deadline = Duration::from_secs(deadline);
        plan.stall = Duration::from_secs(stall);
        let error = deploy_with(&fake, &plan).expect_err("the deploy fails");
        assert!(ended(&error), "{farm:?}: {error}");
        assert_eq!(restore(&fake).0, restored(), "{farm:?}");
        assert_eq!(versions(&fake), [RELEASE, BEFORE], "{farm:?}");
        assert_eq!(fake.posts()[1].body, restore_body(2), "{farm:?}");
        assert_eq!(fake.state(), Some(set_back(2)), "{farm:?}");
    }
}

/// Catches: a baseline kept by the job instead of the coordinator. The forward write is
/// stored but its answer never arrives, so the deploy fails without knowing it set anything;
/// the restore still finds the release desired and sets back what it replaced.
#[test]
fn a_forward_write_stored_with_its_answer_dropped_is_still_set_back() {
    let fake = serve(Script {
        fates: vec![Fate::Drop],
        ..Script::new()
    });
    let error = deploy(&fake).expect_err("no answer");
    assert!(matches!(error, Deploy::Call { .. }), "{error}");
    assert_eq!(fake.state().map(|s| s.version), Some(RELEASE.to_owned()));
    assert_eq!(restore(&fake).0, restored());
    assert_eq!(versions(&fake), [RELEASE, BEFORE]);
    assert_eq!(fake.state(), Some(set_back(2)));
}

/// Catches: a re-run taking the release it sets again for the baseline. With the release
/// already desired (an earlier attempt set it), the re-run's forward write sets it again and
/// fails; the restore sets back the version before the release, never the release.
#[test]
fn a_rerun_with_this_release_already_desired_sets_back_replaced_not_this_release() {
    let fake = serve(Script {
        state: Some(State::released()),
        farm: vec![("converged", 2), ("blocked", 2)],
        ..Script::new()
    });
    assert!(matches!(deploy(&fake), Err(Deploy::Blocked(_))));
    assert_eq!(restore(&fake).0, restored());
    assert_eq!(versions(&fake), [RELEASE, BEFORE]);
    assert_eq!(fake.posts()[1].body, restore_body(3));
    assert_eq!(fake.state(), Some(set_back(3)));
}

/// Catches: a restore that overwrites a write accepted between its read and its POST. The
/// competing write (a newer release, or this same release set again by a re-run's forward
/// write) gets the restore a 409; the competing write stands, and the restore exits 1 naming
/// the state now and its write.
#[test]
fn a_competing_write_at_the_restores_read_stands_and_is_named() {
    let newer = "3.0.0+g000000000003";
    for (version, write_id, named) in [
        (newer, "release-99", "the write release-99"),
        (RELEASE, "", "a write without an id"),
    ] {
        let competing = Competing {
            at_read: 0,
            version: version.to_owned(),
            min_up: json!({"storage": 1, "frontend": 1}),
            write_id: write_id.to_owned(),
        };
        let fake = serve(Script {
            state: Some(State::released()),
            competing: vec![competing],
            ..Script::new()
        });
        let (result, _) = restore(&fake);
        let error = result.expect_err("refused");
        assert_eq!(
            error,
            Error::OtherWriter {
                desired: version.to_owned(),
                write: named.to_owned(),
            }
        );
        assert!(error.to_string().contains(version), "{error}");
        let posts = fake.posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].status, 409);
        assert_eq!(posts[0].body, restore_body(2));
        let now = fake.state().expect("a state");
        assert_eq!(
            (now.version.as_str(), now.write_id.as_str()),
            (version, write_id)
        );
        assert_eq!(now.generation, 3);
    }
}

/// Catches: a restore that landed taken for a lost one, or the reverse. The first POST is
/// stored and answered 503; the retry, on the same generation, gets a 409; the read after it
/// shows the restore's own `write_id`, so the restore is reported done.
#[test]
fn a_restore_that_lands_behind_a_503_reads_its_own_write_id_and_is_done() {
    let fake = serve(Script {
        state: Some(State::released()),
        fates: vec![Fate::Lands503],
        ..Script::new()
    });
    let (result, out) = restore(&fake);
    assert_eq!(result, restored());
    let posts = fake.posts();
    assert_eq!(
        posts.iter().map(|p| p.status).collect::<Vec<_>>(),
        [503, 409]
    );
    assert!(posts.iter().all(|p| p.body == restore_body(2)));
    assert_eq!(fake.state(), Some(set_back(2)));
    assert!(out.contains("whose answer was lost before"), "{out}");
}

/// Catches: the version alone taken as proof of the restore. Another writer sets the version
/// the restore would set, between its read and its POST: once with other minimums and once
/// with the very same ones. Both times the restore gets a 409, reads another `write_id`, and
/// reports another writer's result, never a restore done.
#[test]
fn another_writer_setting_the_replaced_version_is_never_taken_for_the_restore() {
    for (min_up, write_id, named) in [
        (
            json!({"storage": 3, "frontend": 1}),
            "operator-1",
            "the write operator-1",
        ),
        (
            json!({"storage": 2, "frontend": 1}),
            "",
            "a write without an id",
        ),
    ] {
        let competing = Competing {
            at_read: 0,
            version: BEFORE.to_owned(),
            min_up: min_up.clone(),
            write_id: write_id.to_owned(),
        };
        let fake = serve(Script {
            state: Some(State::released()),
            competing: vec![competing],
            ..Script::new()
        });
        let (result, out) = restore(&fake);
        assert_eq!(
            result,
            Err(Error::OtherWriter {
                desired: BEFORE.to_owned(),
                write: named.to_owned(),
            })
        );
        assert!(!out.contains("set back"), "{out}");
        let now = fake.state().expect("a state");
        assert_eq!((now.min_up, now.write_id.as_str()), (min_up, write_id));
    }
}

/// Catches: a token used twice. The fake refuses a token it has seen with 401, as the
/// coordinator refuses a `jti` it has seen; every call of the deploy and of the restore
/// presents a token no earlier call used, the restore lands, and a replay of the first is
/// refused.
#[test]
fn every_call_presents_a_fresh_token_and_a_replayed_one_is_refused() {
    let fake = serve(Script::new());
    assert_eq!(deploy(&fake), Ok(()));
    assert_eq!(restore(&fake).0, restored());
    let tokens: Vec<String> = fake.seen().into_iter().map(|r| r.authorization).collect();
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), tokens.len(), "{tokens:?}");
    assert!(tokens.len() >= 4, "{tokens:?}");
    assert_eq!(fake.state(), Some(set_back(2)));

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .tls_config(xtask::fetch::tls_config(std::slice::from_ref(&fake.ca)))
        .build()
        .into();
    let replay = agent
        .get(format!("{}/v1/convergence", fake.base))
        .header("Authorization", &tokens[0])
        .call()
        .expect("an answer");
    assert_eq!(replay.status().as_u16(), 401);
    assert!(fake.refuses(tokens[0].trim_start_matches("Bearer ")));
}

/// Catches: a restore that sets something it should not. With another version desired (the
/// release was superseded) it sets nothing and exits 0 saying why; with nothing replaced (a
/// first deploy) it sets nothing and exits 1 naming the forced rollback.
#[test]
fn a_superseded_release_and_an_empty_replaced_make_no_post() {
    let newer = "3.0.0+g000000000003";
    let fake = serve(Script {
        state: Some(State {
            version: newer.to_owned(),
            ..State::released()
        }),
        ..Script::new()
    });
    let (result, _) = restore(&fake);
    let line = result.expect("nothing to set back");
    assert!(
        line.starts_with(&format!(
            "nothing to set back: the desired version is {newer}, not {RELEASE}"
        )),
        "{line}"
    );
    assert!(fake.posts().is_empty());

    let first = serve(Script {
        state: None,
        ..Script::new()
    });
    assert_eq!(deploy(&first), Ok(()));
    let (result, _) = restore(&first);
    let error = result.expect_err("nothing replaced");
    assert_eq!(error, Error::NothingReplaced(RELEASE.to_owned()));
    assert!(error.to_string().contains(FORCED_ROLLBACK), "{error}");
    assert_eq!(versions(&first), [RELEASE]);
}

/// Catches: a restore against a coordinator older than the generation, which would post
/// without a condition and could overwrite another writer. It makes no POST and exits 1
/// naming the forced rollback.
#[test]
fn a_coordinator_without_generation_makes_no_post_and_names_the_forced_rollback() {
    let fake = serve(Script {
        state: Some(State::released()),
        generation_shown: false,
        ..Script::new()
    });
    let (result, _) = restore(&fake);
    let error = result.expect_err("no generation");
    assert_eq!(error, Error::NoGeneration);
    assert_eq!(
        error.to_string(),
        format!(
            "the coordinator shows no generation (it is older than the restore): nothing was set back; to go back, use {FORCED_ROLLBACK}"
        )
    );
    assert!(fake.posts().is_empty());
}

/// Catches: a restore that could not be made reported as anything but a failure that names
/// what to do. A 503 on every try (four tries, the pause between them on the virtual clock)
/// and a 403 each exit 1 naming the version it could not set and the forced rollback, and the
/// release stays desired.
#[test]
fn a_503_on_every_try_and_a_403_each_exit_1_naming_replaced_and_the_forced_rollback() {
    for (fates, tries, why) in [
        (vec![Fate::Status(503); 4], 4, "status 503".to_owned()),
        (
            vec![Fate::Status(403)],
            1,
            "refused with status 403".to_owned(),
        ),
    ] {
        let fake = serve(Script {
            state: Some(State::released()),
            fates,
            ..Script::new()
        });
        let (result, _, time) = restore_timed(&fake, &args(&fake.base));
        let error = result.expect_err("not set");
        assert!(
            matches!(&error, Error::Failed { version, why: w } if version == BEFORE && w.contains(&why)),
            "{error}"
        );
        let text = error.to_string();
        assert!(
            text.starts_with(&format!("could not set the farm back to {BEFORE}: ")),
            "{text}"
        );
        assert!(text.ends_with(FORCED_ROLLBACK), "{text}");
        assert_eq!(fake.posts().len(), tries);
        assert_eq!(time.slept.borrow().len(), tries - 1);
        assert_eq!(fake.state(), Some(State::released()));
    }
}

/// Catches: the restore of a service other than `kbf` posting without its name (it would set
/// kbf's desired version) or reading kbf's convergence.
#[test]
fn the_restore_of_another_service_names_it() {
    let fake = serve(Script {
        state: Some(State {
            version: "7.2".to_owned(),
            replaced: Some(("7.1".to_owned(), json!({}))),
            ..State::released()
        }),
        ..Script::new()
    });
    let mut other = args(&fake.base);
    other.service = "tierstore".to_owned();
    other.version = "7.2".to_owned();
    let (result, _, _) = restore_timed(&fake, &other);
    assert_eq!(
        result,
        Err(Error::Restored {
            version: "7.1".to_owned(),
            release: "7.2".to_owned(),
        })
    );
    assert_eq!(
        fake.posts()[0].body,
        json!({"version": "7.1", "min_up": {}, "if_generation": 2, "write_id": WRITE_ID, "service": "tierstore"})
    );
    assert!(
        fake.seen()
            .iter()
            .filter(|r| r.method == "GET")
            .all(|r| r.path == "/v1/convergence?service=tierstore")
    );
}

/// Catches: a restore that cannot tell what it read, or a refused POST whose read after it
/// fails, taken for a restore done or for another writer: both exit 1 with the reason.
#[test]
fn an_unreadable_answer_and_a_failed_read_after_a_409_are_failures() {
    let reads = |replaced: Value| {
        let mut read = deploy_fake::farm("converged", 0, 0);
        read["generation"] = json!(2);
        read["replaced"] = replaced;
        read["write_id"] = json!("");
        read
    };
    let unreadable = deploy_fake::serve(deploy_fake::Script::after(
        Ok(reads(json!({"min_up": {}}))),
        vec![],
    ));
    let (result, _) = restore_scripted(&unreadable);
    assert_eq!(
        result,
        Err(Error::Deploy(Deploy::Answer(
            "no version in replaced".to_owned()
        )))
    );
    assert!(unreadable.requests("/v1/desired").is_empty());

    let mut script = deploy_fake::Script::after(
        Ok(reads(json!({"version": BEFORE, "min_up": {"storage": 2}}))),
        vec![Err(403)],
    );
    script.post = 409;
    let refused = deploy_fake::serve(script);
    let (result, _) = restore_scripted(&refused);
    let error = result.expect_err("not set");
    assert!(
        matches!(&error, Error::Failed { version, why } if version == BEFORE && why.contains("403")),
        "{error}"
    );
}

fn restore_scripted(fake: &deploy_fake::Fake) -> (Result<String, Error>, String) {
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let mut out = Vec::new();
    let result = restore_on(
        &args(&fake.base),
        &fake.service(),
        &roots,
        &Time::default().clock(),
        &mut out,
    );
    (result, String::from_utf8(out).expect("utf8"))
}

/// Catches: the verb's entry points. `run_with` trusts the farm CA given as PEM (or the host
/// roots without one) and exits 1 with the restore's message; an unset `--url` sets nothing
/// and says so; a `--run` that makes no write id, a version not in the hosts' form and a bad
/// URL are refused before any call; and `deploy-farm` with neither a deploy's flags nor
/// `restore` names what it needs.
#[test]
fn the_entry_points_check_their_flags_and_trust_the_given_ca() {
    let fake = serve(Script {
        state: Some(State::released()),
        ..Script::new()
    });
    let mut out = Vec::new();
    let host = [fake.ca.clone()];
    let with_pem = |ca_pem: &str| restore::Args {
        ca_pem: ca_pem.to_owned(),
        ..args(&fake.base)
    };
    let set_back = restored().expect_err("an error").to_string();
    // The coordinator trusted by the host roots, then by the farm CA given as PEM (the
    // runner's token service, the same fake, by the host roots both times). The first sets
    // the farm back; the second then finds nothing of this release to set back.
    let failed = restore::run_with(
        &args(&fake.base),
        &token_service(&fake.base),
        &host,
        &mut out,
    )
    .expect_err("set back");
    assert_eq!(failed.to_string(), set_back);
    let nothing = restore::run_with(
        &with_pem(&fake.ca_pem),
        &token_service(&fake.base),
        &host,
        &mut out,
    )
    .expect("nothing to set back");
    assert!(
        nothing[0].starts_with("nothing to set back: the desired version is 1.0.0"),
        "{nothing:?}"
    );
    let untrusted = restore::run_with(
        &with_pem(&deploy_fake::fake_ca().1),
        &token_service(&fake.base),
        &host,
        &mut out,
    )
    .expect_err("untrusted");
    assert!(
        untrusted
            .to_string()
            .starts_with("read the farm before the restore: "),
        "{untrusted}"
    );
    let no_cert = restore::run_with(
        &with_pem("no PEM"),
        &token_service(&fake.base),
        &host,
        &mut out,
    )
    .expect_err("refused");
    assert_eq!(no_cert.to_string(), "--ca-pem holds no certificate");
    let unset = run_command(&Command {
        then: Some(Then::Restore(args(""))),
        deploy: None,
    })
    .expect("a notice");
    assert_eq!(
        unset,
        [
            "::notice::the restore step is off: the repository variable KBF_COORDINATOR_URL is not set"
        ]
    );
    for run in ["", "two words", &"x".repeat(200)] {
        let bad = restore::Args {
            run: run.to_owned(),
            ..args("")
        };
        let refused = restore::run(&bad).expect_err("refused");
        assert!(refused.to_string().starts_with("--run "), "{refused}");
    }
    let tag = restore::Args {
        version: "0.0.13".to_owned(),
        ..args("")
    };
    assert!(
        restore::run(&tag)
            .expect_err("refused")
            .to_string()
            .starts_with("--version ")
    );
    let (result, _, _) = restore_timed(&fake, &args("https://127.0.0.1:1/path"));
    assert!(matches!(result, Err(Error::Deploy(Deploy::Url(_)))));
    let neither = run_command(&Command {
        then: None,
        deploy: None,
    })
    .expect_err("refused");
    assert_eq!(
        neither.to_string(),
        "deploy-farm needs --url and --version, or `restore`"
    );
}

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");

fn xtask(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(XTASK);
    command.args(args);
    command
}

/// Catches: the verb as the workflow calls it. `deploy-farm restore` is a subcommand: an
/// unset `--url` prints the notice and exits 0; a job without `id-token: write` fails naming
/// the permission; a runner token service the system store does not trust fails at the token
/// call, before anything is asked of the coordinator; a deploy's flag given with `restore`,
/// and `restore` without `--run`, are usage errors (exit 2).
#[test]
fn the_verb_is_a_subcommand_that_needs_a_token_service_and_its_run() {
    let restore = |url: &str| {
        let mut command = xtask(&[
            "deploy-farm",
            "restore",
            "--url",
            url,
            "--version",
            RELEASE,
            "--run",
            RUN,
        ]);
        command.env_remove("ACTIONS_ID_TOKEN_REQUEST_URL");
        command
    };
    let off = restore("").output().expect("run");
    assert!(off.status.success(), "{off:?}");
    assert!(String::from_utf8_lossy(&off.stdout).starts_with("::notice::the restore step is off"));

    let fake = serve(Script::new());
    let no_token = restore(&fake.base).output().expect("run");
    assert_eq!(no_token.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_token.stderr).contains("id-token: write"));

    let untrusted = restore(&fake.base)
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            format!("{}/token", fake.base),
        )
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", RUNNER_KEY)
        .args(["--ca-pem", &fake.ca_pem])
        .output()
        .expect("run");
    assert_eq!(untrusted.status.code(), Some(1));
    let text = String::from_utf8_lossy(&untrusted.stderr);
    assert!(text.contains("ask the runner for an OIDC token"), "{text}");
    assert!(fake.seen().is_empty());
    assert_eq!(fake.state(), Some(State::before()));

    for usage in [
        xtask(&[
            "deploy-farm",
            "--url",
            "u",
            "restore",
            "--url",
            "u",
            "--version",
            RELEASE,
            "--run",
            RUN,
        ]),
        xtask(&["deploy-farm", "restore", "--url", "u", "--version", RELEASE]),
    ]
    .iter_mut()
    {
        assert_eq!(usage.output().expect("run").status.code(), Some(2));
    }
}
