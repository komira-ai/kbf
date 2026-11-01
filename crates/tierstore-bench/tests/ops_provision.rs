//! `provision`: what a dry run promises, what an apply does and in what order, the two probes
//! that make isolation a fact (the scoped account reaches its test bucket and is refused a live
//! one), and the rule that it changes only what it made: names inside the test prefix, a bucket or
//! account already on the store left alone, the secret kept off every command line.

mod ops_support;

use std::fs;

use tierstore_bench::config::{ConfigError, Secret};
use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::mc::Mc;
use tierstore_bench::preflight::ops::KeyPair;
use tierstore_bench::provision::state::{STATE_FILE, State};
use tierstore_bench::provision::{Mode, STEPS_FILE, Supply, policy_document, run, run_with};
use tierstore_bench::role::Role;

use ops_support::*;

/// A store that serves its own test buckets and refuses every `live-` one.
fn farm() -> impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static {
    |seen| {
        if seen.path.starts_with("/live-") {
            s3_error(403, "AccessDenied")
        } else {
            empty_listing()
        }
    }
}

fn mc_args(host: &FakeHost) -> Vec<Vec<String>> {
    host.ran()
        .into_iter()
        .map(|c| c.args[3..].to_vec())
        .collect()
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_owned()).collect()
}

/// Catches a dry run that changes something, or one that says nothing: no command runs, no
/// request is sent, and every step of both roles is printed as a plan, by role and not by bucket.
#[tokio::test]
async fn a_dry_run_lists_the_steps_and_changes_nothing() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-dry");
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    let steps = run_with(&host, &config, &ops, Mode::DryRun).await.unwrap();
    assert_eq!(steps.len(), 14);
    assert!(steps.iter().all(|s| s.starts_with("plan: role ")));
    assert_eq!(steps[0], "plan: role hot: make the test bucket");
    assert_eq!(
        steps[1],
        "plan: role hot: set the test bucket's hard quota to 30GiB"
    );
    assert_eq!(steps[7], "plan: role cold: make the test bucket");
    assert!(host.ran().is_empty());
    assert!(stub.requests().is_empty());
    let text = fs::read_to_string(dir.path().join(STEPS_FILE)).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(file["mode"], "dry-run");
    assert_eq!(file["steps"].as_array().unwrap().len(), 14);
    for word in PRIVATE {
        assert!(!text.contains(word), "{word}");
    }
    assert!(!dir.path().join("policy-hot.json").exists());
    assert!(!dir.path().join(STATE_FILE).exists());
}

