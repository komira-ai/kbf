//! `provision --premade`: the buckets, quotas, policies and accounts were made by a held script
//! that holds the rights to make them; the verb makes none of them. It checks that each bucket is
//! there, that its quota is set and no larger than the run's, and proves the account with the two
//! probes. It records what it found, with a `premade` marker, only after the probes pass, and it
//! never removes an account (it holds no right to).

mod ops_support;

use std::fs;
use std::io;

use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::host::{Cmd, CmdOutput};
use tierstore_bench::provision::state::{STATE_FILE, State};
use tierstore_bench::provision::{Mode, STEPS_FILE, Supply, run_with, run_with_supply};

use ops_support::*;

const QUOTA_30_GIB: u64 = 32_212_254_720;

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

/// What `mc quota info --json` prints for a bucket with this quota and type: an indented,
/// multi-line object whose `quota` and `type` keys are omitted when empty (mc `quota-set.go`).
fn quota_text(quota: Option<u64>, kind: Option<&str>) -> String {
    let mut fields = vec![
        "\"status\": \"success\"".to_owned(),
        "\"bucket\": \"b\"".to_owned(),
    ];
    if let Some(bytes) = quota {
        fields.push(format!("\"quota\": {bytes}"));
    }
    if let Some(kind) = kind {
        fields.push(format!("\"type\": \"{kind}\""));
    }
    format!("{{\n {}\n}}\n", fields.join(",\n "))
}

fn quota_line(quota: Option<u64>) -> String {
    quota_text(quota, quota.map(|_| "hard"))
}

/// A machine whose store holds the buckets in `present`, each with `quota`. It lets through only
/// the read-only commands of the key a campaign leaves on the dev box (`ls` of one bucket, `quota
/// info`): every command that makes, changes or lists accounts, policies or quotas, `mb`, and
/// `ls` of the whole alias exits 1, as the store would refuse them.
fn premade_store(
    present: &'static [&'static str],
    quota: Option<u64>,
) -> impl Fn(&Cmd) -> io::Result<CmdOutput> + Send + Sync + 'static {
    premade_store_saying(present, quota_line(quota))
}

/// [`premade_store`], answering `quota info` with `line` as it stands.
fn premade_store_saying(
    present: &'static [&'static str],
    line: String,
) -> impl Fn(&Cmd) -> io::Result<CmdOutput> + Send + Sync + 'static {
    move |cmd| {
        let args: Vec<&str> = cmd.args[3..].iter().map(String::as_str).collect();
        match args.as_slice() {
            ["ls", target] => {
                let bucket = target.strip_prefix("tbops/").unwrap_or("");
                if present.contains(&bucket) {
                    ok(&format!(
                        "{{\"status\":\"success\",\"type\":\"folder\",\"key\":\"{bucket}/\"}}\n"
                    ))
                } else {
                    failed(1, "Bucket does not exist")
                }
            }
            ["quota", "info", _] => ok(&line),
            other => failed(1, &format!("not allowed to the dev-box key: {other:?}")),
        }
    }
}

const BOTH: &[&str] = &["tb-hot-test-bucket-mark", "tb-cold-test-bucket-mark"];
const COLD: &[&str] = &["tb-cold-test-bucket-mark"];

fn mc_args(host: &FakeHost) -> Vec<String> {
    host.ran()
        .into_iter()
        .map(|c| c.args[3..].join(" "))
        .collect()
}

