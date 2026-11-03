//! `cargo xtask deploy-farm restore` at its edges, against the stateful fake coordinator:
//! values longer than the log shows, answers that are not what the coordinator sends, both
//! reads failing or tried again, and a POST whose outcome the restore cannot see. Every fake
//! is driven by the number of a request, and the clock is virtual.

#[path = "support/coordinator_fake.rs"]
mod coordinator_fake;
#[path = "support/deploy_fake.rs"]
mod deploy_fake;
#[path = "support/restore_steps.rs"]
mod restore_steps;

use std::time::Duration;

use coordinator_fake::{BEFORE, Competing, Fake, Fate, RELEASE, ReadFate, Script, State, serve};
use restore_steps::*;
use serde_json::json;
use xtask::deploy_farm::Error as Deploy;
use xtask::deploy_farm::restore::{BY_HAND, Error};

/// The methods of the requests to the coordinator, in order.
fn methods(fake: &Fake) -> Vec<String> {
    fake.seen().into_iter().map(|r| r.method).collect()
}

/// Whether every request presented a token no other request presented.
fn tokens_fresh(fake: &Fake) -> bool {
    let tokens: Vec<String> = fake.seen().into_iter().map(|r| r.authorization).collect();
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    distinct.len() == tokens.len()
}

fn script(fates: Vec<Fate>, read_fates: Vec<ReadFate>) -> Script {
    Script {
        state: Some(State::released()),
        fates,
        read_fates,
        ..Script::new()
    }
}

fn slept(time: &deploy_fake::Time) -> Vec<Duration> {
    time.slept.borrow().clone()
}

/// Catches: a version or write id compared or posted as the log shows it (cut to 64
/// characters). A replaced version of 120 characters is posted exactly, and a write id of 118
/// characters that landed behind a 503 is recognised as the restore's own on the read after
/// the 409.
#[test]
fn long_versions_and_write_ids_are_posted_and_compared_exactly() {
    let long = format!("1.0.0-{}+g000000000001", "z".repeat(100));
    let run = format!("{}-1", "9".repeat(108));
    let fake = serve(Script {
        state: Some(State {
            replaced: Some((long.clone(), json!({"storage": 2, "frontend": 1}))),
            ..State::released()
        }),
        fates: vec![Fate::Lands503],
        ..Script::new()
    });
    let mut long_run = args(&fake.base);
    long_run.run = run.clone();
    let (result, _, _) = restore_timed(&fake, &long_run);
    assert!(matches!(result, Err(Error::Restored { .. })), "{result:?}");
    let posts = fake.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].body["version"], long.as_str());
    assert_eq!(posts[0].body["write_id"], format!("restore-{run}"));
    let now = fake.state().expect("a state");
    assert_eq!(
        (now.version, now.write_id),
        (long, format!("restore-{run}"))
    );
}

/// Catches: a newer version taken for this release because both share the first 64
/// characters. The desired version differs from the release after its 70th character; the
/// restore sets nothing.
#[test]
fn a_superseding_version_sharing_a_long_prefix_is_not_this_release() {
    let release = format!("2.0.0-{}+g000000000002", "a".repeat(70));
    let newer = format!("2.0.0-{}b+g000000000002", "a".repeat(69));
    let fake = serve(Script {
        state: Some(State {
            version: newer.clone(),
            ..State::released()
        }),
        ..Script::new()
    });
    let mut long_release = args(&fake.base);
    long_release.version = release;
    let (result, _, _) = restore_timed(&fake, &long_release);
    let line = result.expect("nothing to set back");
    assert!(line.starts_with("nothing to set back"), "{line}");
    assert!(fake.posts().is_empty());
    assert_eq!(fake.state().map(|s| s.version), Some(newer));
}