/// Catches an apply that misses a step, runs them out of order, or puts the admin secret where a
/// process list shows it: the exact commands for both roles in order, the admin pair in the
/// environment only, and the scoped account's pair only on the input of `user add`.
#[tokio::test]
async fn an_apply_makes_the_bucket_quota_policy_and_account_in_order() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-apply");
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    let steps = run_with(&host, &config, &ops, Mode::Apply).await.unwrap();
    assert_eq!(steps.len(), 14);
    assert!(steps.iter().all(|s| s.starts_with("done: role ")));

    let policy = |role: &str| {
        dir.path()
            .join(format!("policy-{role}.json"))
            .display()
            .to_string()
    };
    let expected = [
        (
            "hot",
            "tb-hot-test-bucket-mark",
            "tb-KEYID-MARK-hot",
            "SECRET-MARK-hot",
        ),
        (
            "cold",
            "tb-cold-test-bucket-mark",
            "tb-KEYID-MARK-cold",
            "SECRET-MARK-cold",
        ),
    ];
    let mut want: Vec<Vec<String>> = Vec::new();
    // First everything is looked for, read-only, for both roles; then each role is made.
    for _ in expected {
        want.push(words(&["ls", "tbops"]));
        want.push(words(&["admin", "user", "list", "tbops"]));
    }
    for (role, bucket, key, _) in expected {
        // The name `plan` prints for the role, and the literal it must be.
        let name = format!("tb-tierstore-bench-{role}");
        assert!(config.plan_lines().contains(&format!("policy={name}")));
        let target = format!("tbops/{bucket}");
        want.push(words(&["ls", "tbops"]));
        want.push(words(&["mb", &target]));
        want.push(words(&["quota", "set", &target, "--size", "30GiB"]));
        want.push(words(&[
            "admin",
            "policy",
            "create",
            "tbops",
            &name,
            &policy(role),
        ]));
        want.push(words(&["admin", "user", "list", "tbops"]));
        want.push(words(&["admin", "user", "add", "tbops"]));
        want.push(words(&[
            "admin", "policy", "attach", "tbops", &name, "--user", key,
        ]));
    }
    assert_eq!(mc_args(&host), want);

    for cmd in host.ran() {
        assert_eq!(cmd.program.to_str(), Some("/private-mark/mc"));
        assert_eq!(
            &cmd.args[..3],
            [
                "--config-dir".to_owned(),
                dir.path().join("mc").display().to_string(),
                "--json".to_owned()
            ]
        );
        assert_eq!(cmd.env.len(), 1);
        assert!(cmd.env[0].1.contains("ADMIN-SECRET-MARK"));
        assert!(
            cmd.args.iter().all(|a| !a.contains("ADMIN")),
            "the admin pair is on a command line: {cmd:?}"
        );
        assert!(
            cmd.args.iter().all(|a| !a.contains("SECRET-MARK")),
            "a secret is on a command line: {cmd:?}"
        );
    }
    // The scoped account's pair reaches `mc` on its input, one value a line, and no other
    // command is given any input.
    let inputs: Vec<(String, String)> = host
        .ran()
        .into_iter()
        .filter_map(|c| c.stdin.map(|input| (c.args[3..].join(" "), input)))
        .collect();
    let [(hot_cmd, hot_in), (cold_cmd, cold_in)] = inputs.as_slice() else {
        panic!("{inputs:?}");
    };
    assert_eq!(hot_cmd, "admin user add tbops");
    assert_eq!(cold_cmd, "admin user add tbops");
    assert_eq!(hot_in, "tb-KEYID-MARK-hot\nSECRET-MARK-hot\n");
    assert_eq!(cold_in, "tb-KEYID-MARK-cold\nSECRET-MARK-cold\n");
    // What it made is written down, by name, for the next run to find.
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(
        record["buckets"],
        serde_json::json!(["tb-cold-test-bucket-mark", "tb-hot-test-bucket-mark"])
    );
    assert_eq!(
        record["accounts"],
        serde_json::json!(["tb-KEYID-MARK-cold", "tb-KEYID-MARK-hot"])
    );

    // Each role's account listed its test bucket and read from the named live bucket.
    let seen = stub.requests();
    let live: Vec<_> = seen
        .iter()
        .filter(|r| r.path.starts_with("/live-"))
        .collect();
    assert_eq!(live.len(), 2);
    assert!(
        live.iter()
            .any(|r| r.path.starts_with("/live-cold-bucket-mark/"))
    );
    assert!(
        live.iter()
            .any(|r| r.path.starts_with("/live-hot-bucket-mark/"))
    );
    assert!(live.iter().all(|r| r.method == "GET"));
    let own: Vec<_> = seen
        .iter()
        .filter(|r| !r.path.starts_with("/live-"))
        .collect();
    assert_eq!(own.len(), 2);
    assert!(own.iter().all(|r| r.query.contains("list-type=2")));

    let text = fs::read_to_string(dir.path().join("provision.json")).unwrap();
    assert!(text.contains("\"mode\": \"apply\""));
    for word in PRIVATE {
        assert!(!text.contains(word), "{word}");
    }
}

/// Catches a policy that reaches more than the test bucket: every resource is the bucket or its
/// objects, there is no wildcard action and no action that makes, drops or reconfigures a bucket,
/// and the file of an apply holds this document.
#[tokio::test]
async fn the_policy_names_the_test_bucket_and_nothing_else() {
    let doc = policy_document("bucket-x");
    let statements = doc["Statement"].as_array().unwrap();
    assert!(!statements.is_empty());
    for statement in statements {
        assert_eq!(statement["Effect"], "Allow");
        for resource in statement["Resource"].as_array().unwrap() {
            let r = resource.as_str().unwrap();
            assert!(
                r == "arn:aws:s3:::bucket-x" || r == "arn:aws:s3:::bucket-x/*",
                "{r}"
            );
        }
        for action in statement["Action"].as_array().unwrap() {
            let a = action.as_str().unwrap();
            assert!(a.starts_with("s3:") && !a.contains('*'), "{a}");
            for forbidden in [
                "CreateBucket",
                "DeleteBucket",
                "BucketPolicy",
                "PutBucket",
                "Lifecycle",
            ] {
                assert!(!a.contains(forbidden), "{a}");
            }
        }
    }

    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-policy");
    let config = run_config(dir.path(), &stub.endpoint, None);
    run_with(&host, &config, &ops, Mode::Apply).await.unwrap();
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("policy-cold.json")).unwrap())
            .unwrap();
    assert_eq!(written, policy_document("tb-cold-test-bucket-mark"));
}

