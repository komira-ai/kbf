use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::super::{FrontendFlags, command, parse};
use super::*;
use crate::test_dir::scratch_dir;

/// The flags of a frontend with the packs on, which `--executor js-agent` needs, and `more`.
fn argv(more: &[&str]) -> Vec<String> {
    [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--state-store=postgres:///kbf?host=/run/postgresql",
        "--pack-index",
        "--pack-store-endpoint=http://127.0.0.1:9",
        "--pack-store-bucket=packs",
        "--pack-store-credentials-file=/nowhere",
        // What `--executor js-agent` prepares its runs with, which the parse requires.
        "--task-secret-file=/nowhere/secret",
        "--task-put-endpoint=http://127.0.0.1:9",
        "--task-put-bucket=packs",
        "--task-put-credentials-file=/nowhere/presign.json",
        "--task-read-url=https://127.0.0.1:9400",
        "--job-listen=127.0.0.1:9400",
        "--job-tls-cert=/nowhere/job.pem",
        "--job-tls-key=/nowhere/job.key",
        "--job-grant-secret-file=/nowhere/secret",
    ]
    .iter()
    .filter(|flag| !more.iter().any(|m| name(m) == name(flag)))
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

/// The flags `--executor js-agent` needs, and nothing it can do without.
const BASE: &str =
    "registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const KBF: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

const JS: [&str; 12] = [
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
    // The tier store of the task flags is `http://`, as it is until it serves TLS.
    "--executor-allow-plain-http-put",
];

/// The name of a flag given as `--name=value`.
fn name(flag: &str) -> &str {
    flag.split('=').next().expect("a flag")
}

/// [`JS`] with `more` given, a flag of `more` taking the place of the one of that name.
fn js_with(more: &[&str]) -> Result<FrontendFlags, String> {
    let mut all: Vec<&str> = JS
        .iter()
        .copied()
        .filter(|flag| !more.iter().any(|m| name(m) == name(flag)))
        .collect();
    all.extend(more);
    parsed(argv(&all))
}

fn refused(more: &[&str]) -> String {
    js_with(more).expect_err("refused")
}

/// Catches a default that builds an executor: a frontend given no `--executor` runs placed
/// actions on daemons, as it always has, and has no settings of the executor.
#[test]
fn a_frontend_with_no_executor_flag_runs_on_daemons_and_holds_no_settings() {
    let flags = parsed(argv(&[])).expect("flags");
    assert_eq!(flags.executor.kind, ExecutorKind::KbfDaemon);
    assert!(flags.executor.js.is_none());
}

/// Catches the rollback losing its settings or tripping on them: with `--executor kbf-daemon`
/// (or none) every setting of the lease server may stay in the unit or `kbf.toml`, the one that
/// lets secrets travel as arguments too, and none is read. Flipping the one flag is the
/// rollback: a frontend that refused to start over a setting nothing reads would need a second
/// edit in an incident.
#[test]
fn the_daemon_executor_keeps_the_lease_servers_settings_unread() {
    // The warning is made (its arguments are only evaluated under a subscriber).
    let _logged =
        tracing::subscriber::set_default(tracing_subscriber::fmt().with_test_writer().finish());
    for executor in ["--executor=kbf-daemon", ""] {
        let mut all: Vec<&str> = JS.to_vec();
        if executor.is_empty() {
            all.remove(0);
        } else {
            all[0] = executor;
        }
        assert!(all.contains(&"--executor-insecure-argv-secrets"));
        // A tag, which the js-agent parse refuses: the daemon executor must never read it.
        all.retain(|flag| !flag.starts_with("--executor-base-image"));
        all.push("--executor-base-image=registry.test/base:22.04");
        let flags = parsed(argv(&all)).expect("flags");
        assert_eq!(flags.executor.kind, ExecutorKind::KbfDaemon);
        assert!(flags.executor.js.is_none());
    }
}

/// Catches an executor started with what it cannot do without left out: each missing flag is
/// named, and all of them at once.
#[test]
fn js_agent_names_each_flag_it_needs() {
    let why = parsed(argv(&["--executor=js-agent"])).expect_err("refused");
    for flag in [
        "--executor-lease-url",
        "--executor-lease-token-file",
        "--executor-frontend-id",
        "--executor-class",
        "--executor-hosts",
        "--executor-host-cpus",
        "--executor-host-memory-gib",
        "--executor-base-image",
        "--executor-kbf-image",
    ] {
        assert!(why.contains(flag), "{flag} in {why}");
    }
    for (at, flag) in JS.iter().enumerate().skip(1).take(9) {
        let name = name(flag);
        let mut all = JS.to_vec();
        all.remove(at);
        let why = parsed(argv(&all)).expect_err("refused");
        let first = why.lines().next().expect("a line");
        assert!(first.ends_with(&format!("needs {name}")), "{name} in {why}");
    }
}

/// Catches `js-agent` running without having been told that the secrets travel as arguments,
/// and without the packs the ingest reads.
#[test]
fn js_agent_needs_the_secrets_opt_in_and_the_packs() {
    let mut all = JS.to_vec();
    all.retain(|flag| *flag != "--executor-insecure-argv-secrets");
    let why = parsed(argv(&all)).expect_err("refused");
    assert!(why.contains("--executor-insecure-argv-secrets"), "{why}");

    let mut bare: Vec<String> = argv(&JS)
        .into_iter()
        .filter(|flag| !flag.contains("pack-store") && flag != "--pack-index")
        .collect();
    bare.retain(|flag| !flag.starts_with("--state-store"));
    let why = parsed(bare).expect_err("refused");
    assert!(why.contains("--pack-store-endpoint"), "{why}");
}

/// Catches settings that do not become the executor's: the shape is in cores and GiB, the two
/// images are the ones given by digest, and everything else is what was given.
#[test]
fn js_agent_settings_become_the_executors_config() {
    let flags = js_with(&[
        "--executor-host-slots=4",
        "--executor-arch=arm64",
        "--executor-work-dir=/scratch",
        "--executor-allow-plain-http-put",
        "--executor-allow-plain-http-grant",
        "--executor-run-margin-secs=120",
        "--executor-lease-ttl-secs=7200",
        "--executor-max-run-secs=1800",
        "--executor-default-timeout-secs=900",
        "--executor-call-timeout-secs=3",
        "--executor-output-disk-bytes=4294967296",
    ])
    .expect("flags");
    let js = flags.executor.js.expect("js-agent");
    let config = &js.config;
    assert_eq!(flags.executor.kind, ExecutorKind::JsAgent);
    assert_eq!(js.lease_url, "http://127.0.0.1:7");
    assert_eq!(js.token_file, Path::new("/etc/token"));
    assert!(js.ca_file.is_none());
    assert_eq!(js.base_image.to_string(), BASE);
    assert_eq!(js.kbf_image.to_string(), KBF);
    assert_eq!(config.frontend, "front-1");
    assert_eq!(config.class, "farm-linux");
    assert_eq!(config.hosts, 2);
    assert_eq!(config.cpu_millis, 8_000);
    assert_eq!(config.memory_bytes, 32 << 30);
    assert_eq!(config.slots, 4);
    assert_eq!(config.arch, "arm64");
    assert_eq!(config.work_dir.as_deref(), Some("/scratch"));
    assert!(config.allow_plain_http_put);
    assert!(config.allow_plain_http_grant);
    assert!(config.insecure_argv_secrets);
    assert_eq!(config.run_margin, Duration::from_secs(120));
    assert_eq!(config.lease_ttl, Duration::from_secs(7200));
    assert_eq!(config.max_run, Duration::from_secs(1800));
    assert_eq!(config.default_timeout, Duration::from_secs(900));
    assert_eq!(config.call_timeout, Duration::from_secs(3));
    assert_eq!(config.output_disk_bytes, 4 << 30);
}

/// Catches `--executor-locate` not becoming the executor's setting, or being on by default: it
/// is off unless given, and given with `--executor js-agent` it is the config's `locate`.
#[test]
fn executor_locate_is_off_unless_given_and_becomes_the_config() {
    let off = js_with(&[]).expect("flags");
    assert!(!off.executor.js.expect("js-agent").config.locate);
    let on = js_with(&["--executor-locate"]).expect("flags");
    assert!(on.executor.js.expect("js-agent").config.locate);
}

/// Catches `--executor-locate` accepted where nothing reads it: daemons build no task command
/// line, so with `--executor kbf-daemon` (the default) it would do nothing and is refused when
/// the flags are read, naming both flags.
#[test]
fn locate_with_kbf_daemon_is_refused_at_start() {
    let why = parsed(argv(&["--executor-locate"])).expect_err("refused");
    assert!(why.contains("--executor-locate"), "{why}");
    assert!(why.contains("--executor js-agent"), "{why}");
    parsed(argv(&[])).expect("daemons without the flag still parse");
}

/// Catches a flag default that has drifted from the executor's own: the defaults are written
/// as text in the flags and are the config's.
#[test]
fn the_defaults_are_the_executors_own() {
    let mut all: Vec<&str> = JS
        .iter()
        .copied()
        .filter(|flag| *flag != "--executor-allow-plain-http-put")
        .collect();
    all.push("--task-put-endpoint=https://127.0.0.1:9");
    let flags = parsed(argv(&all)).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    let own = Config::new("front-1", 2, "farm-linux", 8_000, 32 << 30);
    assert_eq!(config.slots, own.slots);
    assert_eq!(config.arch, own.arch);
    assert_eq!(config.work_dir, own.work_dir);
    assert_eq!(config.allow_plain_http_put, own.allow_plain_http_put);
    assert_eq!(config.allow_plain_http_grant, own.allow_plain_http_grant);
    assert!(!config.allow_plain_http_grant, "off by default");
    assert_eq!(config.run_margin, own.run_margin);
    assert_eq!(config.lease_ttl, own.lease_ttl);
    assert_eq!(config.max_run, own.max_run);
    assert_eq!(config.default_timeout, own.default_timeout);
    assert_eq!(config.call_timeout, own.call_timeout);
    assert_eq!(config.output_disk_bytes, own.output_disk_bytes);
    assert_eq!(own.delivery, config.delivery);
}

/// Catches an image that is taken when it is not pinned by digest, or dropped when it is: for
/// each of the base and the kbf image the scheme is optional, a tag, a bare name and a short
/// digest are refused naming the flag, and so is leaving the flag out (there is no default
/// image: an executor with none could not tell what an action's image is served as).
#[test]
fn the_base_and_kbf_images_are_pinned_by_digest() {
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    // Neither image the other flag of `JS` names.
    let fresh = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    for flag in ["--executor-base-image", "--executor-kbf-image"] {
        let own = format!("registry.test/image@sha256:{fresh}");
        for given in [own.clone(), format!("docker://{own}")] {
            let flags = js_with(&[&format!("{flag}={given}")]).expect("flags");
            let js = flags.executor.js.expect("js-agent");
            let image = if flag.contains("base") {
                js.base_image
            } else {
                js.kbf_image
            };
            assert_eq!(image.to_string(), own, "{given}");
        }
        for tag in [
            "registry.test/base:22.04",
            &format!("registry.test/base:22.04@sha256:{digest}"),
            "registry.test/base",
            "registry.test/base@sha256:abc",
            "",
        ] {
            let why = refused(&[&format!("{flag}={tag}")]);
            assert!(why.contains(flag), "{tag}: {why}");
        }
    }
}

/// Catches a kbf image that is the base itself being taken: it would have no task tool as its
/// entrypoint, and every run would execute the action's first argument as the tool. The same
/// digest under another repository name is the same image.
#[test]
fn the_kbf_image_must_be_another_image_than_the_base() {
    let why = refused(&[&format!("--executor-kbf-image={BASE}")]);
    assert!(
        why.contains("--executor-kbf-image") && why.contains(BASE),
        "{why}"
    );
    let digest = BASE.split_once('@').expect("a digest").1;
    let why = refused(&[&format!("--executor-kbf-image=mirror.test/base@{digest}")]);
    assert!(why.contains("--executor-kbf-image"), "{why}");
}

/// Catches the retired flags being accepted: the task tool is the kbf image's entrypoint, so
/// there is no path to give, no certificate mounted beside it, no default image, and no
/// entrypoint to declare absent.
#[test]
fn the_flags_of_the_host_preloaded_tool_are_gone() {
    for retired in [
        "--executor-task-tool=/mnt/kbf-tools/kbf-task",
        "--executor-root-der=farm-ca.der",
        "--executor-default-image=registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "--executor-images-have-no-entrypoint",
    ] {
        let why = refused(&[retired]);
        assert!(why.contains("unexpected argument"), "{retired}: {why}");
    }
}

/// Catches a lease server or a directory that is not one being taken:
/// each is refused at the flags, naming the flag.
#[test]
fn settings_that_are_not_what_they_name_are_refused_naming_the_flag() {
    let cases: [(&[&str], &str); 6] = [
        (&["--executor-lease-url=ftp://x"], "--executor-lease-url"),
        (
            &["--executor-lease-url=http://lease.example:80"],
            "--executor-lease-url",
        ),
        (
            &["--executor-lease-url=https://lease.example"],
            "--executor-lease-ca-file",
        ),
        (
            &["--executor-lease-ca-file=/etc/ca.pem"],
            "--executor-lease-ca-file",
        ),
        (&["--executor-work-dir=scratch"], "--executor-work-dir"),
        (
            &["--executor-output-disk-bytes=0"],
            "--executor-output-disk-bytes",
        ),
    ];
    for (more, flag) in cases {
        let why = refused(more);
        assert!(why.contains(flag), "{more:?}: {why}");
    }
}

/// Catches settings that break a rule the executor's safety rests on starting anyway: the
/// executor's own validation is run on the flags, and a frontend id that cannot label a lease
/// is refused.
#[test]
fn settings_that_break_a_rule_of_the_executor_are_refused() {
    for more in [
        &["--executor-lease-ttl-secs=10"][..],
        &["--executor-default-timeout-secs=4000"],
        &["--executor-call-timeout-secs=60"],
        &["--executor-max-run-secs=20000"],
    ] {
        let why = refused(more);
        assert!(why.contains("--executor js-agent: "), "{more:?}: {why}");
    }
    let mut all = JS.to_vec();
    all[3] = "--executor-frontend-id=front 1";
    let why = parsed(argv(&all)).expect_err("refused");
    assert!(why.contains("frontend id"), "{why}");
}

fn flags_of(url: &str, token: &Path, ca: Option<&Path>) -> JsAgentFlags {
    let config = Config::new("front-1", 1, "farm-linux", 2_000, 16 << 30);
    JsAgentFlags {
        config,
        lease_url: url.to_owned(),
        token_file: token.to_path_buf(),
        ca_file: ca.map(Path::to_path_buf),
        base_image: ImageRef::parse(&format!("docker://{BASE}")).expect("an image"),
        kbf_image: ImageRef::parse(&format!("docker://{KBF}")).expect("an image"),
    }
}

fn write_private(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).expect("written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

/// Catches a client built from files it should have refused: a token file that others can read
/// or that is missing, a CA file that is missing or holds no certificate, and a plain URL with a
/// CA. A good token and a plain loopback URL give a client.
#[test]
fn the_client_is_built_from_a_private_token_and_a_ca_that_is_a_certificate() {
    let dir = scratch_dir("executor-client");
    let token = dir.path().join("token");
    write_private(&token, "a-token-of-printable-bytes-0123456789\n", 0o600);
    assert!(
        flags_of("http://127.0.0.1:7", &token, None)
            .client()
            .is_ok()
    );

    let open = dir.path().join("open");
    write_private(&open, "a-token-of-printable-bytes-0123456789\n", 0o644);
    let why = flags_of("http://127.0.0.1:7", &open, None)
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-lease-token-file"), "{why}");
    let absent = dir.path().join("absent");
    assert!(
        flags_of("http://127.0.0.1:7", &absent, None)
            .client()
            .is_err()
    );

    let ca = dir.path().join("ca.pem");
    write_private(&ca, "not a certificate", 0o600);
    let why = flags_of("https://localhost:7", &token, Some(&ca))
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-lease-url"), "{why}");
    let why = flags_of("https://localhost:7", &token, Some(&absent))
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-lease-ca-file"), "{why}");
    let why = flags_of("http://127.0.0.1:7", &token, Some(&ca))
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-lease-url"), "{why}");
}

/// [`JS`] without the insecure flag, and with `more`.
fn js_without_insecure(more: &[&str]) -> Result<FrontendFlags, String> {
    let mut all: Vec<&str> = JS
        .iter()
        .copied()
        .filter(|flag| *flag != "--executor-insecure-argv-secrets")
        .collect();
    all.extend(more);
    parsed(argv(&all))
}

fn delivery_of(flags: FrontendFlags) -> crate::executor::Delivery {
    flags.executor.js.expect("js-agent").config.delivery
}

/// Catches a default that is not today's: with no `--executor-secret-delivery` the secrets
/// travel as arguments, and the insecure flag is still what allows it.
#[test]
fn argv_is_the_default_delivery_and_still_needs_the_insecure_flag() {
    let flags = js_with(&[]).expect("flags");
    assert_eq!(delivery_of(flags), crate::executor::Delivery::InsecureArgv);
    let flags = js_with(&["--executor-secret-delivery=argv"]).expect("flags");
    assert_eq!(delivery_of(flags), crate::executor::Delivery::InsecureArgv);
    let why = js_without_insecure(&["--executor-secret-delivery=argv"]).expect_err("refused");
    assert!(why.contains("--executor-insecure-argv-secrets"), "{why}");
}

/// Catches `files` demanding the opt-in that exists for secrets in arguments: it parses
/// without the insecure flag and holds the delivery and nothing in `insecure_argv_secrets`.
#[test]
fn files_mode_needs_no_insecure_flag() {
    let flags = js_without_insecure(&["--executor-secret-delivery=files"]).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    assert_eq!(config.delivery, crate::executor::Delivery::Files);
    assert!(!config.insecure_argv_secrets);
}

/// Catches the contradiction being accepted: the operator says the secrets go as files and also
/// that they may go as arguments.
#[test]
fn files_mode_refuses_the_insecure_flag() {
    let why = refused(&["--executor-secret-delivery=files"]);
    assert!(why.contains("--executor-secret-delivery"), "{why}");
    assert!(why.contains("--executor-insecure-argv-secrets"), "{why}");
}

/// Catches a value that is neither delivery being read as one of them.
#[test]
fn an_unknown_delivery_is_refused() {
    let why = js_without_insecure(&["--executor-secret-delivery=env"]).expect_err("refused");
    assert!(why.contains("--executor-secret-delivery"), "{why}");
}

/// Catches a delivery that would do nothing being accepted: with `--executor kbf-daemon` no
/// task command line is built, so `files` is refused, naming the flag; the default `argv`
/// stays accepted (it is what every unit that names nothing has).
#[test]
fn the_daemon_executor_refuses_the_files_delivery() {
    let why = parsed(argv(&["--executor-secret-delivery=files"])).expect_err("refused");
    assert!(why.contains("--executor-secret-delivery"), "{why}");
    let flags = parsed(argv(&["--executor-secret-delivery=argv"])).expect("flags");
    assert_eq!(flags.executor.kind, ExecutorKind::KbfDaemon);
}
