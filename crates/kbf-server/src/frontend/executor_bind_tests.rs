//! `--executor js-agent` against the flags beside it, through the parse of a whole frontend's
//! flags: each disagreement is refused when the flags are read, naming them, and not on the
//! first run.

use super::super::{FrontendFlags, command, parse};
use super::*;

/// A frontend that runs placed actions on hosts, with everything it needs and nothing it can
/// do without. The tier store and the read listener are at IP addresses, the store `http://`
/// as it is today.
const BASE: [&str; 28] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://192.0.2.5:9300",
    "--pack-store-bucket=packs",
    "--pack-store-credentials-file=/nowhere",
    "--task-secret-file=/nowhere/secret",
    "--task-put-endpoint=http://192.0.2.5:9300",
    "--task-put-bucket=packs",
    "--task-put-credentials-file=/nowhere/presign.json",
    "--task-read-url=https://192.0.2.6:9400",
    "--job-listen=192.0.2.6:9400",
    "--job-tls-cert=/nowhere/job.pem",
    "--job-tls-key=/nowhere/job.key",
    "--job-grant-secret-file=/nowhere/secret",
    "--executor=js-agent",
    "--executor-lease-url=http://127.0.0.1:7",
    "--executor-lease-token-file=/etc/token",
    "--executor-frontend-id=front-1",
    "--executor-class=farm-linux",
    "--executor-hosts=2",
    "--executor-host-cpus=8",
    "--executor-host-memory-gib=32",
    "--executor-base-image=registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "--executor-kbf-image=registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    "--executor-insecure-argv-secrets",
    "--executor-allow-plain-http-put",
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

/// Catches a frontend that parses with the executor and without what it prepares runs with:
/// without any one of the packs, the task flags or the read listener it would start and
/// refuse every run (or hand out grants to a port nobody serves). Each is named.
#[test]
fn js_agent_names_what_it_prepares_its_runs_with() {
    let packs = [
        "--pack-store-endpoint",
        "--pack-store-bucket",
        "--pack-store-credentials-file",
        "--pack-index",
    ];
    assert!(refused(&packs, &[]).contains("needs --pack-store-endpoint"));
    let task = [
        "--task-secret-file",
        "--task-put-endpoint",
        "--task-put-bucket",
        "--task-put-credentials-file",
        "--task-read-url",
    ];
    assert!(refused(&task, &[]).contains("needs --task-secret-file"));
    let job = [
        "--job-listen",
        "--job-tls-cert",
        "--job-tls-key",
        "--job-grant-secret-file",
    ];
    assert!(refused(&job, &[]).contains("needs --job-listen"));
    assert!(parsed(argv(&[], &[])).is_ok(), "and with all of them");
}

/// Catches the checks applying when the executor is switched back: a unit that keeps every
/// setting of the host executor, and that disagrees with itself, still starts on daemons. The
/// rollback is the one flag.
#[test]
fn the_daemon_executor_is_not_checked_against_them() {
    let flags = parsed(argv(
        &["--executor"],
        &[
            "--executor=kbf-daemon",
            "--task-put-bucket=elsewhere",
            "--task-read-url=https://farm.test:9400",
        ],
    ))
    .expect("starts on daemons");
    assert!(flags.executor.js.is_none());
}

/// Catches a presign ceiling that is the 4 hours, with the preparation adding its slack to every
/// run's wall time: a longest run that fits 4 hours but not 4 hours less the slack passes the
/// executor's own check and fails every mint. The flags are refused naming the numbers, and the
/// ceiling the executor is built with is the 4 hours less the slack.
#[test]
fn the_longest_run_must_fit_the_presign_ceiling_less_the_slack() {
    // A slack of 10000 s leaves 4400 s: a longest run of 4100 s and its margin of 300 s fit.
    let slack = [
        "--task-url-slack-secs=10000",
        "--task-run-wall-secs=4000",
        "--executor-lease-ttl-secs=5000",
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
        why.contains("(4401 s)") && why.contains("--task-url-slack-secs (10000 s)"),
        "{why}"
    );
    // The same run fits with no slack.
    let mut no_slack = with("--executor-max-run-secs=4101");
    no_slack.retain(|flag| !flag.starts_with("--task-url-slack-secs"));
    no_slack.push("--task-url-slack-secs=0");
    parsed(argv(&[], &no_slack)).expect("fits with no slack");
}