/// Catches a scoped account that reaches a live bucket going unnoticed: the apply stops with an
/// error that says so, removes the account it just made, and says to remove it by hand when the
/// removal itself fails.
#[tokio::test]
async fn an_account_that_reaches_a_live_bucket_stops_the_apply_and_is_removed() {
    // A store that serves everything: the live bucket is reachable (the key is just not there).
    let stub = Stub::start(|seen| {
        if seen.path.starts_with("/live-") {
            s3_error(404, "NoSuchKey")
        } else {
            empty_listing()
        }
    })
    .await;
    let (dir, host, _c, ops) = base("tb-prov-leak");
    let config = run_config(dir.path(), &stub.endpoint, None);
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    let BenchError::Ops(text) = err else {
        panic!("{err:?}");
    };
    assert_eq!(
        text,
        "role cold: the scoped account was not refused the live bucket (the account was removed): got Reached"
    );
    let last = mc_args(&host).pop().unwrap();
    assert_eq!(
        last,
        words(&["admin", "user", "remove", "tbops", "tb-KEYID-MARK-cold"])
    );
    assert!(!dir.path().join(STEPS_FILE).exists());

    let (dir, host, _c, ops) = base("tb-prov-leak-stuck");
    let config = run_config(dir.path(), &stub.endpoint, None);
    host.answer(|cmd| {
        if cmd.args.contains(&"remove".to_owned()) {
            failed(1, "no")
        } else {
            ok("")
        }
    });
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("(REMOVE THE ACCOUNT BY HAND)"),
        "{err}"
    );
}

/// Catches an account that cannot use its own bucket going on to the live probe: the apply
/// stops before any request to a live bucket, with what the store said.
#[tokio::test]
async fn an_account_that_cannot_list_its_test_bucket_stops_before_the_live_probe() {
    let stub = Stub::start(|_| s3_error(403, "AccessDenied")).await;
    let (dir, host, _c, ops) = base("tb-prov-own");
    let config = run_config(dir.path(), &stub.endpoint, None);
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "role cold: the scoped account cannot list its test bucket: got Denied(\"AccessDenied\")"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|r| !r.path.starts_with("/live-"))
    );
}

/// Catches an apply that goes on after a command failed: the first failing command stops it, the
/// error names the command and not its arguments, and nothing after it ran.
#[tokio::test]
async fn a_failing_command_stops_the_apply_at_once() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-stop");
    let config = run_config(dir.path(), &stub.endpoint, None);
    host.answer(|cmd| {
        if cmd.args.contains(&"quota".to_owned()) {
            failed(1, "denied for SECRET-MARK")
        } else {
            ok("")
        }
    });
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "mc quota set: exited Some(1)");
    assert_eq!(
        host.ran().len(),
        5,
        "the two look-ups, then look-up, make bucket and the quota that failed"
    );
    assert!(stub.requests().is_empty());
}

/// Catches inputs that are not checked before anything is done: no store to provision, a role with
/// no live bucket named, a test bucket that is the live one, an endpoint that is not `http://`,
/// and an admin key file that is not there each stop the verb with a plain reason, and no command
/// has run.
#[tokio::test]
async fn bad_inputs_stop_the_verb_before_any_command() {
    let (dir, host, _c, ops) = base("tb-prov-inputs");
    let none = no_stores(dir.path());
    let err = run_with(&host, &none, &ops, Mode::Apply).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "the run configures no hot or cold store to provision"
    );

    let config = run_config(dir.path(), "http://host-mark:9000", None);
    let mut no_live = ops.clone();
    no_live.live_buckets.clear();
    let err = run_with(&host, &config, &no_live, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the ops file names no live bucket for cold"
    );

    let mut same = ops.clone();
    same.live_buckets.insert(
        tierstore_bench::role::Role::Cold,
        "tb-cold-test-bucket-mark".to_owned(),
    );
    let err = run_with(&host, &config, &same, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "role cold: the test bucket is the live bucket"
    );

    let bad_endpoint = run_config(dir.path(), "https://host-mark", None);
    let err = run_with(&host, &bad_endpoint, &ops, Mode::DryRun)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "a store endpoint must be http://host[:port]"
    );

    host.files
        .lock()
        .unwrap()
        .remove(std::path::Path::new("/private-mark/admin-cold.json"));
    let err = run_with(&host, &config, &ops, Mode::DryRun)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(host.ran().is_empty());
}

