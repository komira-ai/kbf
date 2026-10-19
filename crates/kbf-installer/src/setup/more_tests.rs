//! More tests of `setup`: the state database, the refusal of the retired layout, the flags
//! that came with them, and the two real implementations of the seams (`Fetching`, `Postgres`).
//! The helpers are `tests.rs`'s.

use super::*;
use crate::helper::fetcher::fake::FakeFetcher;
use crate::setup::backend::{Fetching, Postgres};
use crate::testkit::{A, c};
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
        &Unwired,
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
/// Mutants to plant: the key left out of either file; the schema step moved after `kbf.toml`;
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
    assert_eq!(*server.asked.borrow(), [format!("setup {DATABASE}")]);
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
/// Mutants to plant: `apply` passed true to the step.
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
        &Unwired,
        &server,
        &raw(Mode::AllInOne),
        dry,
    )
    .expect("dry run");
    assert_eq!(*server.asked.borrow(), [format!("plan {DATABASE}")]);
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
/// Mutants to plant: the step's error ignored.
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
        &Unwired,
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
/// Mutants to plant: the settings file rewritten on every run.
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
    let mut os = settled(&["kbf-frontend@a.service"]);
    os.users.push(user("kbf-server", 989));
    let out = go_on(&os, &mut exec, &Unwired, &up, &raw(Mode::Frontend), APPLY).expect("finished");
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
    let out = go_on(
        &os,
        &mut exec,
        &Unwired,
        &current,
        &raw(Mode::Frontend),
        APPLY,
    )
    .expect("a third run");
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert!(
        out.contains("the schema is at version 3, nothing to apply"),
        "{out}"
    );
    assert!(out.contains("\nnothing to do\n"), "{out}");
}

/// Catches a database flag that is accepted where it must not be: it is required of the two
/// modes that keep state in Postgres and refused of the others; a password (the file is
/// world-readable), another scheme or a character a TOML string would escape is refused; and
/// the message never quotes the URL.
///
/// Mutants to plant: the password check removed; the mode check removed.
#[test]
fn the_database_flag_is_required_where_there_is_state_and_refused_with_a_password() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let mut r = raw(mode);
        r.database = None;
        let why = r.validate().expect_err("required").join("; ");
        assert!(why.contains("--database is required"), "{why}");
    }
    let mut r = raw(Mode::Daemon);
    r.database = Some(DATABASE.to_owned());
    let why = r.validate().expect_err("refused").join("; ");
    assert!(why.contains("--database is for the modes"), "{why}");
    for bad in [
        "postgres://kbf:hunter2@db.example/kbf",
        "postgres:///kbf?host=/run/postgresql&password=hunter2",
    ] {
        let mut r = raw(Mode::Frontend);
        r.database = Some(bad.to_owned());
        let why = r.validate().expect_err("refused").join("; ");
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
        let why = r.validate().expect_err("refused").join("; ");
        assert!(why.contains("not a usable Postgres URL"), "{bad}: {why}");
        assert!(!why.contains(bad), "{why}");
    }
    let mut ok = raw(Mode::Frontend);
    ok.database = Some("postgresql:///kbf?host=/run/postgresql".to_owned());
    assert!(ok.validate().is_ok());
}

/// Catches a setup that writes a settings file somewhere the unit does not read: the unit
/// reads `/etc/kbf/installer.toml`, so `--installer-config` (a flag of `run`) naming another
/// path is refused rather than followed.
#[test]
fn setup_writes_the_installer_toml_the_unit_reads() {
    let mut r = raw(Mode::Daemon);
    r.installer_config = "/srv/other.toml".to_owned();
    let why = r.validate().expect_err("refused").join("; ");
    assert!(why.contains("--installer-config is for run"), "{why}");
}

/// Catches a setup that goes ahead on a host the retired helper still manages (contract
/// section 8: the two never both manage one host): either of its roots, on any mode, stops it
/// before a step with exit 5 and the path; a host without them is not affected.
///
/// Mutants to plant: one of the two paths dropped from the list; the check moved after the
/// first plan is applied.
#[test]
fn an_unmigrated_old_layout_stops_setup_before_any_step() {
    for old in ["/usr/local/kbf", "/var/lib/kbf-deploy"] {
        let mut exec = host();
        exec.entries.insert(
            hp(old),
            Entry::Dir {
                perms: Perms::new(0o755),
                owner: ROOT,
            },
        );
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
        assert!(
            matches!(&err, SetupError::OldLayout(p) if p == old),
            "{err:?}"
        );
        assert_eq!(
            (err.exit(), err.code()),
            (Exit::NeedsOperator, "old_layout")
        );
        assert!(exec.log.is_empty(), "{old}: {:?}", exec.log);
    }
}

