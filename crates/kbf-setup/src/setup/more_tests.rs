//! More tests of `setup`: the state database, the flags
//! that came with them, and the real implementation of the seam (`Postgres`).
//! The helpers are `tests.rs`'s.

use super::*;
use crate::setup::backend::Postgres;
use kbf_host::testkit::ScratchHost;

/// A scratch host with the directories a Linux host has above the layout.
fn scratch(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("setup-{name}"));
    for sub in [
        "run",
        "var/lib/kbf-installer",
        "var/lib/kbf-installer/releases",
    ] {
        std::fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let layout = Layout::linux(kbf_host::Root::new(&exec.root).expect("absolute"));
    (exec, layout)
}

fn frontend_with(server: &Server, options: Options) -> (FakeExec, Result<String, SetupError>) {
    let mut exec = released(host(), Platform::Linux);
    let out = go_on(
        &FakeOs::linux(),
        &mut exec,
        server,
        &raw(Mode::Frontend),
        options,
    );
    (exec, out)
}

/// Catches the state database not reaching the three places that need it, or reaching them
/// in the wrong order: `--database` is written to `installer.toml` (`database`, the key the
/// installer's own schema step reads) and to `kbf.toml` (`[scheduler] state-store`, the flag
/// the frontend starts with); the schema is created once, after `installer.toml` and before
/// `kbf.toml` and any unit, and the output says so.
///
/// Catches: the key left out of either file; the schema step moved after `kbf.toml`;
/// the step run twice.
#[test]
fn the_database_goes_to_both_files_and_its_schema_is_made_between_them() {
    let server = Server::fresh();
    let (exec, out) = frontend_with(&server, APPLY);
    let out = out.expect("setup");
    assert!(
        text(&exec, INSTALLER_TOML).contains(&format!("database = \"{DATABASE}\"\n")),
        "{}",
        text(&exec, INSTALLER_TOML)
    );
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_TOML);
    assert_eq!(
        *server.asked.borrow(),
        [format!("reach {DATABASE}"), format!("setup {DATABASE}")]
    );
    let at = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {out}"))
    };
    let step = at("database step: created the schema at version 3");
    assert!(at(INSTALLER_TOML) < step && step < at(KBF_TOML), "{out}");
    assert!(step < at("systemctl"), "{out}");
}

/// Catches a dry run that writes a schema or hides what it would do: it only asks the server
/// (`plan`), says what `--apply` would do, and changes nothing on the host.
///
/// Catches: `apply` passed true to the step.
#[test]
fn a_dry_run_asks_the_server_and_changes_nothing() {
    let server = Server::fresh();
    let dry = Options {
        apply: false,
        no_start: false,
    };
    let mut exec = host();
    let out = go_on(
        &FakeOs::linux(),
        &mut exec,
        &server,
        &raw(Mode::AllInOne),
        dry,
    )
    .expect("dry run");
    assert_eq!(
        *server.asked.borrow(),
        [format!("reach {DATABASE}"), format!("plan {DATABASE}")]
    );
    assert!(
        out.contains("database step: dry run: would create the schema at version 3"),
        "{out}"
    );
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches a server that cannot be reached being passed over: setup stops with the database
/// step's own reason (exit 1, word `database`), after `installer.toml` (so a re-run has what
/// it needs) and before any release, `kbf.toml` or unit; and a host that keeps no state in
/// Postgres never asks a server.
///
/// Catches: the step's error ignored.
#[test]
fn an_unreachable_database_stops_before_the_config_and_the_units() {
    let server = Server {
        down: true,
        ..Server::fresh()
    };
    let (exec, err) = frontend_with(&server, APPLY);
    let err = err.expect_err("stops");
    assert!(
        matches!(&err, SetupError::Database(why) if why == "no server"),
        "{err:?}"
    );
    assert_eq!((err.exit(), err.code()), (Exit::Failed, "database"));
    assert!(exec.entries.contains_key(&hp(INSTALLER_TOML)));
    assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    assert!(cmds(&exec).iter().all(|c| !c.contains("systemctl")));

    let nodes = Server::fresh();
    let mut exec = host();
    go_on(
        &FakeOs::linux(),
        &mut exec,
        &nodes,
        &raw(Mode::Daemon),
        APPLY,
    )
    .expect("a node");
    assert!(nodes.asked.borrow().is_empty());
}

/// Catches a re-run of a frontend host that does work, or a partial run that cannot be
/// finished: after a run that stopped at the database the same flags with the server back
/// finish it (the files already written are not rewritten), and a third run has nothing to do
/// but report the schema as current.
///
/// Catches: the settings file rewritten on every run.
#[test]
fn a_run_that_stopped_at_the_database_is_finished_by_running_again() {
    let down = Server {
        down: true,
        ..Server::fresh()
    };
    let (mut exec, _) = frontend_with(&down, APPLY);
    let installer_writes = |e: &FakeExec| {
        e.log
            .iter()
            .filter(
                |s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == INSTALLER_TOML),
            )
            .count()
    };
    assert_eq!(installer_writes(&exec), 1);
    let up = Server::fresh();
    let mut os = settled(&["kbf-frontend.service"]);
    os.users.push(user("kbf-server", 989));
    let out = go_on(&os, &mut exec, &up, &raw(Mode::Frontend), APPLY).expect("finished");
    assert_eq!(
        installer_writes(&exec),
        1,
        "the settings were not rewritten"
    );
    assert_eq!(text(&exec, KBF_TOML), FRONTEND_TOML);
    assert!(out.contains("created the schema"), "{out}");

    let current = Server {
        found: Some(3),
        ..Server::fresh()
    };
    let steps = exec.log.len();
    let out = go_on(&os, &mut exec, &current, &raw(Mode::Frontend), APPLY).expect("a third run");
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert!(
        out.contains("the schema is at version 3, nothing to apply"),
        "{out}"
    );
    assert!(out.contains("\nnothing to do\n"), "{out}");
}