/// Catches `run` that does not read `ops.json` from the result directory: without the file it
/// stops naming it; with one it goes as far as the admin key file and stops there, having
/// run nothing.
#[tokio::test]
async fn run_reads_the_ops_file_of_the_result_directory() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-prov-run");
    let config = run_config(dir.path(), "http://host-mark:9000", None);
    let err = run(&config, Mode::DryRun, Supply::Create)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("ops.json"));

    fs::write(dir.path().join("ops.json"), "{}").unwrap();
    let err = run(&config, Mode::DryRun, Supply::Create)
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("ops.json is not an ops file"),
        "{err}"
    );

    fs::write(dir.path().join("ops.json"), ops_json().to_string()).unwrap();
    let err = run(&config, Mode::DryRun, Supply::Create)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
}

/// Catches a result directory that cannot hold the policy or the steps going unreported: a policy
/// file that is a directory stops the apply at the policy step; a steps file that is a directory
/// ends an apply that ran every step in an error.
#[tokio::test]
async fn a_policy_or_steps_file_that_cannot_be_written_is_an_error() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-write");
    let config = run_config(dir.path(), &stub.endpoint, None);
    fs::create_dir_all(dir.path().join("policy-cold.json")).unwrap();
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert_eq!(
        host.ran().len(),
        5,
        "the two look-ups, bucket and quota, then the policy file failed"
    );

    let (dir, host, _c, ops) = base("tb-prov-write-steps");
    let config = run_config(dir.path(), &stub.endpoint, None);
    fs::create_dir_all(dir.path().join(STEPS_FILE)).unwrap();
    let err = run_with(&host, &config, &ops, Mode::DryRun)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(host.ran().is_empty());
}

fn ran_args(host: &FakeHost) -> Vec<String> {
    mc_args(host).into_iter().map(|a| a.join(" ")).collect()
}

/// A machine whose store already holds the named bucket and account ids, as `mc ls` and `mc admin
/// user list` print them (one JSON object a line); every other command succeeds.
fn store_holding(
    buckets: &[&str],
    accounts: &[&str],
) -> impl Fn(&Cmd) -> std::io::Result<CmdOutput> + Send + Sync + 'static {
    let listing: String = buckets
        .iter()
        .map(|b| format!("{{\"status\":\"success\",\"type\":\"folder\",\"key\":\"{b}/\"}}\n"))
        .collect();
    let users: String = accounts
        .iter()
        .map(|a| {
            format!("{{\"status\":\"success\",\"accessKey\":\"{a}\",\"userStatus\":\"enabled\"}}\n")
        })
        .collect();
    move |cmd| {
        let has = |word: &str| cmd.args.iter().any(|a| a == word);
        if has("ls") {
            ok(&listing)
        } else if has("list") {
            ok(&users)
        } else {
            ok("")
        }
    }
}

use tierstore_bench::preflight::host::{Cmd, CmdOutput};

/// Catches a name outside the test prefix reaching a store: a bucket of the farm, or an account
/// id that is not the run's, stops the verb (dry run or apply) with the value named, before any
/// command is run or request sent.
#[tokio::test]
async fn a_name_outside_the_test_prefix_stops_provision_before_any_store_call() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-prefix");
    for mode in [Mode::DryRun, Mode::Apply] {
        let mut config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
        config.stores.get_mut(&Role::Cold).unwrap().bucket = "farm-cold-bucket".to_owned();
        let err = run_with(&host, &config, &ops, mode).await.unwrap_err();
        let BenchError::Config(ConfigError::OutsidePrefix {
            role, what, value, ..
        }) = &err
        else {
            panic!("{err:?}");
        };
        assert_eq!(
            (*role, *what, value.as_str()),
            (Role::Cold, "bucket", "farm-cold-bucket")
        );
        assert!(err.to_string().contains("farm-cold-bucket"), "{err}");

        let mut config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
        config.stores.get_mut(&Role::Hot).unwrap().access_key_id = "farm-hot-key".to_owned();
        let err = run_with(&host, &config, &ops, mode).await.unwrap_err();
        assert!(err.to_string().contains("farm-hot-key"), "{err}");
    }
    assert!(host.ran().is_empty());
    assert!(stub.requests().is_empty());
    assert!(!dir.path().join(STATE_FILE).exists());
}