/// Catches the first release having a second way in: setup hands a commit to the `Fetcher`
/// and to nothing else, and makes what it returned current. With the fake fetcher on a scratch
/// root the release is in `releases/<commit>` and `state.json` and the link say it is current;
/// the fetcher was asked for that commit and no component list.
///
/// Mutants to plant: the fetcher result ignored (a commit the fetcher did not install made
/// current); the state not written.
#[test]
fn the_first_release_comes_through_the_fetcher_and_becomes_current() {
    let (exec, layout) = scratch("first");
    let fetcher = FakeFetcher::new(&layout);
    // The scratch root goes with the host, so the host lives to the end of the test.
    let fetching = Fetching::new(&fetcher, layout.clone(), exec);
    fetching.install_first(&c(A)).expect("installed");
    let asked = fetcher.calls.lock().expect("lock").clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (asked[0].commit.clone(), asked[0].components.len()),
        (c(A), 0)
    );
    let state = crate::state::read_unlocked(&layout)
        .expect("read")
        .expect("a state");
    assert_eq!(state.current, Some(c(A)));
    let link = std::fs::read_link(layout.join(&layout.current())).expect("current is a link");
    assert_eq!(link.display().to_string(), A);
}

/// Catches a fetch that failed, or lied, being made current: when the fetcher refuses, setup
/// returns its error and writes no state; when it installs a release other than the one asked
/// for it is `Rejected` and nothing becomes current.
///
/// Mutants to plant: the commit comparison removed.
#[test]
fn a_failed_or_mismatched_fetch_is_never_made_current() {
    let (exec, layout) = scratch("first-failed");
    let fetcher = FakeFetcher::new(&layout);
    *fetcher.fail.lock().expect("lock") = Some(FetchError::Unavailable("down".to_owned()));
    let fetching = Fetching::new(&fetcher, layout.clone(), exec);
    let err = fetching.install_first(&c(A)).expect_err("fails");
    assert!(
        matches!(&err, SetupError::Fetch(FetchError::Unavailable(w)) if w == "down"),
        "{err:?}"
    );
    assert!(
        crate::state::read_unlocked(&layout)
            .expect("read")
            .is_none()
    );

    let (exec, layout) = scratch("first-lied");
    let fetcher = FakeFetcher::new(&layout);
    *fetcher.tamper.lock().expect("lock") =
        Some(|r| r.commit = crate::testkit::c(crate::testkit::C));
    let fetching = Fetching::new(&fetcher, layout.clone(), exec);
    let err = fetching.install_first(&c(A)).expect_err("lied");
    assert!(
        matches!(&err, SetupError::Fetch(FetchError::Rejected(_))),
        "{err:?}"
    );
    assert!(
        crate::state::read_unlocked(&layout)
            .expect("read")
            .is_none()
    );
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
        extra: &[],
        takes: Vec::new(),
    };
    let theirs = format!(
        "mode = \"frontend\"\n[scheduler]\nstate-store = \"{DATABASE}\"\nheartbeat-interval-ms = 500\n"
    );
    assert_eq!(decide(Some(&theirs), &ours), Decision::Keep);
    let other = theirs.replace(DATABASE, "postgres:///other?host=/run/postgresql");
    let Decision::Refuse(why) = decide(Some(&other), &ours) else {
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
/// nothing to apply.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn setup_creates_the_schema_over_a_real_server() {
    let url = std::env::var("KBF_TEST_POSTGRES_URL").expect("a test database");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connect");
    client
        .batch_execute("DROP SCHEMA IF EXISTS kbf_state CASCADE")
        .expect("start with no schema");
    let mut r = raw(Mode::Frontend);
    r.database = Some(without_password(&url));
    let backend = Backend {
        releases: &Unwired,
        schemas: &Postgres,
    };
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
    assert!(
        dry.contains("dry run: would create the schema at version 1"),
        "{dry}"
    );
    let none = client
        .query(
            "SELECT 1 FROM pg_namespace WHERE nspname = 'kbf_state'",
            &[],
        )
        .expect("catalog");
    assert!(none.is_empty(), "the dry run created the schema");
    let made = say(APPLY);
    assert!(made.contains("created the schema at version 1"), "{made}");
    client
        .query_one("SELECT version FROM kbf_state.schema_version", &[])
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
/// Mutants to plant: `Syntax` carrying `e.to_string()` again; the `state-store` value back in
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
        extra: &[],
        takes: Vec::new(),
    };
    let theirs = "mode = \"frontend\"\n[scheduler]\nstate-store = \"postgres:///other?host=/run/postgresql\"\nheartbeat-interval-ms = 500\n";
    let Decision::Refuse(why) = decide(Some(theirs), &ours) else {
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
/// Mutants to plant: `redact` left out of the `kbf.toml` refusal (`e` printed); the toml text in
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
        extra: &[],
        takes: Vec::new(),
    };
    let theirs = "mode = \"frontend\"\n[scheduler]\nstate-store = postgres://u:PW@h/x\n";
    let Decision::Refuse(why) = decide(Some(theirs), &ours) else {
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