/// Asserts that a database flag is accepted only where it means something: the two modes that
/// keep state in Postgres take the local default when it is left out (the database `kbf` on the
/// local socket, as the account the mode's unit runs as, marked as defaulted) and a given URL is
/// not marked; the other modes refuse the flag; a password (the file is world-readable), another
/// scheme or a character a TOML string would escape is refused; and the message never quotes the
/// URL.
#[test]
fn the_database_flag_defaults_where_there_is_state_and_is_refused_with_a_password() {
    let local = "postgres:///kbf?host=/run/postgresql&user=kbf";
    for (mode, default) in [(Mode::Frontend, DATABASE), (Mode::AllInOne, local)] {
        let mut r = raw(mode);
        let given = r.validate(Platform::Linux).expect("given");
        assert_eq!(given.database.as_deref(), Some(DATABASE));
        assert!(!given.database_defaulted);
        assert!(given.local_database.is_none());
        r.database = None;
        let left_out = r.validate(Platform::Linux).expect("the local default");
        assert_eq!(left_out.database.as_deref(), Some(default));
        assert!(left_out.database_defaulted);
        assert!(left_out.local_database.is_some());
    }
    let mut r = raw(Mode::Daemon);
    r.database = Some(DATABASE.to_owned());
    let why = r.validate(Platform::Linux).expect_err("refused").join("; ");
    assert!(why.contains("--database is for the modes"), "{why}");
    for bad in [
        "postgres://kbf:hunter2@db.example/kbf",
        "postgres:///kbf?host=/run/postgresql&password=hunter2",
    ] {
        let mut r = raw(Mode::Frontend);
        r.database = Some(bad.to_owned());
        let why = r.validate(Platform::Linux).expect_err("refused").join("; ");
        assert!(why.contains("password"), "{bad}: {why}");
        assert!(!why.contains("hunter2") && !why.contains(bad), "{why}");
    }
    for bad in [
        "sqlite:///var/lib/kbf/state.db",
        "postgres:///kbf?host=/run/postgresql\"x",
        "postgres:///kbf?host=/run/post gresql",
        "postgres:///kbf?host=/run/postgresql&sslmode=require",
    ] {
        let mut r = raw(Mode::Frontend);
        r.database = Some(bad.to_owned());
        let why = r.validate(Platform::Linux).expect_err("refused").join("; ");
        assert!(why.contains("not a usable Postgres URL"), "{bad}: {why}");
        assert!(!why.contains(bad), "{why}");
    }
    let mut ok = raw(Mode::Frontend);
    ok.database = Some("postgresql:///kbf?host=/run/postgresql&user=kbf".to_owned());
    assert!(ok.validate(Platform::Linux).is_ok());
}