/// Catches `provision` changing a bucket it did not make (`mc mb --ignore-existing` kept it
/// silently, then the quota and the policy were put on it): the bucket is on the store and the
/// run has no record of making it, so the verb stops at the look-up; nothing was made, set or
/// recorded.
#[tokio::test]
async fn a_bucket_already_on_the_store_that_the_run_did_not_make_is_refused() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-foreign-bucket");
    let config = run_config(dir.path(), &stub.endpoint, None);
    host.answer(store_holding(&["tb-cold-test-bucket-mark"], &[]));
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "role cold: the test bucket is already on the store and this run did not make it"
    );
    assert_eq!(
        ran_args(&host),
        ["ls tbops"],
        "one read-only look-up and nothing else"
    );
    assert!(!dir.path().join(STATE_FILE).exists());
    assert!(!dir.path().join("policy-cold.json").exists());
}

/// Catches `provision` resetting the secret of a live account (`mc admin user add` does that to an
/// account that exists): the account is on the store and not the run's, so the verb stops after
/// two read-only look-ups, before it makes the bucket, sets the quota or writes the policy.
#[tokio::test]
async fn an_account_already_on_the_store_that_the_run_did_not_make_is_refused_before_anything_changes()
 {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-foreign-account");
    let config = run_config(dir.path(), &stub.endpoint, None);
    host.answer(store_holding(
        &["other-bucket"],
        &["tb-KEYID-MARK-cold", "someone-else"],
    ));
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "role cold: the scoped account is already on the store and this run did not make it"
    );
    assert_eq!(ran_args(&host), ["ls tbops", "admin user list tbops"]);
    assert!(!dir.path().join(STATE_FILE).exists());
    assert!(stub.requests().is_empty());
}

/// Catches a second apply of the same run failing on, or redoing, what the first made: the run's
/// own bucket and account are on the store and in its record, so they are found and kept (no
/// `mb`, no `user add`), and the quota, the policy and the attachment are put again.
#[tokio::test]
async fn a_second_apply_of_the_same_run_keeps_what_it_made() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-again");
    let config = run_config(dir.path(), &stub.endpoint, None);
    run_with(&host, &config, &ops, Mode::Apply).await.unwrap();
    assert!(ran_args(&host).iter().any(|c| c.starts_with("mb ")));

    let again = healthy_host();
    again.answer(store_holding(
        &["tb-cold-test-bucket-mark"],
        &["tb-KEYID-MARK-cold"],
    ));
    let steps = run_with(&again, &config, &ops, Mode::Apply).await.unwrap();
    assert_eq!(steps.len(), 7);
    let ran = ran_args(&again);
    assert!(
        ran.iter()
            .all(|c| !c.starts_with("mb ") && c != "admin user add tbops"),
        "{ran:?}"
    );
    for kept in ["quota set", "admin policy create", "admin policy attach"] {
        assert!(ran.iter().any(|c| c.starts_with(kept)), "{kept}: {ran:?}");
    }
    assert!(
        again.ran().iter().all(|c| c.stdin.is_none()),
        "no secret is sent again"
    );
}

/// Catches a record that cannot be read being taken for an empty one (the run would then call its
/// own bucket someone else's, or the reverse): a file that is not a record, and one that cannot be
/// read, each stop the verb before any command.
#[tokio::test]
async fn a_damaged_record_stops_the_verb_before_any_command() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prov-record");
    let config = run_config(dir.path(), &stub.endpoint, None);
    fs::write(dir.path().join(STATE_FILE), "{\"buckets\": 3}").unwrap();
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a provision record"), "{err}");
    fs::remove_file(dir.path().join(STATE_FILE)).unwrap();
    fs::create_dir(dir.path().join(STATE_FILE)).unwrap();
    let err = run_with(&host, &config, &ops, Mode::Apply)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(host.ran().is_empty());
}

/// A client over `host` for the fixtures' store.
fn mc_on<'a>(host: &'a FakeHost, dir: &std::path::Path) -> Mc<'a> {
    let admin = KeyPair {
        access_key_id: "ADMIN-KEYID-MARK".to_owned(),
        secret_access_key: Secret::new("ADMIN-SECRET-MARK"),
    };
    Mc::new(
        host,
        std::path::Path::new("/private-mark/mc"),
        dir,
        "http://host-mark:9000",
        &admin,
    )
    .unwrap()
}