/// Catches a premade run that makes, changes or lists something: with a store that refuses every
/// such command, an apply succeeds and the commands it ran are exactly the look-ups (the bucket
/// by `ls`, the quota by `quota info`, hot then cold), all of them before the first probe request;
/// no command gets input, and the steps say `premade`.
#[tokio::test]
async fn a_premade_apply_issues_none_of_the_creating_commands() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-apply");
    host.answer(premade_store(BOTH, Some(QUOTA_30_GIB)));
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    let steps = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap();
    assert_eq!(
        mc_args(&host),
        [
            "ls tbops/tb-hot-test-bucket-mark",
            "quota info tbops/tb-hot-test-bucket-mark",
            "ls tbops/tb-cold-test-bucket-mark",
            "quota info tbops/tb-cold-test-bucket-mark",
        ]
    );
    assert!(host.ran().iter().all(|c| c.stdin.is_none()));
    assert_eq!(steps.len(), 10);
    assert!(
        steps
            .iter()
            .all(|s| s.starts_with("done: role ") && s.contains("premade")),
        "{steps:?}"
    );
    assert!(
        !dir.path().join("policy-hot.json").exists()
            && !dir.path().join("policy-cold.json").exists(),
        "no policy document is written"
    );
    // Both accounts reached their own bucket and were refused a live one.
    let seen = stub.requests();
    assert_eq!(seen.len(), 4);
    assert_eq!(
        seen.iter().filter(|r| r.path.starts_with("/live-")).count(),
        2
    );
    let text = fs::read_to_string(dir.path().join(STEPS_FILE)).unwrap();
    assert!(text.contains("\"mode\": \"apply-premade\""), "{text}");
    for word in PRIVATE {
        assert!(!text.contains(word), "{word}");
    }
}

/// Catches a premade dry run that runs a command, sends a request, writes a record, or does not
/// say `premade` in its plan.
#[tokio::test]
async fn a_premade_dry_run_plans_the_checks_and_changes_nothing() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-dry");
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    let steps = run_with_supply(&host, &config, &ops, Mode::DryRun, Supply::Premade)
        .await
        .unwrap();
    assert_eq!(steps.len(), 10);
    assert!(
        steps
            .iter()
            .all(|s| s.starts_with("plan: role ") && s.contains("premade")),
        "{steps:?}"
    );
    assert!(steps.iter().all(|s| !s.contains("make the test bucket")));
    assert!(host.ran().is_empty());
    assert!(stub.requests().is_empty());
    assert!(!dir.path().join(STATE_FILE).exists());
    let text = fs::read_to_string(dir.path().join(STEPS_FILE)).unwrap();
    assert!(text.contains("\"mode\": \"dry-run-premade\""), "{text}");
}

/// Catches a premade run that goes on without its bucket: the cold bucket is absent (the store
/// refuses the `ls`), the verb stops naming the held script, no account has sent a request (the
/// hot one included, whose bucket is there) and nothing is recorded.
#[tokio::test]
async fn an_absent_bucket_is_refused_before_any_probe() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-absent");
    host.answer(premade_store(
        &["tb-hot-test-bucket-mark"],
        Some(QUOTA_30_GIB),
    ));
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.starts_with("role cold: "), "{text}");
    assert!(text.contains("admin_mint.sh"), "{text}");
    assert!(stub.requests().is_empty());
    assert!(!dir.path().join(STATE_FILE).exists());
    assert!(!dir.path().join(STEPS_FILE).exists());
}

/// Catches an `ls` that cannot be run being read as an absent bucket, or as a present one.
#[tokio::test]
async fn a_look_up_that_cannot_be_run_stops_the_verb() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-nostart");
    host.answer(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let config = run_config(dir.path(), &stub.endpoint, None);
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot start"), "{err}");
    assert!(stub.requests().is_empty());
}