/// Catches an `http://` tier store with no word from the operator: the task refuses an `http`
/// write unless told, so every run would fail at the task, late. The flag is also no more than
/// a warning when the store is `https`.
#[test]
fn a_plain_put_endpoint_needs_the_plain_http_opt_in() {
    let why = refused(&["--executor-allow-plain-http-put"], &[]);
    assert!(
        why.contains("--task-put-endpoint is http://")
            && why.contains("--executor-allow-plain-http-put"),
        "{why}"
    );
    let https = ["--task-put-endpoint=https://192.0.2.5:9300"];
    let without = parsed(argv(&["--executor-allow-plain-http-put"], &https));
    assert!(without.is_ok(), "an https store needs no opt-in");

    let mut flags = parsed(argv(&[], &https)).expect("parses");
    let js = flags.executor.js.as_mut().expect("js-agent");
    let beside = Beside {
        task: flags.task.as_ref(),
        job_read: flags.job_read.as_ref(),
        packs: flags.packs.as_ref(),
    };
    let warnings = check(js, &beside).expect("checks");
    assert!(
        warnings.iter().any(|w| w.contains("is not needed")),
        "{warnings:?}"
    );
}

/// Catches an `http://` read url accepted with no word from the operator: the grant's bearer
/// token would cross the job network in clear. It is refused when the flags are parsed, naming
/// the flag that allows it; `--executor-allow-plain-http-grant` accepts it, and is no more than a
/// warning when the read url is `https`.
#[test]
fn a_plain_read_url_needs_the_plain_http_grant_opt_in() {
    let http = ["--task-read-url=http://192.0.2.6:9400"];
    let plain_listener = ["--job-plain-http"];
    let tls_files = ["--job-tls-cert", "--job-tls-key"];
    let why = refused(&tls_files, &[http[0], plain_listener[0]]);
    assert!(
        why.contains("--task-read-url") && why.contains("--executor-allow-plain-http-grant"),
        "{why}"
    );
    // The hint says what passes the next check too: against a TLS listener the opt-in alone would
    // be refused at the scheme, so the hint asks for an https:// url (or the plain listener).
    let tls_why = refused(&[], &http);
    assert!(
        tls_why.contains("use an https:// url") && tls_why.contains("--job-plain-http"),
        "{tls_why}"
    );
    assert!(
        !why.contains("use an https:// url") && why.contains("--executor-allow-plain-http-grant"),
        "a plain listener is told the opt-in: {why}"
    );
    let mut with = http.to_vec();
    with.extend(["--executor-allow-plain-http-grant", plain_listener[0]]);
    let flags = parsed(argv(&tls_files, &with)).expect("accepted with the opt-in");
    assert_eq!(
        flags.task.as_ref().map(|t| t.read_url.as_str()),
        Some("http://192.0.2.6:9400")
    );
    let js = flags.executor.js.as_ref().expect("js-agent");
    assert!(js.config.allow_plain_http_grant);

    let mut https = parsed(argv(&[], &["--executor-allow-plain-http-grant"])).expect("parses");
    let js = https.executor.js.as_mut().expect("js-agent");
    let beside = Beside {
        task: https.task.as_ref(),
        job_read: https.job_read.as_ref(),
        packs: https.packs.as_ref(),
    };
    let warnings = check(js, &beside).expect("checks");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("--executor-allow-plain-http-grant") && w.contains("not needed")),
        "{warnings:?}"
    );
    let bare = parsed(argv(&[], &[])).expect("parses");
    assert!(
        !bare
            .executor
            .js
            .expect("js-agent")
            .config
            .allow_plain_http_grant
    );
}

/// Catches `--executor-locate` opening the plain-http read grant: the task asks `/locate` over
/// the same grant, so an `http://` read url is as refused with it as without it, naming the
/// flag that allows it, and accepted with that flag.
#[test]
fn locate_does_not_loosen_the_plain_read_url_rule() {
    let http = "--task-read-url=http://192.0.2.6:9400";
    let why = refused(
        &["--job-tls-cert", "--job-tls-key"],
        &[http, "--job-plain-http", "--executor-locate"],
    );
    assert!(why.contains("--executor-allow-plain-http-grant"), "{why}");
    let flags = parsed(argv(
        &["--job-tls-cert", "--job-tls-key"],
        &[
            http,
            "--job-plain-http",
            "--executor-locate",
            "--executor-allow-plain-http-grant",
        ],
    ))
    .expect("accepted with the opt-in");
    assert!(flags.executor.js.expect("js-agent").config.locate);
}

/// Catches a read url or a tier store given by name: the job network has no DNS, so the task
/// could not connect. An IPv6 address in brackets is an address.
#[test]
fn the_read_url_and_the_put_endpoint_are_ip_addresses() {
    let why = refused(&[], &["--task-read-url=https://farm.test:9400"]);
    assert!(
        why.contains("--task-read-url") && why.contains("not an IP address"),
        "{why}"
    );
    let why = refused(&[], &["--task-put-endpoint=http://tier.test:9300"]);
    assert!(
        why.contains("--task-put-endpoint") && why.contains("not an IP address"),
        "{why}"
    );
    parsed(argv(
        &[],
        &[
            "--task-read-url=https://[2001:db8::6]:9400",
            "--task-put-endpoint=http://[2001:db8::5]:9300",
        ],
    ))
    .expect("addresses");
}