/// Catches an account being removed that the run did not make, or one without the prefix: the
/// removal runs no command for a name the record lacks, or one recorded without the prefix (a
/// hand-edited record), and removes, and forgets, one the run made.
#[test]
fn only_an_account_the_run_made_and_that_carries_the_prefix_is_removed() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-prov-remove");
    let host = FakeHost::new();
    let mc = mc_on(&host, dir.path());
    let mut state = State::load(dir.path(), "tb-").unwrap();

    let err = state.remove_account(&mc, "tb-live-account").unwrap_err();
    assert!(err.to_string().contains("not made by this run"), "{err}");
    state.record_account("farm-account").unwrap();
    assert!(
        !state.made_account("farm-account"),
        "no prefix, not the run's"
    );
    state.remove_account(&mc, "farm-account").unwrap_err();
    assert!(host.ran().is_empty());

    state.record_account("tb-mine").unwrap();
    assert!(state.made_account("tb-mine"));
    state.remove_account(&mc, "tb-mine").unwrap();
    assert_eq!(ran_args(&host), ["admin user remove tbops tb-mine"]);
    assert!(!state.made_account("tb-mine"));
    let reloaded = State::load(dir.path(), "tb-").unwrap();
    assert!(!reloaded.made_account("tb-mine"), "the record was saved");
    state.remove_account(&mc, "tb-mine").unwrap_err();
    assert_eq!(host.ran().len(), 1, "removed once");

    let mut other_run = State::load(dir.path(), "other-").unwrap();
    other_run.record_bucket("tb-bucket").unwrap();
    assert!(
        !other_run.made_bucket("tb-bucket"),
        "another prefix, not the run's"
    );
}

/// Catches the scoped secret being able to shift the lines `mc` reads (a line break in the id would
/// make the secret the id's tail), a look-up that cannot be read being taken for "absent", and
/// the parsing of the two look-ups: the matching line among others, and an answer that is not JSON.
#[test]
fn the_account_pair_is_one_line_each_and_a_look_up_that_fails_is_not_absence() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-prov-mc");
    let host = FakeHost::new();
    let mc = mc_on(&host, dir.path());
    for (id, secret) in [
        (
            "a
b", "s",
        ),
        (
            "a", "s
t",
        ),
        (
            "a
", "s",
        ),
    ] {
        let key = KeyPair {
            access_key_id: id.to_owned(),
            secret_access_key: Secret::new(secret),
        };
        let err = mc.add_account(&key).unwrap_err();
        assert!(err.to_string().contains("line break"), "{err}");
    }
    assert!(host.ran().is_empty());

    host.answer(store_holding(&["one", "tb-two"], &["u1", "tb-user"]));
    assert!(mc.bucket_exists("tb-two").unwrap());
    assert!(
        !mc.bucket_exists("tb").unwrap(),
        "a prefix of a name is not the name"
    );
    assert!(mc.account_exists("tb-user").unwrap());
    assert!(!mc.account_exists("tb").unwrap());

    host.answer(|_| ok("not json at all\n"));
    assert!(
        mc.bucket_exists("x")
            .unwrap_err()
            .to_string()
            .contains("not JSON")
    );
    assert!(mc.account_exists("x").is_err());
    host.answer(|_| failed(1, "unreachable"));
    assert!(mc.bucket_exists("x").is_err());
    assert!(mc.account_exists("x").is_err());
}

/// Catches a record that cannot be saved going unreported (the run would carry on with nothing
/// written down, and the next run would call its bucket someone else's): a result directory that
/// turns into a file, and a record file that turns into a directory, each make the recording
/// fail with the file named.
#[test]
fn a_record_that_cannot_be_saved_is_an_error() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-prov-save");
    let results = dir.path().join("results");
    let mut state = State::load(&results, "tb-").unwrap();
    fs::write(&results, "a file where the directory should be").unwrap();
    let err = state.record_bucket("tb-b").unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");

    let other = dir.path().join("other");
    let mut state = State::load(&other, "tb-").unwrap();
    fs::create_dir_all(other.join(STATE_FILE)).unwrap();
    let err = state.record_account("tb-a").unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains(STATE_FILE), "{err}");
}