/// Catches a bucket with no quota, or one over the run's 30 GiB, being used: each stops the verb
/// before a probe, names the held script, and a quota at or under the run's is accepted.
#[tokio::test]
async fn a_missing_or_too_large_quota_is_refused() {
    for (name, quota, refused) in [
        ("tb-prem-q-none", None, true),
        ("tb-prem-q-zero", Some(0), true),
        ("tb-prem-q-big", Some(QUOTA_30_GIB + 1), true),
        ("tb-prem-q-edge", Some(QUOTA_30_GIB), false),
        ("tb-prem-q-small", Some(1 << 30), false),
    ] {
        let stub = Stub::start(farm()).await;
        let (dir, host, _c, ops) = base(name);
        host.answer(premade_store(COLD, quota));
        let config = run_config(dir.path(), &stub.endpoint, None);
        let got = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade).await;
        if refused {
            let text = got.unwrap_err().to_string();
            assert!(text.starts_with("role cold: "), "{name}: {text}");
            assert!(text.contains("quota"), "{name}: {text}");
            assert!(text.contains("admin_mint.sh"), "{name}: {text}");
            assert!(stub.requests().is_empty(), "{name}");
            assert!(!dir.path().join(STATE_FILE).exists(), "{name}");
        } else {
            got.unwrap();
        }
    }
}

/// Catches a quota that cannot be read being taken for one that is fine: a failing `quota info`
/// and an answer that is not JSON each stop the verb before a probe.
#[tokio::test]
async fn a_quota_that_cannot_be_read_stops_the_verb() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-q-unread");
    let config = run_config(dir.path(), &stub.endpoint, None);
    let inner = premade_store(COLD, Some(QUOTA_30_GIB));
    host.answer(move |cmd| {
        if cmd.args.contains(&"quota".to_owned()) {
            failed(1, "denied")
        } else {
            inner(cmd)
        }
    });
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "mc quota info: exited Some(1)");
    host.answer(|cmd| {
        if cmd.args.contains(&"quota".to_owned()) {
            ok("not json\n")
        } else {
            premade_store(COLD, Some(QUOTA_30_GIB))(cmd)
        }
    });
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not JSON"), "{err}");
    assert!(stub.requests().is_empty());
}

/// Catches a premade run that removes the account it cannot trust, or goes on past it: the
/// scoped key reaches a live bucket, the verb stops with the instruction to run the held script
/// with `--remove`, no `admin user remove` (or any admin command) is issued, and nothing is
/// recorded.
#[tokio::test]
async fn a_live_probe_failure_stops_without_removing_anything() {
    let stub = Stub::start(|seen| {
        if seen.path.starts_with("/live-") {
            s3_error(404, "NoSuchKey")
        } else {
            empty_listing()
        }
    })
    .await;
    let (dir, host, _c, ops) = base("tb-prem-leak");
    host.answer(premade_store(COLD, Some(QUOTA_30_GIB)));
    let config = run_config(dir.path(), &stub.endpoint, None);
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    let BenchError::Ops(text) = err else {
        panic!("{err:?}");
    };
    assert!(
        text.contains(
            "the scoped account can read a live bucket: run admin_mint.sh --campaign ... --remove now"
        ),
        "{text}"
    );
    assert_eq!(
        mc_args(&host),
        [
            "ls tbops/tb-cold-test-bucket-mark",
            "quota info tbops/tb-cold-test-bucket-mark"
        ]
    );
    assert!(!dir.path().join(STATE_FILE).exists());
    assert!(!dir.path().join(STEPS_FILE).exists());
}

/// Catches an account that cannot use its own bucket going on to the live probe, or being
/// recorded: the verb stops before any request to a live bucket and the record is not written.
#[tokio::test]
async fn an_account_that_cannot_list_its_bucket_is_not_recorded() {
    let stub = Stub::start(|_| s3_error(403, "AccessDenied")).await;
    let (dir, host, _c, ops) = base("tb-prem-own");
    host.answer(premade_store(COLD, Some(QUOTA_30_GIB)));
    let config = run_config(dir.path(), &stub.endpoint, None);
    let err = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("role cold: the scoped account cannot list its test bucket"),
        "{err}"
    );
    assert!(
        stub.requests()
            .iter()
            .all(|r| !r.path.starts_with("/live-"))
    );
    assert!(!dir.path().join(STATE_FILE).exists());
}