/// Catches a task that writes its pack to one bucket while the frontend reads packs from
/// another: every pack would be missing at the ingest.
#[test]
fn the_task_writes_to_the_bucket_the_packs_are_read_from() {
    let why = refused(&[], &["--task-put-bucket=elsewhere"]);
    assert!(
        why.contains("--task-put-bucket \"elsewhere\"")
            && why.contains("--pack-store-bucket \"packs\""),
        "{why}"
    );
}

/// Catches a lease ttl past what the lease server grants going unsaid: the server clamps it, and
/// the executor plans its renewals by the ask. It is a warning, as a server may be started to
/// grant more.
#[test]
fn a_lease_ttl_past_the_servers_default_maximum_is_warned() {
    let mut flags = parsed(argv(&[], &[])).expect("parses");
    let js = flags.executor.js.as_mut().expect("js-agent");
    let beside = Beside {
        task: flags.task.as_ref(),
        job_read: flags.job_read.as_ref(),
        packs: flags.packs.as_ref(),
    };
    assert_eq!(check(js, &beside).expect("checks"), Vec::<String>::new());
    js.config.lease_ttl = Duration::from_secs(3601);
    let warnings = check(js, &beside).expect("checks");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("--executor-lease-ttl-secs 3601"),
        "{warnings:?}"
    );
}

/// Catches the host of a url being misread: names, ports, brackets and junk.
#[test]
fn the_host_of_a_url_is_an_address_or_nothing() {
    for (url, ip) in [
        ("https://192.0.2.6:9400", Some("192.0.2.6")),
        ("http://192.0.2.5", Some("192.0.2.5")),
        ("https://[::1]:9400", Some("::1")),
        ("https://farm.test:9400", None),
        ("https://192.0.2.6.example:9400", None),
        ("not a url", None),
        ("", None),
    ] {
        assert_eq!(
            host_ip(url),
            ip.map(|i| i.parse().expect("an address")),
            "{url}"
        );
    }
}

/// Catches a tier store whose scheme is not `http://` or `https://` riding in on the plain
/// opt-in (or `HTTPS://` being taken for plain): only those two schemes are a store.
#[test]
fn a_put_endpoint_is_http_or_https_exactly() {
    for endpoint in ["ftp://192.0.2.1:21", "HTTPS://192.0.2.5:9300"] {
        let why = refused(&[], &[&format!("--task-put-endpoint={endpoint}")]);
        assert!(
            why.contains("--task-put-endpoint must start with https://"),
            "{endpoint}: {why}"
        );
    }
}

/// Catches a read url whose scheme is not the one the job listener serves, which would otherwise
/// fail every run's first read: an `http://` url against a TLS listener sends plain HTTP to a TLS
/// port, and an `https://` url against a plain listener starts a handshake nobody answers. Each is
/// refused when the flags are parsed, naming both flags; the matching pairs start.
#[test]
fn the_read_url_scheme_is_the_one_the_job_listener_serves() {
    let http = "--task-read-url=http://192.0.2.6:9400";
    let https = "--task-read-url=https://192.0.2.6:9400";
    let allow = "--executor-allow-plain-http-grant";
    let tls_files = ["--job-tls-cert", "--job-tls-key"];
    let plain = "--job-plain-http";

    let why = refused(&[], &[http, allow]);
    assert!(
        why.contains("--task-read-url is http://") && why.contains("--job-plain-http"),
        "{why}"
    );
    let why = refused(&tls_files, &[https, plain]);
    assert!(
        why.contains("--task-read-url is https://") && why.contains("--job-plain-http"),
        "{why}"
    );
    assert!(parsed(argv(&[], &[https])).is_ok(), "https against TLS");
    assert!(
        parsed(argv(&tls_files, &[http, allow, plain])).is_ok(),
        "http against plain"
    );
}

/// Pins that `--job-plain-http` is accepted beside `--executor kbf-daemon` (and with no executor,
/// which the flag tests of the listener parse): the listener is independent of the executor, and
/// the daemon executor takes no read grant, so the plain listener leaks nothing it would not
/// serve anyway. The listener's plain mode stays what the flag says.
#[test]
fn a_plain_job_listener_is_accepted_beside_the_daemon_executor() {
    let flags = parsed(argv(
        &["--executor", "--job-tls-cert", "--job-tls-key"],
        &["--executor=kbf-daemon", "--job-plain-http"],
    ))
    .expect("accepted");
    assert!(flags.executor.js.is_none());
    assert!(flags.job_read.expect("the listener").tls.is_none());
}