/// Catches a setup that writes a settings file somewhere the unit does not read: the unit
/// reads `/etc/kbf/installer.toml`, so `--installer-config` (a flag of `run`) naming another
/// path is refused rather than followed.
#[test]
fn setup_writes_the_installer_toml_the_unit_reads() {
    let mut r = raw(Mode::Daemon);
    r.installer_config = Some("/srv/other.toml".to_owned());
    let why = r.validate(Platform::Linux).expect_err("refused").join("; ");
    assert!(why.contains("--installer-config is for run"), "{why}");
}

/// Catches `setup` skipping the enable of a unit an older file shadows, and so leaving a node
/// on the old flags: a regular file at `/etc/systemd/system/<the mode's unit>` (systemd reads
/// `/etc` before `/usr/lib`, and `setup` skips a unit that is already enabled) stops setup
/// with exit 5 and `unit_shadow` before any step, dry run or apply, naming the file and the
/// fix. A link (what `systemctl link` makes), another mode's file, and no file are not
/// refused.
///
/// Catches: the check dropped; the check refusing a link; the check refusing another mode's file; the check after
/// the first plan is applied.
#[test]
fn a_regular_file_shadowing_the_packaged_unit_stops_setup_before_any_step() {
    let cases = [
        (Mode::StorageAndDaemon, "kbf-storage-and-daemon.service"),
        (Mode::Daemon, "kbf-daemon.service"),
        (Mode::Storage, "kbf-storage.service"),
        (Mode::AllInOne, "kbf-all-in-one.service"),
        (Mode::Frontend, "kbf-frontend.service"),
    ];
    for (mode, file) in cases {
        let path = format!("/etc/systemd/system/{file}");
        for options in [
            APPLY,
            Options {
                apply: false,
                no_start: false,
            },
        ] {
            let mut exec = host();
            put(&mut exec, &path, 0o644, ROOT, "[Service]\nExecStart=/old\n");
            let err = go(&FakeOs::linux(), &mut exec, &raw(mode), options).expect_err("refused");
            assert_eq!(
                (err.exit(), err.code()),
                (Exit::NeedsOperator, "unit_shadow"),
                "{file}"
            );
            let said = err.to_string();
            assert!(said.contains(&path), "{said}");
            assert!(
                said.contains("mv ") && said.contains("daemon-reload"),
                "{said}"
            );
            assert!(!said.contains('\n'), "one line: {said}");
            assert!(exec.log.is_empty(), "{file}: {:?}", exec.log);
        }
    }

    // Not refused: a link to the packaged unit, a file of another mode, and no file at all.
    let mut exec = host();
    exec.entries.insert(
        hp("/etc/systemd/system/kbf-daemon.service"),
        Entry::Link {
            target: "/usr/lib/systemd/system/kbf-daemon.service".into(),
        },
    );
    put(
        &mut exec,
        "/etc/systemd/system/kbf-storage.service",
        0o644,
        ROOT,
        "x",
    );
    go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY)
        .expect("a link and another mode's file pass");
}

/// Catches the real schema seam quoting the URL or connecting at construction: a URL the
/// client cannot take is a fixed message (never the text), and a good one makes a runner
/// without touching a server.
#[test]
fn the_postgres_seam_makes_a_runner_without_a_server_and_never_quotes_the_url() {
    assert!(Postgres.of(DATABASE).is_ok());
    let Err(err) = Postgres.of("postgres://u:hunter2@") else {
        panic!("not a usable URL");
    };
    assert!(!err.to_string().contains("hunter2"), "{err}");
    assert!(matches!(err, SetupError::Database(_)));
}

/// Catches `kbf.toml` writing over settings it cannot see once it carries `state-store`: a
/// file with other `[scheduler]` keys is the operator's, kept when `state-store` matches and
/// refused (naming the key) when it differs.
#[test]
fn another_scheduler_key_makes_the_file_the_operators() {
    use crate::setup::kbf_toml::{Decision, Ours, decide};
    let ours = Ours {
        mode: "frontend",
        frontend_url: None,
        token_file: None,
        api_token_file: None,
        state_store: Some(DATABASE),
        update: None,
        extra: &[],
    };
    let theirs = format!(
        "mode = \"frontend\"\n[scheduler]\nstate-store = \"{DATABASE}\"\nheartbeat-interval-ms = 500\n"
    );
    assert_eq!(
        decide(Some(&theirs), &ours, "/etc/kbf/kbf.toml"),
        Decision::Keep
    );
    let other = theirs.replace(DATABASE, "postgres:///other?host=/run/postgresql");
    let Decision::Refuse(why) = decide(Some(&other), &ours, "/etc/kbf/kbf.toml") else {
        panic!("refused");
    };
    assert!(why.contains("[scheduler] state-store"), "{why}");
}

