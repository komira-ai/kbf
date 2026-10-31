//! The binary: its exit status and what it prints.

use std::process::Command;

use kbf_testkit::dir::TestDir;

/// Catches `main` that swallows a stopped verb: exit status 2 and the error on stderr (here the
/// ops file a preflight reads first, which is not there).
#[test]
fn a_stopped_verb_exits_2_and_says_what_stopped_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-bin");
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["preflight", "--test-prefix", "tb-", "--result-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ops.json"), "{stderr}");
}

/// Catches a report that is built and not printed: the table of an empty result directory is on
/// standard output and the status is 0, and a directory that is not there is a stopped verb.
#[test]
fn report_prints_its_table_and_a_missing_directory_stops_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-report");
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["report", "--result-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("# tierstore-bench report"));
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["report", "--result-dir"])
        .arg(dir.path().join("absent"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// Catches a binary that cannot print its help, which means a clap definition that does not
/// build, and a verb left out of it.
#[test]
fn help_lists_every_verb() {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for verb in [
        "preflight",
        "plan",
        "provision",
        "run",
        "fault-proxy",
        "verify-teardown",
        "report",
    ] {
        assert!(text.contains(verb), "{verb}");
    }
}

/// Catches a `plan` that prints something else than the contract's lines: standard output is
/// `key=value` lines only (the prefix, then each store's bucket, then the account ids of `hot`
/// and `cold`), the status is 0, and a bucket outside the prefix is exit status 2 with the value
/// named and nothing on standard output.
#[test]
fn plan_prints_the_prefix_the_buckets_and_the_accounts() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-plan-bin");
    let file = dir.path().join("run.json");
    let write = |hot_bucket: &str| {
        std::fs::write(
            &file,
            format!(
                r#"{{"test_prefix": "bench-", "stores": {{
                "hot": {{"endpoint": "http://h.example:1", "bucket": "{hot_bucket}",
                         "access_key_id": "bench-hot-key", "secret_access_key": "SECRET-ONE"}},
                "cold": {{"endpoint": "http://c.example:1", "bucket": "bench-cold",
                          "access_key_id": "bench-cold-key", "secret_access_key": "SECRET-TWO"}},
                "instance": {{"endpoint": "http://i.example:1", "bucket": "bench-front",
                              "access_key_id": "front-key", "secret_access_key": "SECRET-THREE"}}}}}}"#
            ),
        )
        .unwrap();
    };
    let plan = || {
        Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
            .args(["plan", "--result-dir"])
            .arg(dir.path())
            .arg("--run-file")
            .arg(&file)
            .output()
            .unwrap()
    };

    write("bench-hot");
    let out = plan();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "test_prefix=bench-\n\
         bucket=bench-hot\naccount=bench-hot-key\npolicy=bench-tierstore-bench-hot\n\
         bucket=bench-cold\naccount=bench-cold-key\npolicy=bench-tierstore-bench-cold\n\
         bucket=bench-front\n"
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("SECRET"));

    write("live-hot");
    let out = plan();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("live-hot"));
}