/// Catches a record that is missing, written before the probes, or unmarked: after the probes
/// pass the record holds the pre-made bucket and account of each role with `premade` true, and a
/// run that made them itself writes a record with no such key.
#[tokio::test]
async fn the_record_is_written_after_the_probes_with_the_premade_marker() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-record");
    host.answer(premade_store(BOTH, Some(QUOTA_30_GIB)));
    let config = run_config(dir.path(), &stub.endpoint, Some(&stub.endpoint));
    run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap();
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(record["premade"], true);
    assert_eq!(
        record["buckets"],
        serde_json::json!(["tb-cold-test-bucket-mark", "tb-hot-test-bucket-mark"])
    );
    assert_eq!(
        record["accounts"],
        serde_json::json!(["tb-KEYID-MARK-cold", "tb-KEYID-MARK-hot"])
    );
    let state = State::load(dir.path(), "tb-").unwrap();
    assert!(state.made_bucket("tb-hot-test-bucket-mark"));
    assert!(state.made_account("tb-KEYID-MARK-cold"));

    let (dir, host, _c, ops) = base("tb-prem-record-default");
    let config = run_config(dir.path(), &stub.endpoint, None);
    run_with(&host, &config, &ops, Mode::Apply).await.unwrap();
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert!(record.get("premade").is_none(), "{record}");
}

/// Catches a record schema that is not backward compatible: a record without the key reads as
/// not premade, and one with it reads.
#[test]
fn a_record_without_the_premade_key_still_reads() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tb-prem-compat");
    fs::write(
        dir.path().join(STATE_FILE),
        r#"{"buckets": ["tb-a"], "accounts": ["tb-b"]}"#,
    )
    .unwrap();
    let state = State::load(dir.path(), "tb-").unwrap();
    assert!(state.made_bucket("tb-a") && state.made_account("tb-b"));
    assert!(!state.is_premade());
    fs::write(
        dir.path().join(STATE_FILE),
        r#"{"buckets": ["tb-a"], "accounts": ["tb-b"], "premade": true}"#,
    )
    .unwrap();
    let state = State::load(dir.path(), "tb-").unwrap();
    assert!(state.made_bucket("tb-a") && state.is_premade());
}

/// Catches a default run whose commands changed: without `--premade` the commands are exactly
/// those of an apply that makes everything, in order.
#[tokio::test]
async fn the_default_mode_runs_the_same_commands_as_before() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-default");
    let config = run_config(dir.path(), &stub.endpoint, None);
    run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Create)
        .await
        .unwrap();
    let policy = dir.path().join("policy-cold.json").display().to_string();
    let target = "tbops/tb-cold-test-bucket-mark";
    let name = "tb-tierstore-bench-cold";
    assert_eq!(
        mc_args(&host),
        [
            "ls tbops".to_owned(),
            "admin user list tbops".to_owned(),
            "ls tbops".to_owned(),
            format!("mb {target}"),
            format!("quota set {target} --size 30GiB"),
            format!("admin policy create tbops {name} {policy}"),
            "admin user list tbops".to_owned(),
            "admin user add tbops".to_owned(),
            format!("admin policy attach tbops {name} --user tb-KEYID-MARK-cold"),
        ]
    );
}

/// Catches a quota that is not a hard one being taken as the limit: a soft quota, one of an
/// unknown type, a differently cased one and an answer with no type each stop the verb before a
/// probe, and a hard one is accepted.
#[tokio::test]
async fn a_quota_that_is_not_hard_is_refused() {
    for (name, kind, refused) in [
        ("tb-prem-t-soft", Some("fifo"), true),
        ("tb-prem-t-odd", Some("other"), true),
        ("tb-prem-t-none", None, true),
        ("tb-prem-t-hard", Some("hard"), false),
        ("tb-prem-t-caps", Some("HARD"), true),
    ] {
        let line = quota_text(Some(1024), kind);
        let stub = Stub::start(farm()).await;
        let (dir, host, _c, ops) = base(name);
        host.answer(premade_store_saying(COLD, line));
        let config = run_config(dir.path(), &stub.endpoint, None);
        let got = run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade).await;
        if refused {
            let text = got.unwrap_err().to_string();
            assert!(text.contains("quota"), "{name}: {text}");
            assert!(stub.requests().is_empty(), "{name}");
            assert!(!dir.path().join(STATE_FILE).exists(), "{name}");
        } else {
            got.unwrap();
        }
    }
}