/// `url` without the password of its `user:password@` part (the user stays). `setup` refuses
/// a URL with a password (the file it goes into is world-readable), and CI's Postgres service
/// trusts host connections, so the password in `KBF_TEST_POSTGRES_URL` is not needed.
fn without_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_owned();
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(user, _)| user);
    format!("{scheme}://{user}@{host}")
}

#[test]
fn without_password_keeps_the_user_and_the_rest() {
    assert_eq!(
        without_password("postgres://postgres:kbf-ci-throwaway@127.0.0.1:5432/postgres"),
        "postgres://postgres@127.0.0.1:5432/postgres"
    );
    for same in ["postgres://u@h/db", "postgres:///db?host=/run/x"] {
        assert_eq!(without_password(same), same);
    }
}

/// Catches `setup` not reaching a real database through its own seam (the fake-server tests
/// drive `run` alone): on a fake host, with the real `Postgres` seam, the dry run says what it
/// would do and creates nothing, `--apply` creates the schema, and a second `--apply` finds
/// nothing to apply. The database is the test's own and empty, so the schema `setup` creates
/// is in no other test's database.
#[test]
fn setup_creates_the_schema_over_a_real_server() {
    let scratch = kbf_state::postgres::TestDatabase::empty();
    let url = scratch.url();
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    let mut r = raw(Mode::Frontend);
    r.database = Some(without_password(&url));
    let schemas = Postgres;
    let backend = Backend { schemas: &schemas };
    let say = |options: Options| {
        let mut out = Vec::new();
        let mut exec = released(host(), Platform::Linux);
        run(&FakeOs::linux(), &mut exec, &backend, &r, options, &mut out).expect("setup");
        String::from_utf8(out).expect("text")
    };
    let dry = say(Options {
        apply: false,
        no_start: true,
    });
    let latest = kbf_pack_index::schema::latest();
    let would = format!("dry run: would create the schema at version {latest}");
    assert!(dry.contains(&would), "{dry}");
    let none = client
        .query(
            "SELECT 1 FROM pg_namespace WHERE nspname = 'kbf_state'",
            &[],
        )
        .expect("catalog");
    assert!(none.is_empty(), "the dry run created the schema");
    let made = say(APPLY);
    let created = format!("created the schema at version {latest}");
    assert!(made.contains(&created), "{made}");
    client
        .query_one("SELECT max(version) FROM kbf_state.schema_version", &[])
        .expect("the version row");
    let again = say(APPLY);
    assert!(again.contains("nothing to apply"), "{again}");
}

/// Catches a setup that an interruption leaves unfinishable, or whose re-run reaches a
/// different host than a clean run: a failure planted at every single step of a clean run,
/// in every Linux mode, must be an error, must never leave a `kbf.toml` without the files
/// it names, and the same command again must finish with the files a clean run leaves.
/// This is the stated re-run rule: setup holds no state of its own, each run plans from what
/// is on the host.
#[test]
fn a_failure_at_any_step_is_finished_by_running_again() {
    for mode in [Mode::Storage, Mode::Daemon, Mode::Frontend, Mode::AllInOne] {
        let clean = {
            let mut exec = released(host(), Platform::Linux);
            go_ok(&FakeOs::linux(), &mut exec, &raw(mode));
            exec
        };
        assert!(clean.log.len() > 5, "{mode:?}: {}", clean.log.len());
        for at in 0..clean.log.len() {
            let mut exec = released(host(), Platform::Linux);
            exec.fail_at = Some(at);
            let err = go(&FakeOs::linux(), &mut exec, &raw(mode), APPLY);
            assert!(
                matches!(err, Err(SetupError::Host(_))),
                "{mode:?} at {at}: {err:?}"
            );
            if exec.entries.contains_key(&hp(KBF_TOML)) {
                assert!(
                    exec.entries.contains_key(&hp(INSTALLER_TOML)),
                    "{mode:?} at {at}: kbf.toml before installer.toml"
                );
            }
            exec.fail_at = None;
            go(&FakeOs::linux(), &mut exec, &raw(mode), APPLY)
                .unwrap_or_else(|e| panic!("{mode:?} at {at}: the re-run failed: {e}"));
            assert_eq!(
                exec.entries, clean.entries,
                "{mode:?} at {at}: the re-run left another host than a clean run"
            );
        }
    }
}