/// Catches: another writer's id taken for the restore's once leading colons are dropped. A
/// competing write whose id is `::restore-4711-2` stands, and the restore reports another
/// writer.
#[test]
fn a_competing_id_that_differs_only_by_leading_colons_is_another_writer() {
    let colons = format!("::{WRITE_ID}");
    let fake = serve(Script {
        competing: vec![Competing {
            at_read: 0,
            version: RELEASE.to_owned(),
            min_up: json!({"storage": 1, "frontend": 1}),
            write_id: colons.clone(),
        }],
        ..script(Vec::new(), Vec::new())
    });
    let (result, _) = restore(&fake);
    assert!(
        matches!(&result, Err(Error::OtherWriter { desired, .. }) if desired == RELEASE),
        "{result:?}"
    );
    assert_eq!(fake.state().map(|s| s.write_id), Some(colons));
}

/// Catches: an answer that is not the coordinator's taken for one with a default in it, so
/// that a missing or mistyped `desired` reads as "another version" and the restore passes
/// with nothing set. Each is an error naming the field, and nothing is posted. An explicit
/// `null` is a coordinator with no desired state, which is nothing to set back.
#[test]
fn a_malformed_answer_before_the_post_is_an_error_and_posts_nothing() {
    let cases: [(&'static str, &str); 8] = [
        ("[]", "the convergence answer is not an object"),
        ("{}", "`desired` is neither a version nor null"),
        (
            r#"{"desired":17}"#,
            "`desired` is neither a version nor null",
        ),
        (
            r#"{"desired":"2.0.0+g000000000002","generation":"2","replaced":null,"write_id":""}"#,
            "`generation` is not a count",
        ),
        (
            r#"{"desired":"2.0.0+g000000000002","generation":2,"replaced":null,"write_id":5}"#,
            "`write_id` is not text",
        ),
        (
            r#"{"desired":"2.0.0+g000000000002","generation":2,"replaced":null}"#,
            "`write_id` is not text",
        ),
        (
            r#"{"desired":"2.0.0+g000000000002","generation":2,"replaced":"x","write_id":""}"#,
            "`replaced` is neither null nor a version with its minimums",
        ),
        (
            r#"{"desired":"2.0.0+g000000000002","generation":2,"replaced":{"version":"1.0.0+g000000000001"},"write_id":""}"#,
            "`replaced` is not a version with its minimums",
        ),
    ];
    for (body, why) in cases {
        let fake = serve(script(Vec::new(), vec![ReadFate::Raw(body)]));
        let (result, _) = restore(&fake);
        assert_eq!(
            result,
            Err(Error::Deploy(Deploy::Answer(why.to_owned()))),
            "{body}"
        );
        assert!(fake.posts().is_empty(), "{body}");
        assert_eq!(fake.state(), Some(State::released()), "{body}");
    }
    let none = serve(script(
        Vec::new(),
        vec![ReadFate::Raw(r#"{"desired":null}"#)],
    ));
    let line = restore(&none).0.expect("nothing to set back");
    assert!(
        line.starts_with("nothing to set back: the desired version is (none)"),
        "{line}"
    );
    assert!(none.posts().is_empty());
}

/// Catches: the first read's failures handled as anything but what they are. A 503 is tried
/// again after the pause, with a fresh token, and the restore goes on; four 503s end it with
/// the read's error and nothing posted; a 401 ends it at once, with no second read.
#[test]
fn the_first_read_is_tried_again_on_503_and_ends_on_exhaustion_or_refusal() {
    let fake = serve(script(Vec::new(), vec![ReadFate::Status(503)]));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    assert_eq!(result, restored());
    assert_eq!(methods(&fake), ["GET", "GET", "POST"]);
    assert_eq!(slept(&time), [Duration::from_secs(1)]);
    assert!(tokens_fresh(&fake));
    assert_eq!(fake.state(), Some(set_back(2)));

    let fake = serve(script(Vec::new(), vec![ReadFate::Status(503); 4]));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    assert_eq!(
        result,
        Err(Error::Deploy(Deploy::Call {
            what: "read the farm before the restore",
            why: "status 503".to_owned(),
        }))
    );
    assert_eq!(methods(&fake), ["GET"; 4]);
    assert_eq!(slept(&time).len(), 3);
    assert!(tokens_fresh(&fake));
    assert_eq!(fake.state(), Some(State::released()));

    let fake = serve(script(Vec::new(), vec![ReadFate::Status(401)]));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    assert_eq!(
        result,
        Err(Error::Deploy(Deploy::Refused {
            what: "read the farm before the restore",
            status: 401,
        }))
    );
    assert_eq!(methods(&fake), ["GET"]);
    assert!(slept(&time).is_empty());
}

/// Catches: the read after an unanswered POST handled as anything but what it is. A 503 on
/// it is tried again and the restore's own write is found; when every try fails, or its
/// answer cannot be read, or shows no generation, the restore says the outcome is not known
/// (the write landed here) instead of "could not set".
#[test]
fn the_read_after_an_unanswered_post_is_tried_again_and_an_unknown_end_is_named() {
    let fake = serve(script(
        vec![Fate::Drop],
        vec![ReadFate::Answer, ReadFate::Status(503)],
    ));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    assert_eq!(result, restored());
    assert_eq!(methods(&fake), ["GET", "POST", "GET", "GET"]);
    assert_eq!(slept(&time), [Duration::from_secs(1)]);
    assert!(tokens_fresh(&fake));

    let failing: [(Vec<ReadFate>, &str); 3] = [
        (
            vec![
                ReadFate::Answer,
                ReadFate::Status(503),
                ReadFate::Status(503),
                ReadFate::Status(503),
                ReadFate::Status(503),
            ],
            "read the farm after the restore was refused: status 503",
        ),
        (
            vec![ReadFate::Answer, ReadFate::Raw("[]")],
            "the convergence answer is not an object",
        ),
        (
            vec![
                ReadFate::Answer,
                ReadFate::Raw(r#"{"desired":"1.0.0+g000000000001"}"#),
            ],
            "the read after the restore shows no generation",
        ),
    ];
    for (read_fates, why) in failing {
        let fake = serve(script(vec![Fate::Drop], read_fates));
        let (result, _) = restore(&fake);
        let error = result.expect_err("not known");
        assert!(
            matches!(&error, Error::Unknown { version, write_id, why: w } if version == BEFORE && write_id == WRITE_ID && w.ends_with(why)),
            "{error}"
        );
        assert!(error.to_string().contains(BY_HAND), "{error}");
        assert_eq!(fake.state(), Some(set_back(2)), "{why}");
    }
}

/// Catches: a POST whose outcome is unseen classified by guess. Dropped before it was stored:
/// the read after it finds no write of the restore's, so it reports not set and the release
/// stays desired. Stored with an answer that is not JSON: the read finds the restore's own
/// write, so it is done. Refused with 401: no second try and no read after it. Three 503s
/// and then an answer on the fourth and last try: set back, after three pauses.
#[test]
fn a_post_whose_outcome_is_unseen_is_classified_by_its_write_id() {
    let fake = serve(script(vec![Fate::DropUnstored], Vec::new()));
    let error = restore(&fake).0.expect_err("not set");
    assert!(
        matches!(&error, Error::Failed { version, .. } if version == BEFORE),
        "{error}"
    );
    assert_eq!(fake.state(), Some(State::released()));
    assert_eq!(methods(&fake), ["GET", "POST", "GET"]);

    let fake = serve(script(vec![Fate::Garbled], Vec::new()));
    assert_eq!(restore(&fake).0, restored());
    assert_eq!(fake.state(), Some(set_back(2)));
    assert_eq!(methods(&fake), ["GET", "POST", "GET"]);

    let fake = serve(script(vec![Fate::Status(401)], Vec::new()));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    let error = result.expect_err("refused");
    assert!(
        matches!(&error, Error::Failed { why, .. } if why.contains("status 401")),
        "{error}"
    );
    assert_eq!(methods(&fake), ["GET", "POST"]);
    assert!(slept(&time).is_empty());
    assert_eq!(fake.state(), Some(State::released()));

    let fake = serve(script(vec![Fate::Status(503); 3], Vec::new()));
    let (result, _, time) = restore_timed(&fake, &args(&fake.base));
    assert_eq!(result, restored());
    assert_eq!(fake.posts().len(), 4);
    assert_eq!(slept(&time).len(), 3);
    assert!(tokens_fresh(&fake));
    assert_eq!(fake.state(), Some(set_back(2)));
}

/// Catches: advice that names commands this checkout does not have. The hand step the
/// restore names is re-running the `gamma` job, which `release.yml` has; it does not name the
/// coordinator's `force`, which is planned and not built.
#[test]
fn the_hand_step_named_is_one_this_checkout_supports() {
    let release = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.github/workflows/release.yml"),
    )
    .expect("release.yml");
    assert!(
        release.contains("\n  gamma:\n"),
        "release.yml has no gamma job"
    );
    assert!(
        BY_HAND.contains("re-run") && BY_HAND.contains("the `gamma` job"),
        "{BY_HAND}"
    );
    assert!(!BY_HAND.contains("force"), "{BY_HAND}");
}

/// Catches: the restore's own check of its flags skipped when it is called past the verb: a
/// `--run` that makes no write id is refused before any request.
#[test]
fn the_restore_checks_its_run_before_any_request() {
    let fake = serve(script(Vec::new(), Vec::new()));
    let mut bad = args(&fake.base);
    bad.run = String::new();
    let (result, _, _) = restore_timed(&fake, &bad);
    assert_eq!(result, Err(Error::Run(String::new())));
    assert!(fake.seen().is_empty());
}

/// Catches: a job budget derived from anything but the clients' own limits. A call with its
/// token is two calls of at most 60 seconds; a call tried again is four of them and three
/// pauses; the restore is three such calls (28.5 minutes at the default pause of 30 seconds);
/// the deploy is two of them, the deadline, a last read and a pause (81.5 minutes for a
/// deadline of 60). Each grows with the pause.
#[test]
fn the_bounds_of_the_verbs_come_from_their_calls_tries_and_pauses() {
    use xtask::deploy_farm::{EVERY_SECS, call_bound, deploy_bound, restore, retried_bound};
    let every = Duration::from_secs(EVERY_SECS);
    assert_eq!(EVERY_SECS, 30);
    assert_eq!(call_bound(), Duration::from_secs(120));
    assert_eq!(retried_bound(every), Duration::from_secs(4 * 120 + 3 * 30));
    assert_eq!(restore::bound(every), Duration::from_secs(1710));
    assert_eq!(
        deploy_bound(Duration::from_secs(3600), every),
        Duration::from_secs(2 * 570 + 3600 + 120 + 30)
    );
    let slower = Duration::from_secs(60);
    assert_eq!(
        restore::bound(slower),
        Duration::from_secs(3 * (4 * 120 + 3 * 60))
    );
    assert!(
        deploy_bound(Duration::from_secs(3600), slower)
            > deploy_bound(Duration::from_secs(3600), every)
    );
}

/// Catches: an unknown outcome whose message shows a shortened write id, which the operator
/// could never find in the coordinator's state. With a write id of 118 characters, a POST
/// whose answer is lost and a read after it that is refused, the message names the whole id.
#[test]
fn an_unknown_outcome_names_the_whole_write_id() {
    let run = format!("{}-1", "9".repeat(108));
    let id = format!("restore-{run}");
    let fake = serve(script(
        vec![Fate::Drop],
        vec![ReadFate::Answer, ReadFate::Status(403)],
    ));
    let mut long_run = args(&fake.base);
    long_run.run = run;
    let (result, _, _) = restore_timed(&fake, &long_run);
    let error = result.expect_err("not known");
    // The transport's own words for the lost answer are the TLS library's; the rest is exact.
    let Error::Unknown {
        version,
        write_id,
        why,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!((version.as_str(), write_id.as_str()), (BEFORE, id.as_str()));
    let lost = why
        .strip_suffix(", then read the farm after the restore was refused: refused with status 403: the coordinator's OIDC allow-list does not accept this job's token")
        .expect("the read after it, refused");
    assert!(lost.starts_with("set the farm back: "), "{lost}");
    assert_eq!(
        error.to_string(),
        format!(
            "whether the farm is set back to {BEFORE} is not known: {why}; read GET /v1/convergence for the write `{id}`, and if it is not there, use {BY_HAND}"
        )
    );
}
