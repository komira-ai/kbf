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
        "--task-out-root=/mnt/ts/put",
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

const JS: [&str; 10] = [
    "--executor=js-agent",
    "--executor-job-url=http://127.0.0.1:7",
    "--executor-job-token-file=/etc/token",
    "--executor-frontend-id=front-1",
    "--executor-hosts=2",
    "--executor-host-cpus=8",
    "--executor-host-memory-gib=32",
    "--executor-base-image=registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "--executor-kbf-image=registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
    "--executor-base-tree=/opt/kbf/base-tree",
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

/// Catches a default that builds an executor: a frontend given no `--executor` has no settings
/// of the executor.
#[test]
fn a_frontend_with_no_executor_flag_holds_no_settings() {
    let flags = parsed(argv(&[])).expect("flags");
    assert!(flags.executor.js.is_none());
}

/// Catches the deleted worker executor still being accepted, or an unknown one: `--executor
/// kbf-daemon` is refused at start with a message that says kbf-daemon was deleted and names
/// `--executor js-agent`, another value is refused naming the flag and the only executor, and
/// `js-agent` still parses.
#[test]
fn the_deleted_daemon_executor_and_unknown_ones_are_refused_naming_why() {
    let why = parsed(argv(&["--executor=kbf-daemon"])).expect_err("refused");
    assert!(
        why.contains(
            "--executor kbf-daemon is gone: kbf-daemon was deleted, and placed actions run as \
             jobs of the job service, with --executor js-agent"
        ),
        "{why}"
    );
    let why = parsed(argv(&["--executor=lease"])).expect_err("refused");
    assert!(
        why.contains("--executor lease: the only executor is js-agent"),
        "{why}"
    );
    js_with(&[]).expect("js-agent parses");
}

/// Catches settings tripping a frontend with no executor: with no `--executor` every setting of
/// the job service may stay in `kbf.toml`, and none is read.
#[test]
fn no_executor_keeps_the_job_services_settings_unread() {
    let mut all: Vec<&str> = JS.to_vec();
    all.remove(0);
    // A tag, which the js-agent parse refuses: with no executor it must never be read.
    all.retain(|flag| !flag.starts_with("--executor-base-image"));
    all.push("--executor-base-image=registry.test/base:22.04");
    let flags = parsed(argv(&all)).expect("flags");
    assert!(flags.executor.js.is_none());
}

/// Catches an executor started with what it cannot do without left out: each missing flag is
/// named, and all of them at once.
#[test]
fn js_agent_names_each_flag_it_needs() {
    let why = parsed(argv(&["--executor=js-agent"])).expect_err("refused");
    for flag in [
        "--executor-job-url",
        "--executor-job-token-file",
        "--executor-frontend-id",
        "--executor-base-image",
        "--executor-kbf-image",
        "--executor-base-tree",
    ] {
        assert!(why.contains(flag), "{flag} in {why}");
    }
    for flag in [
        "--executor-hosts",
        "--executor-host-cpus",
        "--executor-host-memory-gib",
    ] {
        assert!(!why.contains(flag), "{flag} is not needed: {why}");
    }
    let shape = [
        "--executor-hosts",
        "--executor-host-cpus",
        "--executor-host-memory-gib",
    ];
    for (at, flag) in JS.iter().enumerate().skip(1).take(9) {
        if shape.contains(&name(flag)) {
            continue;
        }
        let name = name(flag);
        let mut all = JS.to_vec();
        all.remove(at);
        let why = parsed(argv(&all)).expect_err("refused");
        let first = why.lines().next().expect("a line");
        assert!(first.ends_with(&format!("needs {name}")), "{name} in {why}");
    }
}

/// Catches `js-agent` running without the packs the ingest reads.
#[test]
fn js_agent_needs_the_packs() {
    let mut bare: Vec<String> = argv(&JS)
        .into_iter()
        .filter(|flag| !flag.contains("pack-store") && flag != "--pack-index")
        .collect();
    bare.retain(|flag| !flag.starts_with("--state-store"));
    let why = parsed(bare).expect_err("refused");
    assert!(why.contains("--pack-store-endpoint"), "{why}");
}

/// Catches the virtual workers' shape being required: left out, the executor gets the
/// ceilings the flags default to (the constants), and giving them lowers each ceiling.
#[test]
fn the_workers_shape_defaults_to_generous_ceilings() {
    let mut all = JS.to_vec();
    all.retain(|flag| {
        ![
            "--executor-hosts",
            "--executor-host-cpus",
            "--executor-host-memory-gib",
        ]
        .contains(&name(flag))
    });
    let flags = parsed(argv(&all)).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    assert_eq!(config.hosts, DEFAULT_HOSTS);
    assert_eq!(config.cpu_millis, DEFAULT_HOST_CPUS * 1_000);
    assert_eq!(config.memory_bytes, DEFAULT_HOST_MEMORY_GIB << 30);
    let flags = js_with(&[]).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    assert_eq!(
        (config.hosts, config.cpu_millis, config.memory_bytes),
        (2, 8_000, 32 << 30),
        "a given shape lowers the ceilings"
    );
}

/// Catches settings that do not become the executor's: the shape is in cores and GiB, the two
/// images are the ones given by digest, and everything else is what was given.
#[test]
fn js_agent_settings_become_the_executors_config() {
    let flags = js_with(&[
        "--executor-host-slots=4",
        "--executor-arch=arm64",
        "--executor-run-margin-secs=120",
        "--executor-max-run-secs=1800",
        "--executor-default-timeout-secs=900",
        "--executor-call-timeout-secs=3",
        "--executor-poll-ms=250",
        "--executor-prepare-limit-secs=45",
        "--executor-prepare-concurrency=3",
        "--executor-failed-stderr-log-bytes=2048",
    ])
    .expect("flags");
    let js = flags.executor.js.expect("js-agent");
    let config = &js.config;
    assert_eq!(js.job_url, "http://127.0.0.1:7");
    assert_eq!(js.token_file, Path::new("/etc/token"));
    assert!(js.ca_file.is_none());
    assert_eq!(js.base_image.to_string(), BASE);
    assert_eq!(js.kbf_image.to_string(), KBF);
    assert_eq!(config.frontend, "front-1");
    assert_eq!(config.hosts, 2);
    assert_eq!(config.cpu_millis, 8_000);
    assert_eq!(config.memory_bytes, 32 << 30);
    assert_eq!(config.slots, 4);
    assert_eq!(config.arch, "arm64");
    assert_eq!(config.run_margin, Duration::from_secs(120));
    assert_eq!(config.max_run, Duration::from_secs(1800));
    assert_eq!(config.default_timeout, Duration::from_secs(900));
    assert_eq!(config.call_timeout, Duration::from_secs(3));
    assert_eq!(config.poll, Duration::from_millis(250));
    assert_eq!(config.prepare_limit, Duration::from_secs(45));
    assert_eq!(config.prepare_concurrency, 3);
    assert_eq!(config.failed_stderr_log_bytes, 2048);
}

/// Catches a flag default that has drifted from the executor's own: the defaults are written
/// as text in the flags and are the config's.
#[test]
fn the_defaults_are_the_executors_own() {
    let flags = js_with(&[]).expect("flags");
    let config = flags.executor.js.expect("js-agent").config;
    let own = Config::new("front-1", 2, 8_000, 32 << 30);
    assert_eq!(config.slots, own.slots);
    assert_eq!(config.arch, own.arch);
    assert_eq!(config.run_margin, own.run_margin);
    assert_eq!(config.max_run, own.max_run);
    assert_eq!(config.default_timeout, own.default_timeout);
    assert_eq!(config.call_timeout, own.call_timeout);
    assert_eq!(config.prepare_limit, own.prepare_limit);
    assert_eq!(config.prepare_concurrency, own.prepare_concurrency);
    assert_eq!(config.failed_stderr_log_bytes, own.failed_stderr_log_bytes);
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

/// Catches a job service or a directory that is not one being taken:
/// each is refused at the flags, naming the flag.
#[test]
fn settings_that_are_not_what_they_name_are_refused_naming_the_flag() {
    let cases: [(&[&str], &str); 4] = [
        (&["--executor-job-url=ftp://x"], "--executor-job-url"),
        (
            &["--executor-job-url=http://jobs.example:80"],
            "--executor-job-url",
        ),
        (
            &["--executor-job-url=https://jobs.example"],
            "--executor-job-ca-file",
        ),
        (
            &["--executor-job-ca-file=/etc/ca.pem"],
            "--executor-job-ca-file",
        ),
    ];
    for (more, flag) in cases {
        let why = refused(more);
        assert!(why.contains(flag), "{more:?}: {why}");
    }
}

/// Catches settings that break a rule the executor's safety rests on starting anyway: the
/// executor's own validation is run on the flags, and a frontend id that cannot label a job
/// is refused.
#[test]
fn settings_that_break_a_rule_of_the_executor_are_refused() {
    for more in [
        &["--executor-default-timeout-secs=4000"][..],
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

/// Asserts that `--executor-base-tree` is the directory the executor's flags carry, as given and
/// not looked at (nothing is read before the start), that it is the one path flag of the
/// executor with a `DIR` value name, and that with no executor any value of it is taken
/// without reading it. Catches: a path that is made absolute or checked at parse time, so a
/// unit whose directory the release's installer makes after the config is read would not parse.
#[test]
fn the_base_tree_flag_is_the_directory_of_the_bundles_and_unread_with_no_executor() {
    let flags = js_with(&["--executor-base-tree=relative/base-tree"]).expect("flags");
    let js = flags.executor.js.expect("js-agent");
    assert_eq!(js.base_tree, std::path::PathBuf::from("relative/base-tree"));
    let flags = js_with(&[]).expect("flags");
    let js = flags.executor.js.expect("js-agent");
    assert_eq!(js.base_tree, std::path::PathBuf::from("/opt/kbf/base-tree"));
    let help = crate::frontend::command().render_long_help().to_string();
    assert!(help.contains("--executor-base-tree <DIR>"), "{help}");

    let mut all = JS.to_vec();
    all.remove(0);
    all[8] = "--executor-base-tree=/does/not/exist";
    let flags = parsed(argv(&all)).expect("flags");
    assert!(flags.executor.js.is_none());
}

fn flags_of(url: &str, token: &Path, ca: Option<&Path>) -> JsAgentFlags {
    let config = Config::new("front-1", 1, 2_000, 16 << 30);
    JsAgentFlags {
        config,
        job_url: url.to_owned(),
        token_file: token.to_path_buf(),
        ca_file: ca.map(Path::to_path_buf),
        base_image: ImageRef::parse(&format!("docker://{BASE}")).expect("an image"),
        kbf_image: ImageRef::parse(&format!("docker://{KBF}")).expect("an image"),
        base_tree: "/opt/kbf/base-tree".into(),
        base_tree_url: String::new(),
        base_tree_keep: 1,
        base_tree_timeout: std::time::Duration::from_secs(60),
        published: std::sync::Arc::default(),
        put_retry: std::time::Duration::from_secs(60),
    }
}

fn write_private(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).expect("written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

/// Catches a client built from files it should have refused: a token file that others can read
/// (the job client takes mode 0600 or 0400 alone) or that is missing, a CA file that is missing or holds no certificate, and a plain URL with a
/// CA. A good token and a plain loopback URL give a client.
#[tokio::test]
async fn the_client_is_built_from_a_private_token_and_a_ca_that_is_a_certificate() {
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
    assert!(why.contains("--executor-job-token-file"), "{why}");
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
    assert!(why.contains("--executor-job-url"), "{why}");
    let why = flags_of("https://localhost:7", &token, Some(&absent))
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-job-ca-file"), "{why}");
    let why = flags_of("http://127.0.0.1:7", &token, Some(&ca))
        .client()
        .expect_err("refused")
        .to_string();
    assert!(why.contains("--executor-job-url"), "{why}");
}

/// Catches `--executor-output-disk-bytes` still being read: a job books no disk, so the flag
/// that sized its scratch is gone, and a frontend given it, as `--flag=value` or as
/// `--flag value`, with `--executor js-agent` or with no executor, is refused when the command line is parsed, with clap's unknown-argument
/// error naming the flag; with every value it once took (the old default, the smallest and a
/// zero it refused) the same. The same command line without it parses, so the refusal is the
/// flag's.
#[test]
fn the_output_disk_flag_is_refused_naming_it() {
    const FLAG: &str = "--executor-output-disk-bytes";
    for value in ["8589934592", "1", "0"] {
        let joined = format!("{FLAG}={value}");
        for given in [vec![joined.as_str()], vec![FLAG, value]] {
            let why = refused(&given);
            assert!(why.contains("unexpected argument"), "{given:?}: {why}");
            assert!(why.contains(&format!("'{FLAG}'")), "{given:?}: {why}");
            let mut bare = argv(&[]);
            bare.extend(given.iter().map(ToString::to_string));
            let why = parsed(bare).expect_err("refused with no executor");
            assert!(why.contains("unexpected argument"), "{given:?}: {why}");
            assert!(why.contains(&format!("'{FLAG}'")), "{given:?}: {why}");
        }
    }
    js_with(&[]).expect("js-agent without the flag parses");
    parsed(argv(&[])).expect("no executor without the flag parses");
}

/// Catches the lease server's flags still being read, or the job service's being optional: each
/// removed flag is refused when the command line is parsed, with an error naming it, so a unit
/// that still has one fails at start and never runs on the wrong protocol; each flag that
/// replaced them is named, in the order of `REQUIRED`, when it is missing; and a frontend with no executor, given
/// none of them, builds no executor and no client.
#[test]
fn old_lease_flags_are_refused_and_job_flags_are_required() {
    for removed in [
        "--executor-lease-url=http://127.0.0.1:7",
        "--executor-lease-token-file=/etc/token",
        "--executor-lease-ca-file=/etc/ca.pem",
        "--executor-lease-ttl-secs=7200",
        "--executor-class=farm-linux",
    ] {
        let why = refused(&[removed]);
        assert!(why.contains(name(removed)), "{removed}: {why}");
        assert!(why.contains("unexpected argument"), "{removed}: {why}");
        // The same with no executor: the unit keeps the job flags, not the removed ones.
        let why = parsed(argv(&[removed])).expect_err("refused");
        assert!(why.contains(name(removed)), "{removed}: {why}");
    }

    let order: Vec<&str> = REQUIRED.iter().map(|(_, long)| *long).collect();
    assert_eq!(
        order,
        [
            "--executor-job-url",
            "--executor-job-token-file",
            "--executor-frontend-id",
            "--executor-base-image",
            "--executor-kbf-image",
            "--executor-base-tree",
        ]
    );
    let why = parsed(argv(&["--executor=js-agent"])).expect_err("refused");
    let first = why.lines().next().expect("a line");
    let all = format!("--executor js-agent needs {}", order.join(", "));
    assert!(first.ends_with(&all), "{all} in {why}");
    for missing in &order {
        let mut all = JS.to_vec();
        all.retain(|flag| name(flag) != *missing);
        let why = parsed(argv(&all)).expect_err("refused");
        let first = why.lines().next().expect("a line");
        let one = format!("--executor js-agent needs {missing}");
        assert!(first.ends_with(&one), "{one} in {why}");
    }

    let flags = parsed(argv(&[])).expect("flags");
    assert!(flags.executor.js.is_none(), "no executor, so no client");
    let none = ExecutorFlags::default();
    assert!(none.js.is_none());
}

/// Catches the flags of the presigned path being read again, or refused without saying why:
/// each of the eight, given a value, or given bare as the switches once were, with
/// `--executor js-agent` or with no executor, is refused when the command line is parsed, naming
/// the flag, the release that removed it and that the flag is to be deleted. The same command
/// line without them parses, so the refusal is the flag's.
#[test]
fn the_flags_of_the_presigned_path_are_refused_naming_the_release() {
    for gone in super::REMOVED {
        let flag = format!("--{gone}=x");
        for why in [
            refused(&[&flag]),
            parsed(argv(&[&flag])).expect_err("refused"),
        ] {
            assert!(
                why.contains(&format!(
                    "invalid value 'x' for '--{gone} <VALUE>': the flag was removed in this \
                     release (kbf-server 0."
                )),
                "{gone}: {why}"
            );
            assert!(why.contains("delete the flag"), "{gone}: {why}");
            assert!(why.contains("presigned path"), "{gone}: {why}");
        }
        let bare = format!("--{gone}");
        let why = refused(&[&bare]);
        assert!(why.contains(&bare), "{gone}: {why}");
    }
    js_with(&[]).expect("js-agent without them parses");
    parsed(argv(&[])).expect("no executor without them parses");
}

/// Catches a log excerpt that cannot be bounded or turned off, or one allowed past the output a
/// result holds: `--executor-failed-stderr-log-bytes` takes 0 (none) to 8192 and refuses more.
#[test]
fn the_stderr_log_bytes_take_zero_to_the_output_cap() {
    for ok in ["0", "8192"] {
        let flag = format!("--executor-failed-stderr-log-bytes={ok}");
        js_with(&[flag.as_str()]).expect("taken");
    }
    let why = js_with(&["--executor-failed-stderr-log-bytes=8193"]).expect_err("refused");
    assert!(why.contains("--executor-failed-stderr-log-bytes"), "{why}");
}

/// Asserts that a js-agent executor given no `--executor-poll-ms` polls every 1000 ms.
#[test]
fn the_poll_interval_defaults_to_one_second() {
    let config = js_with(&[])
        .expect("flags")
        .executor
        .js
        .expect("js-agent")
        .config;
    assert_eq!(config.poll, Duration::from_millis(1000));
}

/// Asserts that `--executor-poll-ms` outside 100 to 10000 is refused, and the ends are taken.
#[test]
fn the_poll_interval_outside_its_range_is_refused() {
    for bad in [
        "--executor-poll-ms=0",
        "--executor-poll-ms=99",
        "--executor-poll-ms=10001",
    ] {
        let why = refused(&[bad]);
        assert!(why.contains("executor-poll-ms"), "{bad}: {why}");
    }
    let low = js_with(&["--executor-poll-ms=100"]).expect("low end");
    assert_eq!(
        low.executor.js.expect("js-agent").config.poll,
        Duration::from_millis(100)
    );
}

/// The flags that make a frontend's task output a put directory, which shared ownership needs.
const PUT_DIR: [&str; 2] = ["--task-out-mode=put-dir", "--task-out-root=/mnt/ts/put"];

/// [`js_with`] and the put-directory flags, with `more`.
fn shared_with(more: &[&str]) -> Result<FrontendFlags, String> {
    let mut all = PUT_DIR.to_vec();
    all.push("--executor-ownership=shared");
    all.extend(more);
    js_with(&all)
}

/// Catches defaults that turn shared ownership on or change the liveness time: given no
/// ownership flag a frontend is single and its liveness time is 45 s, and the same flags with
/// a put directory change nothing of that.
#[test]
fn ownership_defaults_to_single_and_live_secs_to_45() {
    for flags in [js_with(&[]), js_with(&PUT_DIR)] {
        let config = flags.expect("flags").executor.js.expect("js-agent").config;
        assert_eq!(config.ownership, Ownership::Single);
        assert!(!config.shared());
        assert_eq!(config.live, Duration::from_secs(45));
    }
}

/// Catches `--executor-ownership shared` and `--executor-live-secs` that do not reach the
/// executor's config, or a live time that is not read in seconds: shared with a put
/// directory parses to `Ownership::Shared`, a given live time is the config's, and `single`
/// named outright is the default.
#[test]
fn ownership_shared_and_live_secs_become_the_config() {
    let config = |more: &[&str]| {
        let flags = shared_with(more).expect("flags");
        flags.executor.js.expect("js-agent").config
    };
    let default = config(&[]);
    assert_eq!(default.ownership, Ownership::Shared);
    assert!(default.shared());
    assert_eq!(default.live, Duration::from_secs(45));
    assert_eq!(
        config(&["--executor-live-secs=90"]).live,
        Duration::from_secs(90)
    );
    let single = js_with(&["--executor-ownership=single", "--executor-live-secs=7"])
        .expect("flags")
        .executor
        .js
        .expect("js-agent")
        .config;
    assert_eq!(single.ownership, Ownership::Single);
    assert_eq!(single.live, Duration::from_secs(7));
}

/// Catches a mode that clap lets through: a value other than `single` or `shared` is refused
/// at parse, the message names the flag and the values it takes, and a live time that is not a
/// number is refused.
#[test]
fn an_unknown_ownership_mode_or_a_live_time_that_is_no_number_is_refused() {
    for mode in ["--executor-ownership=both", "--executor-ownership="] {
        let why = refused(&[mode]);
        assert!(why.contains("--executor-ownership"), "{mode}: {why}");
        assert!(why.contains("single") && why.contains("shared"), "{why}");
    }
    let why = refused(&["--executor-live-secs=soon"]);
    assert!(why.contains("--executor-live-secs"), "{why}");
}

/// Catches a liveness time under the fence time parsed with shared ownership, and the same
/// time refused where it is not used: 44 s is refused naming the flag (the fence time is 40 s
/// and the margin 5 s), 45 s parses, and under single ownership any time parses.
#[test]
fn a_live_time_under_the_fence_time_is_refused_with_shared_ownership_only() {
    let why = shared_with(&["--executor-live-secs=44"]).expect_err("refused");
    assert!(
        why.contains(
            "--executor-live-secs must be at least the fence time plus 5 s with \
             --executor-ownership shared"
        ),
        "{why}"
    );
    assert!(shared_with(&["--executor-live-secs=45"]).is_ok());
    assert!(js_with(&["--executor-live-secs=1"]).is_ok());
}
/// Asserts the Macs' flags: none by default; `--executor-mac-hosts` with
/// `--executor-mac-task-bundle` becomes the pool, the least macOS 26 unless `--executor-mac-os-version` says otherwise and
/// the roots `--task-out-root`'s unless `--executor-mac-task-out-root` names others; and each
/// refusal names its flag: hosts with no bundle, a bundle or Mac root with no hosts, a bundle
/// not pinned by digest, a root that is not one, and a least macOS of 0.
#[test]
fn the_mac_flags_become_the_pool_of_macs() {
    let mac = |more: &[&str]| {
        let flags = js_with(more).expect("flags");
        flags.executor.js.expect("js-agent").config.mac
    };
    assert_eq!(mac(&[]), None);
    let task = format!("--executor-mac-task-bundle={BUNDLE}");
    let task = task.as_str();
    assert_eq!(
        mac(&["--executor-mac-hosts=2", task]),
        Some(MacPool {
            hosts: 2,
            bundle: BUNDLE.to_owned(),
            os_version: DEFAULT_MAC_OS_VERSION,
            roots: None,
        })
    );
    let own = mac(&[
        "--executor-mac-hosts=1",
        task,
        "--executor-mac-os-version=27",
        "--executor-mac-task-out-root=/Volumes/ts/put",
    ])
    .expect("a pool");
    assert_eq!(own.os_version, 27);
    assert_eq!(
        own.roots,
        Some(PutDirRoots::new("/Volumes/ts/put").expect("roots"))
    );
    let first = |why: String| why.lines().next().unwrap_or_default().to_owned();
    assert_eq!(
        first(refused(&["--executor-mac-hosts=1"])),
        "error: --executor-mac-hosts needs --executor-mac-task-bundle, the release's kbf-task \
         bundle for the Macs"
    );
    for stray in [task, "--executor-mac-task-out-root=/Volumes/ts/put"] {
        assert_eq!(
            first(refused(&[stray])),
            "error: --executor-mac-task-bundle and --executor-mac-task-out-root are for \
             --executor-mac-hosts: with no Mac worker they would do nothing",
            "{stray}"
        );
    }
    assert_eq!(
        first(refused(&[
            "--executor-mac-hosts=1",
            "--executor-mac-task-bundle=kbf-task"
        ])),
        "error: --executor js-agent: the Macs' task bundle \"kbf-task\" is not pinned by an \
         @sha256 digest (NAME@sha256:HEX)"
    );
    let root = refused(&[
        "--executor-mac-hosts=1",
        task,
        "--executor-mac-task-out-root=put",
    ]);
    assert!(
        first(root.clone()).starts_with("error: --executor-mac-task-out-root: "),
        "{root}"
    );
    let zero = refused(&[
        "--executor-mac-hosts=1",
        task,
        "--executor-mac-os-version=0",
    ]);
    assert!(zero.contains("--executor-mac-os-version"), "{zero}");
}

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// Asserts the default of `--executor-input-tree`: with no flag, with or without a put
/// directory, the config's tree is `InputTree::Manifests`. Catches: a default that turns the
/// tree list on.
#[test]
fn input_tree_defaults_to_manifests() {
    for flags in [js_with(&[]), js_with(&PUT_DIR)] {
        let config = flags.expect("flags").executor.js.expect("js-agent").config;
        assert_eq!(config.input_tree, InputTree::Manifests);
    }
}

/// Asserts that `--executor-input-tree provider` with a put directory becomes
/// `InputTree::Provider` in the executor's config, and `manifests` named outright is the
/// default. Catches: the flag parsed and then dropped before the executor's config.
#[test]
fn input_tree_provider_becomes_the_config() {
    for (mode, want) in [
        ("provider", InputTree::Provider),
        ("manifests", InputTree::Manifests),
    ] {
        let tree = format!("--executor-input-tree={mode}");
        let flags = js_with(&[PUT_DIR[0], PUT_DIR[1], &tree]).expect("flags");
        let config = flags.executor.js.expect("js-agent").config;
        assert_eq!(config.input_tree, want, "{mode}");
    }
}

/// Asserts that a value other than `manifests` or `provider` is refused at parse naming the
/// flag and both values. Catches: a mode clap lets through, taken for the default.
#[test]
fn an_unknown_input_tree_is_refused() {
    for mode in ["--executor-input-tree=tree-list", "--executor-input-tree="] {
        let why = refused(&[mode]);
        assert!(why.contains("--executor-input-tree"), "{mode}: {why}");
        assert!(
            why.contains("manifests") && why.contains("provider"),
            "{why}"
        );
    }
}