/// Catches a credential reaching the output through an error message. A person who types the
/// database URL into `installer.toml` unquoted (`database = postgres://u:pw@...`) makes the
/// file a syntax error, and the parser's own message quotes the offending line; setup tells
/// the operator which line is wrong and never repeats it. The same holds for the line that
/// `kbf.toml` refuses over: the database URL in it is not printed.
///
/// Catches: `Syntax` carrying `e.to_string()` again; the `state-store` value back in
/// the refusal of a differing `kbf.toml`.
#[test]
fn an_error_about_a_file_never_repeats_the_database_url() {
    let mut exec = host();
    put(
        &mut exec,
        INSTALLER_TOML,
        0o644,
        ROOT,
        "confirm_window_s = 900\ndatabase = postgres://kbf:hunter2@db.example/kbf\n",
    );
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
    let shown = err.to_string();
    assert!(
        !shown.contains("hunter2") && !shown.contains("db.example"),
        "{shown}"
    );
    assert!(shown.contains("installer.toml: line 2:"), "{shown}");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(exec.log.is_empty());

    use crate::setup::kbf_toml::{Decision, Ours, decide};
    let ours = Ours {
        mode: "frontend",
        frontend_url: None,
        token_file: None,
        api_token_file: None,
        state_store: Some(DATABASE),
        update: None,
        extra: &[],
    };
    let theirs = "mode = \"frontend\"\n[scheduler]\nstate-store = \"postgres:///other?host=/run/postgresql\"\nheartbeat-interval-ms = 500\n";
    let Decision::Refuse(why) = decide(Some(theirs), &ours, "/etc/kbf/kbf.toml") else {
        panic!("refused");
    };
    assert!(why.contains("[scheduler] state-store"), "{why}");
    assert!(
        !why.contains("postgres:") && !why.contains("run/postgresql"),
        "{why}"
    );
}

/// Catches the refusal of an invalid `kbf.toml` repeating the line the parser objected to (the
/// state-store URL typed unquoted, with its password), and `database.rs` carrying the parser's
/// text for a file that is not TOML.
///
/// Catches: `redact` left out of the `kbf.toml` refusal (`e` printed); the toml text in
/// `DatabaseError::NotToml`.
#[test]
fn an_invalid_config_file_is_refused_without_its_text() {
    use crate::setup::kbf_toml::{Decision, Ours, decide};
    let ours = Ours {
        mode: "frontend",
        frontend_url: None,
        token_file: None,
        api_token_file: None,
        state_store: Some(DATABASE),
        update: None,
        extra: &[],
    };
    let theirs = "mode = \"frontend\"\n[scheduler]\nstate-store = postgres://u:PW@h/x\n";
    let Decision::Refuse(why) = decide(Some(theirs), &ours, "/etc/kbf/kbf.toml") else {
        panic!("refused");
    };
    assert!(why.contains("not valid TOML"), "{why}");
    assert!(!why.contains("PW") && !why.contains("postgres:"), "{why}");

    let (exec, _) = scratch("not-toml");
    let path = exec.root.join("installer.toml");
    std::fs::write(&path, "database = postgres://u:PW@h/x\n").expect("write");
    let err = crate::database::Database::from_installer_toml(&path).expect_err("not toml");
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("PW") && !shown.contains("postgres:"),
        "{shown}"
    );
}

/// The `<key>`s of the property list's `PathState`: the files its `KeepAlive` waits for.
fn gate_files(plist: &str) -> Vec<String> {
    let after = plist
        .split("<key>PathState</key>")
        .nth(1)
        .expect("PathState");
    let dict = after.split("</dict>").next().expect("a dict");
    dict.split("<key>")
        .skip(1)
        .map(|k| k.split("</key>").next().expect("closed").to_owned())
        .collect()
}