const MADE_RECORD: &str =
    r#"{"buckets": ["tb-cold-test-bucket-mark"], "accounts": ["tb-KEYID-MARK-cold"]}"#;
const PREMADE_RECORD: &str = r#"{"buckets": ["tb-cold-test-bucket-mark"], "accounts": ["tb-KEYID-MARK-cold"], "premade": true}"#;

/// Catches a premade run relabelling what an earlier run made: over a record with entries and no
/// `premade` marker, an apply and a dry run are each refused before any command or request, the
/// refusal names the result directory, and the record is as it was.
#[tokio::test]
async fn a_premade_run_over_a_created_record_is_refused() {
    for (name, mode) in [
        ("tb-prem-mix-apply", Mode::Apply),
        ("tb-prem-mix-dry", Mode::DryRun),
    ] {
        let stub = Stub::start(farm()).await;
        let (dir, host, _c, ops) = base(name);
        host.answer(premade_store(COLD, Some(QUOTA_30_GIB)));
        fs::write(dir.path().join(STATE_FILE), MADE_RECORD).unwrap();
        let config = run_config(dir.path(), &stub.endpoint, None);
        let text = run_with_supply(&host, &config, &ops, mode, Supply::Premade)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            text.contains(&dir.path().display().to_string()),
            "{name}: {text}"
        );
        assert!(text.contains("fresh result"), "{name}: {text}");
        assert!(host.ran().is_empty(), "{name}");
        assert!(stub.requests().is_empty(), "{name}");
        assert_eq!(
            fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            MADE_RECORD,
            "{name}"
        );
        assert!(!dir.path().join(STEPS_FILE).exists(), "{name}");
    }
}

/// Catches a created run adopting a premade record: an apply and a dry run over a record with the
/// `premade` marker are each refused before any command, naming the result directory.
#[tokio::test]
async fn a_created_run_over_a_premade_record_is_refused() {
    for (name, mode) in [
        ("tb-prem-rev-apply", Mode::Apply),
        ("tb-prem-rev-dry", Mode::DryRun),
    ] {
        let stub = Stub::start(farm()).await;
        let (dir, host, _c, ops) = base(name);
        fs::write(dir.path().join(STATE_FILE), PREMADE_RECORD).unwrap();
        let config = run_config(dir.path(), &stub.endpoint, None);
        let text = run_with_supply(&host, &config, &ops, mode, Supply::Create)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            text.contains(&dir.path().display().to_string()),
            "{name}: {text}"
        );
        assert!(text.contains("fresh result"), "{name}: {text}");
        assert!(host.ran().is_empty(), "{name}");
        assert!(stub.requests().is_empty(), "{name}");
        assert_eq!(
            fs::read_to_string(dir.path().join(STATE_FILE)).unwrap(),
            PREMADE_RECORD,
            "{name}"
        );
    }
}

/// Catches a re-run being refused: a premade apply over a premade record of the same prefix
/// proceeds and leaves a premade record.
#[tokio::test]
async fn a_premade_run_over_a_premade_record_proceeds() {
    let stub = Stub::start(farm()).await;
    let (dir, host, _c, ops) = base("tb-prem-rerun");
    host.answer(premade_store(COLD, Some(QUOTA_30_GIB)));
    fs::write(dir.path().join(STATE_FILE), PREMADE_RECORD).unwrap();
    let config = run_config(dir.path(), &stub.endpoint, None);
    run_with_supply(&host, &config, &ops, Mode::Apply, Supply::Premade)
        .await
        .unwrap();
    assert!(State::load(dir.path(), "tb-").unwrap().is_premade());
}
