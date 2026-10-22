//! `proxy --kbf-binary`: the proxy's own copies of `kbf` in the verb. The config and the start
//! with the flag; the same without it is in the verb's own tests, whose helpers these share.

use std::fs;

use super::tests::{PROXY, file, handover, usage_of};
use super::*;

/// Removes a scratch directory with the test: the copies in it are 0755 files named `kbf`,
/// which the coverage report takes for the binary if they are left under the target directory.
#[cfg(target_os = "linux")]
struct Gone(PathBuf);

#[cfg(target_os = "linux")]
impl Drop for Gone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The verb's start with the proxy's own copies: the installed release is the copy in
/// `releases/` of the state directory (a stand-in that would fail at once if it were ever
/// run), recorded in `releases.json` with the schema it needs, and the database is unreachable.
///
/// Catches a start that looks for the release under the layout (it has none there: the start
/// would be held as `binary-missing`), reads the root half's `status.json`, or does not
/// migrate: the attempt must fail as `migration-failed`, in the line the log gets and in
/// `boot.json`, with nothing started.
///
/// Mutants it catches: `run_from_copies` left out of the start (`binary-missing`); the status
/// file taken from the root half (`no-installed-release`).
#[cfg(target_os = "linux")]
#[test]
fn the_start_with_copies_runs_the_copy_of_the_installed_release_and_migrates_it_first() {
    use kbf_proxy::Proxy;
    use kbf_proxy::config::ProxyConfig;
    use kbf_state::postgres::PostgresConfig;
    use kbf_state::postgres::schema;

    use crate::fixtures::{FakeRunner, ran, version_json};
    use crate::proxy::boot::Retry;
    use crate::proxy::child_real::ProcChild;
    use crate::proxy::gate_real::ProxyGate;
    use crate::proxy::gate_real_tests::proxy_table;
    use crate::proxy::model::StagedFacts;
    use crate::proxy::runtime::SystemClock;
    use crate::proxy::supervisor::{children_dir, frontend_argv, run_from_copies};
    use crate::testkit::{A, c, host_root};

    let (layout, root) = host_root("proxy-verb-start-copies");
    let _gone = Gone(root.clone());
    let state_dir = root.join("proxy-state");
    fs::create_dir_all(&state_dir).expect("state dir");
    let local = Local::open(root.join("package-kbf"), &state_dir).expect("local");
    fs::write(&local.binary, b"the package's kbf").expect("the package's kbf");
    let stdout = version_json(A, "release");
    let runner = FakeRunner::new(move |_| Ok(ran(0, &stdout, "")));
    local
        .copies
        .ensure(&runner, &local.binary, &c(A))
        .expect("a copy");
    local.orders.adopt(&c(A)).expect("installed");
    let facts = StagedFacts {
        commit: c(A),
        protocol: 1,
        min_protocol: 1,
        state_schema: 1,
        profile: None,
    };
    local.orders.stage(&facts).expect("staged");

    let config = ProxyConfig::from_toml_str(&proxy_table("verb-cp", "")).expect("config");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let (handle, _runner) = Proxy::bind(config.clone(), OwnedFd::from(listener)).expect("proxy");
    let gate = ProxyGate::new(handle.clone());
    let url = "postgres://kbf_marker_user:hunter2_marker@127.0.0.1:1/kbf_marker_db";
    let with = Supervise {
        database: Database::new(
            PostgresConfig::parse(url).expect("parses"),
            schema::embedded(),
        ),
        state_dir: state_dir.clone(),
        proxy_auth_file: config.proxy_auth_file.clone(),
        upstream: config.upstream,
        operator_token_file: config.operator_token_file.clone(),
        status: state_dir.join(releases::STATUS),
        local: Some(local),
    };
    let retry = Retry {
        attempts: 1,
        first_wait: Duration::from_millis(1),
        max_wait: Duration::from_millis(1),
    };
    let child = ProcChild::new(
        layout,
        children_dir(&with.state_dir),
        frontend_argv(&with.proxy_auth_file, with.upstream),
    );
    let child = run_from_copies(child, with.local.as_ref());
    let started = start_with(&handle, &gate, &SystemClock::new(), &child, with, retry);
    let line = startup_line(started);
    assert!(
        line.starts_with("no frontend child adopted, new streams stay held: "),
        "{line}"
    );
    assert!(line.contains("migration-failed"), "{line}");
    let boot = fs::read_to_string(state_dir.join("boot.json")).expect("boot.json");
    assert!(boot.contains("\"reason\":\"migration-failed\""), "{boot}");
    assert!(!state_dir.join("children").exists(), "a child was started");
}

fn args_with(kbf_binary: Option<&str>) -> ProxyArgs {
    ProxyArgs {
        config: None,
        check: false,
        update_dir: None,
        orders_dir: None,
        kbf_binary: kbf_binary.map(PathBuf::from),
    }
}

