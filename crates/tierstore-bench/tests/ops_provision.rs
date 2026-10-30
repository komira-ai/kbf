//! `provision`: what a dry run promises, what an apply does and in what order, and the two probes
//! that make isolation a fact: the scoped account reaches its test bucket and is refused a live
//! one.

mod ops_support;

use std::fs;

use tierstore_bench::error::BenchError;
use tierstore_bench::provision::{Mode, STEPS_FILE, policy_document, run, run_with};

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
}

/// Catches an apply that misses a step, runs them out of order, or puts the admin secret where a
/// process list shows it: the exact commands for both roles in order, the admin pair in the
/// environment only, and the scoped account's pair only in `user add`.
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
            "hot-test-bucket-mark",
            "KEYID-MARK-hot",
            "SECRET-MARK-hot",
        ),
        (
            "cold",
            "cold-test-bucket-mark",
            "KEYID-MARK-cold",
            "SECRET-MARK-cold",
        ),
    ];
    let mut want: Vec<Vec<String>> = Vec::new();
    for (role, bucket, key, secret) in expected {
        let name = format!("tierstore-bench-{role}");
        let target = format!("tbops/{bucket}");
        want.push(words(&["mb", "--ignore-existing", &target]));
        want.push(words(&["quota", "set", &target, "--size", "30GiB"]));
        want.push(words(&[
            "admin",
            "policy",
            "create",
            "tbops",
            &name,
            &policy(role),
        ]));
        want.push(words(&["admin", "user", "add", "tbops", key, secret]));
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
    }

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
    assert_eq!(written, policy_document("cold-test-bucket-mark"));
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
        words(&["admin", "user", "remove", "tbops", "KEYID-MARK-cold"])
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
        2,
        "make bucket, then the quota that failed"
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
        "cold-test-bucket-mark".to_owned(),
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
    let err = run(&config, Mode::DryRun).await.unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
    assert!(err.to_string().contains("ops.json"));

    fs::write(dir.path().join("ops.json"), "{}").unwrap();
    let err = run(&config, Mode::DryRun).await.unwrap_err();
    assert!(
        err.to_string().starts_with("ops.json is not an ops file"),
        "{err}"
    );

    fs::write(dir.path().join("ops.json"), ops_json().to_string()).unwrap();
    let err = run(&config, Mode::DryRun).await.unwrap_err();
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
        2,
        "bucket and quota, then the policy file failed"
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
