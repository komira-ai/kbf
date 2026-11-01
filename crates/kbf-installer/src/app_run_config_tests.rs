//! `run` with the installer config and the unit of the host, and the retired setup flags.

use super::*;

/// Catches a frontend or all-in-one unit that activates releases with no database migration
/// and no warning: `run` for the unit of either mode refuses (exit 2, one line) unless
/// `--installer-config` is given, before it touches the host; a unit of a mode with no
/// database (or a launchd label, which names no mode) still runs without one; and a config
/// that cannot be read stops the service rather than starting it without the step.
///
/// Catches: the unit check removed (the all-in-one unit ran its pass); the unit
/// check matching only `kbf-frontend@` (the plain `kbf-frontend.service`, the unit setup enables on
/// a frontend host, ran its pass without the migration);
/// the all-in-one template (`kbf-all-in-one@1.service`) is in the test list.
#[test]
fn run_on_a_frontend_or_all_in_one_unit_needs_the_installer_config() {
    let (layout, root) = seeded("run-database");
    for unit in [
        "kbf-all-in-one.service",
        "kbf-all-in-one@1.service",
        "kbf-frontend@main.service",
        "kbf-frontend.service",
    ] {
        let (exit, err) = on(&root, &["run", "--passes", "1", "--unit", unit]);
        assert_eq!(exit, Exit::Usage, "{unit}: {err}");
        assert_eq!(
            err,
            "kbf-installer: run needs --installer-config on a frontend or all-in-one host \
             (the database is migrated before a release is activated)\n",
            "{unit}"
        );
        assert!(
            !layout
                .join(&layout.update_dir())
                .join("status.json")
                .exists(),
            "{unit}: nothing was done before the refusal"
        );
    }
    for unit in ["kbf.service", "kbf-daemon.service"] {
        let (exit, err) = on(&root, &["run", "--passes", "1", "--unit", unit]);
        assert_eq!(exit, Exit::Ok, "{unit}: {err}");
    }
    let (exit, err) = on(
        &root,
        &["run", "--passes", "1", "--launchd-label", "com.example.kbf"],
    );
    assert_eq!(exit, Exit::Ok, "a label names no mode: {err}");
    let (exit, err) = on(
        &root,
        &[
            "run",
            "--passes",
            "1",
            "--unit",
            "kbf-all-in-one.service",
            "--installer-config",
            "/etc/kbf/missing.toml",
        ],
    );
    assert_eq!(exit, Exit::Failed, "{err}");
    assert!(err.contains("missing.toml"), "{err}");
}

/// Catches a relative `--installer-config` (on `run` or `setup`) being quietly re-rooted
/// under `/` and read from somewhere the operator did not name: both flags take a host path
/// like `--update-dir` does, so a relative one is a usage error (clap, exit 2), not a read.
#[test]
fn the_installer_config_flag_must_be_an_absolute_host_path() {
    for args in [
        &[
            "run",
            "--unit",
            "kbf.service",
            "--installer-config",
            "etc/kbf/installer.toml",
        ][..],
        &[
            "setup",
            "--mode=frontend",
            "--installer-config",
            "./installer.toml",
        ][..],
    ] {
        let mut full = vec!["kbf-installer"];
        full.extend_from_slice(args);
        assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
    }
}

/// Catches `run --installer-config` not attaching the schema step to the helper it drives
/// (the helper tests attach a fake directly, and the Postgres tests never go through `run`):
/// the helper `run` builds from a config naming a database refuses a release's schema with
/// the database's own error (the socket directory does not exist, so no server is needed),
/// and one built without the flag has no step.
///
/// Catches: the `set_schema_step` call removed from `build_helper`.
#[test]
fn run_with_an_installer_config_drives_a_helper_that_migrates_first() {
    let (_, root) = seeded("run-schema-step");
    fs::create_dir_all(root.join("etc/kbf")).expect("etc");
    fs::write(
        root.join("etc/kbf/installer.toml"),
        "database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n",
    )
    .expect("config");
    let with = |extra: &[&str]| {
        let mut argv = vec!["--root", root.to_str().expect("text")];
        argv.extend_from_slice(&RUN);
        argv.extend_from_slice(extra);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        match build_helper(root.clone(), &args) {
            Ok(helper) => helper,
            Err(f) => panic!("{}", f.why),
        }
    };
    let mut helper = with(&["--installer-config", "/etc/kbf/installer.toml"]);
    let step = helper.schema_step_mut().expect("the step is attached");
    let why = step.before_start(1).expect_err("no server to migrate");
    assert!(!why.is_empty());
    assert!(with(&[]).schema_step_mut().is_none(), "no flag, no step");
}