/// Catches `--kbf-binary` leaving the swap on the root half's `status.json`: with the flag the
/// status file is `releases.json` in the proxy's state directory whatever `--update-dir`
/// says; the copies are opened by the run, not by the config.
///
/// Mutants it catches: the flag not looked at; the status file taken from `--update-dir`.
#[test]
fn with_a_kbf_binary_the_status_file_is_the_proxys_own_releases_json() {
    let table = PROXY.replacen(
        "[proxy]\n",
        "[proxy]\nstate_dir = \"/srv/proxy-state\"\n",
        1,
    );
    let path = file("kbf-binary-places", table.as_bytes());
    let loaded = load(&path).expect("valid");
    let args = ProxyArgs {
        config: Some(path),
        update_dir: Some("/srv/root-half".into()),
        orders_dir: Some("/srv/orders".into()),
        ..args_with(Some("/usr/bin/kbf"))
    };
    let (layout, _) = crate::testkit::host_root_on("kbf-binary-places", Platform::Linux);

    let cfg = handover_config(&args, &loaded, &layout);

    assert_eq!(
        cfg.status_file,
        PathBuf::from("/srv/proxy-state/releases.json")
    );
    assert_eq!(cfg.state_dir, PathBuf::from("/srv/proxy-state"));
    assert!(cfg.local.is_none(), "the copies are opened by the run");
}

/// Catches the copies made without the flag, or made wrong: no flag opens nothing; the flag
/// makes `releases/` in the state directory and names the kbf; a path that is not absolute
/// (the proxy's user runs it) and a state directory that cannot be used are usage errors that
/// name what is wrong.
///
/// Mutants it catches: the copies opened whatever the flag says; the absolute path not
/// required; the state directory's error read as success.
#[test]
fn the_copies_are_opened_only_for_an_absolute_kbf_binary_in_a_usable_state_directory() {
    let state = kbf_host::testkit::scratch("proxy-verb-open-local");
    let none = open_local(&args_with(None), &state).expect("no flag");
    assert!(none.is_none());
    assert!(!state.join("releases").exists(), "nothing made");

    let found = open_local(&args_with(Some("/usr/bin/kbf")), &state);
    let local = found.expect("the flag").expect("copies");
    assert_eq!(local.binary, PathBuf::from("/usr/bin/kbf"));
    assert_eq!(local.copies.root(), state.join("releases"));
    assert!(state.join("releases").is_dir());

    let text = usage_of(open_local(&args_with(Some("kbf")), &state));
    assert!(
        text.contains("--kbf-binary: kbf is not an absolute path"),
        "{text}"
    );

    let blocked = state.join("blocked");
    fs::write(&blocked, b"a file").expect("a file in the way");
    let text = usage_of(open_local(&args_with(Some("/usr/bin/kbf")), &blocked));
    let dir = blocked.join("releases").display().to_string();
    assert!(text.starts_with(&dir), "{text}");
}

/// Catches `--check` passing a configuration the run refuses: a `--kbf-binary` that is not an
/// absolute path is a usage error with `--check` as without it, and an absolute one passes.
///
/// Mutants it catches: the path rule left out of the `--check` branch; the rule applied
/// whether or not the flag is given.
#[test]
fn check_refuses_a_kbf_binary_that_is_not_an_absolute_path() {
    let path = file("check-kbf-binary", PROXY.as_bytes());
    let args = |kbf_binary| ProxyArgs {
        config: Some(path.clone()),
        check: true,
        ..args_with(kbf_binary)
    };
    assert_eq!(run(&args(Some("/usr/bin/kbf")), Path::new("/")), Ok(()));
    assert_eq!(run(&args(None), Path::new("/")), Ok(()));
    let text = usage_of(run(&args(Some("kbf")), Path::new("/")));
    assert!(
        text.contains("--kbf-binary: kbf is not an absolute path"),
        "{text}"
    );
}

/// Catches the start-up line claiming an order spool a proxy with its own copies does not have.
///
/// Mutants it catches: the line printing the spool directory whatever `local` says; the line
/// leaving out that the proxy keeps its own copies.
#[test]
fn the_start_up_line_says_there_is_no_order_spool_with_the_proxys_own_copies() {
    let mut cfg = handover("line-local");
    let binary = PathBuf::from("/usr/bin/kbf");
    cfg.local = Some(Local::open(binary, &cfg.state_dir).expect("copies"));
    let parts = handover::parts(&cfg).expect("the seams");
    assert_eq!(
        update_line(&parts, &cfg),
        format!(
            "update API on (status {}, orders none, the proxy keeps its own copies, \
             staging deadline 1200 s); a POST is refused: proxy not bound yet",
            cfg.status_file.display()
        )
    );
}

/// Catches a proxy with its own copies that still opens root's order spool and fails fast on
/// it: with a file where `orders/` would be, the seams are built and the start fails only at
/// the bind (a failure, not the usage error of a spool the service user cannot use).
///
/// Mutant it catches: `parts` opening the spool whatever `local` says.
#[test]
fn a_proxy_with_its_own_copies_opens_no_order_spool() {
    let loaded = load(&file("cp-spool", PROXY.as_bytes())).expect("valid");
    let mut handover = handover("cp-spool");
    let binary = PathBuf::from("/usr/bin/kbf");
    handover.local = Some(Local::open(binary, &handover.state_dir).expect("copies"));
    fs::write(&handover.orders_dir, b"a file").expect("a file in the way");
    let regular = OwnedFd::from(fs::File::open(file("fd-cp-spool", b"x")).expect("open"));
    let database = state_database(&file("cp-db", PROXY.as_bytes())).expect("a database");
    let served = serve(
        loaded.proxy,
        database,
        &handover,
        regular,
        &kbf_host::Root::host(),
    );
    match served {
        Err(VerbError::Failed(text)) => {
            assert!(text.starts_with("cannot start the proxy: "), "{text}");
        }
        other => panic!("{other:?}"),
    }
}