/// Catches `setup` writing `installer.toml` where the launchd job does not wait for it: the
/// job's `KeepAlive` is gated on that file, so on a Mac that setup wrote it elsewhere
/// (`/etc/kbf`, the Linux path) the installer job would never run and the daemon would find
/// no `kbf.toml`. Runs the Mac plan, then compares the real plist's gate with the files the
/// plan wrote, and checks nothing went under `/etc/kbf`.
#[test]
fn the_launchd_gate_is_the_installer_toml_setup_writes_on_a_mac() {
    let mut exec = released(host(), Platform::MacOs);
    // What the fixture host holds before setup (certificates the checks look for) is not
    // setup's: only files setup made are compared.
    let planted: Vec<String> = exec.entries.keys().map(ToString::to_string).collect();
    go_ok(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon));

    let files: Vec<String> = exec
        .entries
        .iter()
        .filter(|(_, e)| matches!(e, Entry::File { .. }))
        .map(|(p, _)| p.to_string())
        .filter(|p| !planted.contains(p))
        .collect();
    let written: Vec<&String> = files
        .iter()
        .filter(|p| p.ends_with("/installer.toml"))
        .collect();
    let plist = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/macos/com.komira.kbf-installer.plist"),
    )
    .expect("the packaged property list");
    assert_eq!(
        written,
        gate_files(&plist).iter().collect::<Vec<_>>(),
        "setup wrote installer.toml somewhere the launchd gate does not wait"
    );
    // The node job reads kbf.toml from the layout; on a Mac that file is kbf-mac-provision's
    // (G9), so it was there before setup and setup wrote none.
    assert!(
        planted.iter().any(|p| p == Platform::MacOs.kbf_toml())
            && !files.iter().any(|p| p == Platform::MacOs.kbf_toml()),
        "{files:?}"
    );
    assert!(
        !files.iter().any(|p| p.starts_with("/etc/kbf")),
        "a Mac keeps nothing in /etc/kbf: {files:?}"
    );
    // The configuration directory is made where the files go, and no Linux one is made.
    let dirs: Vec<String> = exec
        .entries
        .iter()
        .filter(|(_, e)| matches!(e, Entry::Dir { .. }))
        .map(|(p, _)| p.to_string())
        .collect();
    assert!(
        dirs.iter().any(|d| d == "/usr/local/etc/kbf"),
        "no Mac config directory: {dirs:?}"
    );
    assert!(
        !dirs.iter().any(|d| d.starts_with("/etc/kbf")),
        "a Linux config directory on a Mac: {dirs:?}"
    );
}

/// Catches one of the config paths left on the Linux directory on one platform (a Mac
/// `api_token` or `config_dir` at `/etc/kbf` reaches no test that runs the daemon plan):
/// each file sits directly in its platform's `config_dir`, which is `/etc/kbf` on Linux and
/// `/usr/local/etc/kbf` on macOS, and the `Layout` paths are the `Platform` ones.
///
/// Catches: the Mac `config_dir` and the Mac `api_token` each set to the Linux path.
#[test]
fn every_config_path_lies_in_its_platforms_config_dir() {
    for (platform, dir) in [
        (Platform::Linux, "/etc/kbf"),
        (Platform::MacOs, "/usr/local/etc/kbf"),
    ] {
        assert_eq!(platform.config_dir(), dir);
        let layout = crate::layout::Layout::new(kbf_host::Root::new("/").expect("root"), platform);
        assert_eq!(layout.config_dir().to_string(), dir);
        for (file, path, held) in [
            ("kbf.toml", platform.kbf_toml(), layout.kbf_toml()),
            (
                "installer.toml",
                platform.installer_toml(),
                layout.installer_toml(),
            ),
            (
                "enroll.token",
                platform.enroll_token(),
                layout.enroll_token(),
            ),
            ("api-token", platform.api_token(), layout.api_token()),
        ] {
            assert_eq!(
                std::path::Path::new(path).parent(),
                Some(std::path::Path::new(dir)),
                "{platform:?} {file}: {path}"
            );
            assert_eq!(held.to_string(), path, "{platform:?} {file}");
        }
    }
}

/// Catches `--installer-config` being pinned to the Linux path on a Mac: the only value setup
/// accepts there is the Mac's own, and the Linux one is refused.
#[test]
fn setup_accepts_only_the_platforms_installer_toml_as_installer_config() {
    let mut r = raw(Mode::Daemon);
    r.installer_config = Some("/usr/local/etc/kbf/installer.toml".to_owned());
    r.validate(Platform::MacOs).expect("the Mac's own file");
    let why = r
        .validate(Platform::Linux)
        .expect_err("not Linux's")
        .join("; ");
    assert!(
        why.contains("setup writes /etc/kbf/installer.toml"),
        "{why}"
    );
    r.installer_config = Some("/etc/kbf/installer.toml".to_owned());
    let why = r
        .validate(Platform::MacOs)
        .expect_err("not the Mac's")
        .join("; ");
    assert!(
        why.contains("setup writes /usr/local/etc/kbf/installer.toml"),
        "{why}"
    );
}
