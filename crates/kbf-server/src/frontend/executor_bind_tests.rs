//! `--executor js-agent` against the flags beside it, through the parse of a whole frontend's
//! flags: each disagreement is refused when the flags are read, naming them, and not on the
//! first run.

use std::time::Duration;

use super::super::{FrontendFlags, command, parse};
use crate::executor::{PutDirRoots, TaskOut};

/// A frontend that runs placed actions on hosts, with everything it needs and nothing it can
/// do without: the packs, and the put directories of the runs.
const BASE: [&str; 20] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://192.0.2.5:9300",
    "--pack-store-bucket=packs",
    "--pack-store-credentials-file=/nowhere",
    "--task-secret-file=/nowhere/secret",
    "--task-out-root=/mnt/ts/put",
    "--executor=js-agent",
    "--executor-job-url=http://127.0.0.1:7",
    "--executor-job-token-file=/etc/token",
    "--executor-frontend-id=front-1",
    "--executor-base-image=registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "--executor-kbf-image=registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    "--executor-base-tree=/nowhere/base-tree",
    "--executor-base-tree-url=",
    "--executor-insecure-argv-secrets",
    "--executor-host-cpus=8",
    "--executor-host-memory-gib=32",
];

fn flag_name(flag: &str) -> &str {
    flag.split('=').next().expect("a flag")
}

/// [`BASE`] without the flags `without` names, and with `more`; a flag of `more` replaces the
/// one of that name.
fn argv(without: &[&str], more: &[&str]) -> Vec<String> {
    BASE.iter()
        .filter(|flag| {
            let name = flag_name(flag);
            !without.contains(&name) && !more.iter().any(|m| flag_name(m) == name)
        })
        .chain(more)
        .map(ToString::to_string)
        .collect()
}

fn parsed(argv: Vec<String>) -> Result<FrontendFlags, String> {
    let matches = command()
        .try_get_matches_from(argv)
        .map_err(|e| e.to_string())?;
    parse(&matches).map_err(|e| e.to_string())
}

fn refused(without: &[&str], more: &[&str]) -> String {
    parsed(argv(without, more)).expect_err("refused")
}

/// Catches a frontend that parses with the executor and without what it prepares its runs with:
/// without the packs or the task flags it would start and refuse every run. Each is named.
#[test]
fn js_agent_names_what_it_prepares_its_runs_with() {
    let packs = [
        "--pack-store-endpoint",
        "--pack-store-bucket",
        "--pack-store-credentials-file",
        "--pack-index",
    ];
    assert!(refused(&packs, &[]).contains("needs --pack-store-endpoint"));
    let task = ["--task-secret-file", "--task-out-root"];
    assert!(refused(&task, &[]).contains("needs --task-secret-file"));
    assert!(parsed(argv(&[], &[])).is_ok(), "and with all of them");
}

/// Catches the checks applying with no executor: a unit that keeps every setting of the host
/// executor, and that has dropped what it needs, still starts with none.
#[test]
fn no_executor_is_not_checked_against_them() {
    let flags = parsed(argv(
        &[
            "--executor",
            "--pack-store-endpoint",
            "--pack-store-bucket",
            "--pack-store-credentials-file",
            "--pack-index",
            "--task-secret-file",
            "--task-out-root",
        ],
        &[],
    ))
    .expect("starts with no executor");
    assert!(flags.executor.js.is_none());
}

/// Catches a ceiling that is the 4 hours, with the preparation adding its slack to every run's
/// wall time: a longest run that fits 4 hours but not 4 hours less the slack passes the
/// executor's own check and fails every mint. The flags are refused naming the numbers, and the
/// ceiling the executor is built with is the 4 hours less the slack.
#[test]
fn the_longest_run_must_fit_the_keys_ceiling_less_the_slack() {
    // A slack of 10000 s leaves 4400 s: a longest run of 4100 s and its margin of 300 s fit.
    let slack = [
        "--task-url-slack-secs=10000",
        "--executor-default-timeout-secs=3000",
    ];
    let with = |max_run: &'static str| {
        let mut more = slack.to_vec();
        more.push(max_run);
        more
    };
    let flags = parsed(argv(&[], &with("--executor-max-run-secs=4100"))).expect("fits");
    let config = flags.executor.js.expect("js-agent").config;
    assert_eq!(config.presign_ceiling, Duration::from_secs(4_400));
    // One second more does not.
    let why = refused(&[], &with("--executor-max-run-secs=4101"));
    assert!(
        why.contains("--executor js-agent: --executor-max-run-secs plus")
            && why.contains("(4401 s)")
            && why.contains("--task-url-slack-secs (10000 s)"),
        "{why}"
    );
    // The same run fits with no slack.
    let mut no_slack = with("--executor-max-run-secs=4101");
    no_slack.retain(|flag| !flag.starts_with("--task-url-slack-secs"));
    no_slack.push("--task-url-slack-secs=0");
    parsed(argv(&[], &no_slack)).expect("fits with no slack");
}

/// Pins that `--job-plain-http` is accepted with no executor: the listener is independent of the
/// executor, and with none no read grant is given, so the plain listener leaks nothing it would
/// not serve anyway. The listener's plain mode stays what the flag says.
#[test]
fn a_plain_job_listener_is_accepted_with_no_executor() {
    let flags = parsed(argv(
        &["--executor"],
        &[
            "--job-listen=192.0.2.6:9400",
            "--job-plain-http",
            "--job-grant-secret-file=/nowhere/secret",
        ],
    ))
    .expect("accepted");
    assert!(flags.executor.js.is_none());
    assert!(flags.job_read.expect("the listener").tls.is_none());
}

/// Asserts that the task flags reach the executor's settings: the put directory under
/// `--task-out-root`, the limits of an object, and a ceiling of the default slack less than the 4
/// hours a directory's keys may be good for. The frontend flags say they are for put directories,
/// and no read listener is needed.
#[test]
fn the_task_flags_are_the_executors_settings() {
    let flags = parsed(argv(&[], &[])).expect("accepted");
    assert_eq!(flags.job_read.map(|_| ()), None);
    let put_dir = flags.put_dir.expect("put-dir flags");
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    assert_eq!(put_dir.roots, roots);
    assert_eq!(put_dir.url_slack, Duration::from_secs(600));
    let js = flags.executor.js.expect("js-agent");
    let config = js.config;
    assert_eq!(js.put_retry, Duration::from_secs(60));
    assert_eq!(config.task_out, TaskOut::PutDir(roots));
    assert_eq!(config.max_object_entries, put_dir.max_object_entries);
    assert_eq!(
        config.max_object_manifest_bytes,
        put_dir.max_object_manifest_bytes
    );
    assert_eq!(
        config.object_bytes_per_entry,
        put_dir.object_bytes_per_entry
    );
    assert_eq!(config.presign_ceiling, Duration::from_secs(14_400 - 600));
}

/// Asserts that `--task-put-retry-secs` reaches the executor's flags and is 60 s by default.
/// Catches: a flag that parses and is dropped, leaving the writer of action trees at its own
/// default.
#[test]
fn the_put_retry_total_reaches_the_executor() {
    let flags = parsed(argv(&[], &["--task-put-retry-secs=9"])).expect("accepted");
    assert_eq!(
        flags.executor.js.expect("js-agent").put_retry,
        Duration::from_secs(9)
    );
    let flags = parsed(argv(&[], &[])).expect("accepted");
    assert_eq!(
        flags.executor.js.expect("js-agent").put_retry,
        Duration::from_secs(60)
    );
}