/// Catches `run` building a helper that cannot tell about leases (the stub every build had
/// until a lease source was wired): the helper `run` builds reads the lease cgroups of the
/// unit it restarts, below the host root's `/sys/fs/cgroup`, so a populated lease of
/// `kbf-daemon.service` is one live lease. A unit that never ran (no cgroup), a unit of no
/// mode and a launchd label (the Mac source, which looks under the host root for
/// `sandbox-exec`) each cannot tell, with the reason: the activation is refused
/// (fail closed), and `run` itself still starts for them.
///
/// Catches: `leases::NotWired` left in `build_helper` (the daemon's lease reads as
/// "cannot tell"); the cgroup root taken as the real `/sys/fs/cgroup` instead of the host
/// root's.
#[test]
fn run_builds_a_helper_whose_lease_source_reads_the_restarted_units_cgroup() {
    let (_, root) = seeded("run-lease-source");
    let lease = root.join("sys/fs/cgroup/kbf.slice/kbf-daemon.service/actions/kbf-lease-1-1");
    fs::create_dir_all(&lease).expect("lease cgroup");
    fs::write(lease.join("cgroup.events"), "populated 1\n").expect("events");
    let source = |flags: &[&str]| {
        let mut argv = vec!["--root", root.to_str().expect("text"), "run"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        match build_helper(root.clone(), &args) {
            Ok(helper) => helper.leases().live(),
            Err(f) => panic!("{}", f.why),
        }
    };
    assert_eq!(source(&["--unit", "kbf-daemon.service"]), Ok(1));
    let err = source(&["--unit", "kbf-frontend@a.service"]).expect_err("never ran");
    assert!(err.contains("no such cgroup"), "{err}");
    let err = source(&["--unit", "kbf.service"]).expect_err("no mode's unit");
    assert!(err.contains("is no unit of any mode"), "{err}");
    let err = source(&["--launchd-label", "com.example.kbf"]).expect_err("a Mac source");
    assert!(err.contains("sandbox-exec"), "{err}");
    assert!(!err.contains("no lease source wired"), "{err}");
}

/// Catches `run --installer-config` reading the config under a host root that is not an
/// absolute path (the root is the sandbox flag's; a relative one is refused with its reason
/// rather than reading from the working directory).
///
/// Catches: the `Root::new` error in `under` swallowed.
#[test]
fn run_with_an_installer_config_refuses_a_relative_host_root() {
    let mut argv = RUN.to_vec();
    argv.extend_from_slice(&["--installer-config", "/etc/kbf/installer.toml"]);
    let Verb::Run(args) = parse(&argv).verb else {
        panic!("a run");
    };
    let refused = build_helper(PathBuf::from("relative-root"), &args).err();
    assert!(refused.is_some_and(|f| !f.why.is_empty()));
}

fn run_args(extra: &[&str]) -> RunArgs {
    let mut argv = RUN.to_vec();
    argv.extend_from_slice(extra);
    let Verb::Run(args) = parse(&argv).verb else {
        panic!("a run");
    };
    args
}

/// The file the fetcher reads is `--installer-config` when it is given and the file `setup`
/// writes when it is not.
///
/// Catches the flag ignored (the fetcher would read a file the operator did not name) and a
/// default that is not `setup`'s file.
///
/// Catches: the flag's arm returning the default.
#[test]
fn the_fetcher_reads_the_flags_installer_toml_else_the_one_setup_writes() {
    let root = || kbf_host::Root::new("/").expect("root");
    let linux = Layout::new(root(), Platform::Linux);
    let mac = Layout::new(root(), Platform::MacOs);
    assert_eq!(
        installer_toml_of(&run_args(&[]), &linux).as_str(),
        "/etc/kbf/installer.toml"
    );
    assert_eq!(
        installer_toml_of(&run_args(&[]), &mac).as_str(),
        "/usr/local/etc/kbf/installer.toml"
    );
    let flagged = run_args(&["--installer-config", "/srv/kbf/installer.toml"]);
    assert_eq!(
        installer_toml_of(&flagged, &linux).as_str(),
        "/srv/kbf/installer.toml"
    );
}

/// `run` builds its helper with the host's fetcher, not the build's `NotWired`: a fetch on a
/// scratch host with no `installer.toml` (and no fetch user, on a box that has none) is
/// `Unavailable` with a reason, where the unwired fetcher says it has no fetcher at all.
///
/// Catches `build_helper` going back to `fetcher::NotWired` (every fetch refused with no reason
/// to act on).
///
/// Catches: `Box::new(fetcher::NotWired)` in place of the host fetcher.
#[test]
fn run_builds_its_helper_with_the_host_fetcher_not_the_unwired_one() {
    let (_, root) = seeded("run-fetcher");
    let helper = build_helper(root, &run_args(&[])).unwrap_or_else(|f| panic!("{}", f.why));
    let rec = crate::helper::fetcher::ReleaseLocator {
        commit: c(B),
        components: Vec::new(),
        source: kbf_update::Source::Fetch,
    };
    let got = helper.fetcher().fetch(&rec);
    assert!(
        matches!(&got, Err(crate::helper::fetcher::FetchError::Unavailable(why)) if !why.is_empty()),
        "{got:?}"
    );
}

/// The Linux layout on `root`: the one the tests of the unit `run` derives from the mode pin,
/// whatever OS runs them, because the derivation is Linux's (a Mac has no unit to derive).
fn linux_on(root: &Path) -> Layout {
    Layout::linux(kbf_host::path::Root::new(root).expect("absolute"))
}

/// Writes `text` as the `kbf.toml` of the host `layout` describes.
fn write_kbf_toml(layout: &Layout, text: &str) {
    let file = layout.join(&layout.kbf_toml());
    fs::create_dir_all(file.parent().expect("a directory")).expect("config dir");
    fs::write(file, text).expect("kbf.toml");
}

/// Catches the packaged `kbf-installer.service` (one unit for every mode, no `--unit`) not
/// learning what to restart: with no flag naming the service, `run` restarts the unit
/// `setup` enabled for the `mode` of the host's kbf.toml, one per mode, and a flag still
/// wins over the file.
///
/// Catches: `named_service` ignoring the flags; the first unit of the mode taken
/// from another mode's row.
#[test]
fn run_without_a_service_flag_restarts_the_unit_of_the_hosts_mode() {
    let (_, root) = seeded("run-derived-unit");
    let linux = linux_on(&root);
    let service = |flags: &[&str]| {
        let mut argv = vec!["run", "--passes", "1"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_on(&root, &args, Platform::Linux)
    };
    for (mode, unit) in [
        ("frontend", "kbf-frontend.service"),
        ("storage", "kbf-storage.service"),
        ("daemon", "kbf-daemon.service"),
        ("storage-and-daemon", "kbf-storage-and-daemon.service"),
        ("all-in-one", "kbf-all-in-one.service"),
    ] {
        write_kbf_toml(&linux, &format!("mode = \"{mode}\"\n"));
        let derived = service(&[])
            .unwrap_or_else(|f| panic!("{mode}: {}", f.why))
            .service;
        assert_eq!(derived.unit.as_deref(), Some(unit), "{mode}");
        assert_eq!(derived.launchd_label, None, "{mode}");
    }
    let named = service(&["--unit", "kbf-daemon.service"])
        .expect("named")
        .service;
    assert_eq!(named.unit.as_deref(), Some("kbf-daemon.service"));
    write_kbf_toml(&linux, "mode = \"all-in-one\"\n");
    let named = service(&["--unit", "kbf-daemon.service"])
        .expect("the flag wins")
        .service;
    assert_eq!(named.unit.as_deref(), Some("kbf-daemon.service"));
    let label = service(&["--launchd-label", "com.komira.kbf-daemon"])
        .expect("label")
        .service;
    assert_eq!(label.unit, None);
    assert_eq!(
        label.launchd_label.as_deref(),
        Some("com.komira.kbf-daemon")
    );
}

/// Catches a `run` that starts with no service it can restart: no kbf.toml, no `mode`, a
/// mode kbf does not offer (`node`), or a Mac without a label each end it with exit 2 and the
/// reason, before any pass.
///
/// Catches: the platform check dropped; the unknown mode mapped to a default unit; a
/// missing, unparseable, unreadable or mode-less kbf.toml mapped to a default unit.
#[test]
fn run_with_no_service_and_no_mode_to_derive_one_is_a_usage_error() {
    let (layout, root) = seeded("run-no-unit");
    let linux = linux_on(&root);
    let Verb::Run(args) = parse(&["run", "--passes", "1"]).verb else {
        panic!("a run");
    };
    let refused = |root: &Path| {
        target_on(root, &args, Platform::Linux)
            .err()
            .expect("refused")
    };
    let missing = refused(&root);
    assert_eq!(missing.exit, Exit::Usage);
    assert!(missing.why.contains("--unit"), "{}", missing.why);
    assert!(missing.why.contains("is not there"), "{}", missing.why);
    assert!(missing.why.contains("run setup"), "{}", missing.why);
    let kbf_toml = linux.join(&linux.kbf_toml());
    // Every refusal names the file, says what to do and is a usage exit: no panic, no default.
    let named = |f: &Failure, reason: &str| {
        assert_eq!(f.exit, Exit::Usage, "{reason}: {}", f.why);
        assert!(f.why.contains("/etc/kbf/kbf.toml"), "{reason}: {}", f.why);
        assert!(f.why.contains("--unit"), "{reason}: {}", f.why);
        assert!(f.why.contains("run setup"), "{reason}: {}", f.why);
        assert!(f.why.contains(reason), "{reason}: {}", f.why);
    };
    named(&missing, "is not there");
    write_kbf_toml(&linux, "other = 1\n");
    named(&refused(&root), "has no mode");
    write_kbf_toml(&linux, "mode = 3\n");
    named(&refused(&root), "has no mode");
    write_kbf_toml(&linux, "mode = [unclosed\n");
    named(&refused(&root), "is not TOML");
    fs::write(&kbf_toml, [0xff, 0xfe]).expect("bytes");
    named(&refused(&root), "is not UTF-8");
    fs::write(&kbf_toml, vec![b'#'; (1 << 20) + 1]).expect("big");
    named(&refused(&root), "is over");
    fs::remove_file(&kbf_toml).expect("rm");
    fs::create_dir(&kbf_toml).expect("dir");
    named(&refused(&root), "cannot be read");
    fs::remove_dir(&kbf_toml).expect("rmdir");
    write_kbf_toml(&linux, "mode = \"node\"\n");
    named(&refused(&root), "no mode of kbf");
    write_kbf_toml(&linux, "mode = \"\"\n");
    named(&refused(&root), "no mode of kbf");
    write_kbf_toml(&linux, "mode = \"daemon\"\n");
    let mac = derived_service(&layout, Platform::MacOs)
        .err()
        .expect("a Mac");
    assert_eq!(mac.exit, Exit::Usage);
    assert!(mac.why.contains("--launchd-label"), "{}", mac.why);
    // The whole verb, on the platform this test runs on (the `kbf.toml` is that platform's).
    if Platform::current() == Platform::Linux {
        write_kbf_toml(&layout, "mode = \"daemon\"\n");
        let (exit, err) = on(&root, &["run", "--passes", "1"]);
        assert_eq!(
            exit,
            Exit::Ok,
            "a daemon host does its pass on its unit: {err}"
        );
        write_kbf_toml(&layout, "mode = \"all-in-one\"\n");
        let (exit, err) = on(&root, &["run", "--passes", "1"]);
        assert_eq!(
            exit,
            Exit::Failed,
            "the database mode reads installer.toml: {err}"
        );
        assert!(err.contains("installer.toml"), "{err}");
        assert!(!err.contains("needs --installer-config"), "{err}");
    } else {
        // A Mac derives no label from the mode, whatever its kbf.toml says: it needs the flag.
        for mode in ["daemon", "all-in-one"] {
            write_kbf_toml(&layout, &format!("mode = \"{mode}\"\n"));
            let (exit, err) = on(&root, &["run", "--passes", "1"]);
            assert_eq!(exit, Exit::Usage, "{mode}: {err}");
            assert!(err.contains("--launchd-label"), "{mode}: {err}");
        }
    }
}

/// Catches the derived database-mode unit reading no installer.toml (the schema step then
/// silently absent, the very thing `run` refuses to start without): on a frontend host the
/// target carries `/etc/kbf/installer.toml`, on a daemon host none, and a flag is kept.
///
/// Catches: the default dropped; the default given to every mode.
#[test]
fn a_derived_database_unit_reads_the_default_installer_toml_and_others_none() {
    let (_, root) = seeded("run-derived-config");
    let linux = linux_on(&root);
    let config = |mode: &str, flags: &[&str]| {
        write_kbf_toml(&linux, &format!("mode = \"{mode}\"\n"));
        let mut argv = vec!["run"];
        argv.extend_from_slice(flags);
        let Verb::Run(args) = parse(&argv).verb else {
            panic!("a run");
        };
        target_on(&root, &args, Platform::Linux)
            .expect("target")
            .installer_config
            .map(|p| p.to_string())
    };
    let default = Some("/etc/kbf/installer.toml".to_owned());
    assert_eq!(config("frontend", &[]), default);
    assert_eq!(config("all-in-one", &[]), default);
    assert_eq!(config("daemon", &[]), None);
    assert_eq!(config("storage", &[]), None);
    let other = Some("/etc/kbf/other.toml".to_owned());
    assert_eq!(
        config("all-in-one", &["--installer-config", "/etc/kbf/other.toml"]),
        other
    );
}

/// Catches a host path joined to a root that is not absolute: `under` refuses it with the
/// reason.
#[test]
fn a_path_is_not_joined_to_a_relative_root() {
    let path = HostPath::new("/etc/kbf/installer.toml").expect("a host path");
    let failure = under(Path::new("relative/root"), &path).expect_err("refused");
    assert_eq!(failure.exit, Exit::Failed);
    assert!(!failure.why.is_empty());
    assert_eq!(
        under(Path::new("/srv/root"), &path).expect("joined"),
        PathBuf::from("/srv/root/etc/kbf/installer.toml")
    );
}

/// Asserts that `setup` still takes the flags of the release source, the release keys and the
/// first release, each by its own name and with the values it took before (a script that passes
/// them keeps working for one release), and that they reach the retired flags, which are what
/// the warning is made from; without them nothing is retired.
#[test]
fn setup_still_takes_the_flags_of_the_release_source_and_the_first_release() {
    let cli = parse(&[
        "setup",
        "--mode=daemon",
        "--release-source=bundled",
        "--release-url",
        "https://r.example/kbf",
        "--release-ca",
        "/ca.pem",
        "--frontend-worker-url",
        "https://w.example:8981",
        "--frontend-ca",
        "/fe.pem",
        "--frontend-ca-sha256",
        "ab",
        "--commit",
        "0123456789abcdef0123456789abcdef01234567",
    ]);
    let Verb::Setup(all) = &cli.verb else {
        panic!("not the setup verb");
    };
    let args = &all.setup;
    assert_eq!(args.retired.release_source.as_deref(), Some("bundled"));
    assert_eq!(
        args.retired.release_url.as_deref(),
        Some("https://r.example/kbf")
    );
    assert_eq!(args.retired.release_ca.as_deref(), Some("/ca.pem"));
    assert_eq!(
        args.retired.frontend_worker_url.as_deref(),
        Some("https://w.example:8981")
    );
    assert_eq!(args.retired.frontend_ca.as_deref(), Some("/fe.pem"));
    assert_eq!(args.retired.frontend_ca_sha256.as_deref(), Some("ab"));
    assert_eq!(args.retired.given().len(), 7);
    let plain = parse(&["setup", "--mode=daemon"]);
    let Verb::Setup(all) = &plain.verb else {
        panic!("not the setup verb");
    };
    let args = &all.setup;
    let none = crate::setup::retired::RetiredFlags::default();
    assert_eq!(args.retired, none);
}

/// Asserts that the values `setup` is run with carry the retired flags the command line gave,
/// named in the order they are declared in: the warning is made from them, so a command line
/// whose retired flags were dropped on the way would run without one.
#[test]
fn the_retired_flags_on_the_command_line_reach_the_values_setup_runs_with() {
    let cli = parse(&[
        "setup",
        "--mode=daemon",
        "--commit",
        "0123456789abcdef0123456789abcdef01234567",
        "--release-url",
        "https://r.example/kbf",
    ]);
    let Verb::Setup(all) = &cli.verb else {
        panic!("not the setup verb");
    };
    let raw = raw_from(&all.setup);
    assert_eq!(raw.retired.given(), ["--release-url", "--commit"]);
}
