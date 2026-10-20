//! `--create-local-database` and the checks that a Postgres login is admitted, on a fake host
//! whose `psql` answers from a small model of a server (no server needed). The same code is
//! run against a real server in `database_tests.rs` (the `state-postgres` job).
//!
//! Every refusal and every printed line is checked for the names the operator wrote: they
//! are `secretrole`, `secretdb` and `/run/secretsock` here, and none may appear.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use kbf_host::testkit::reply;
use kbf_host::{Cmd, Step};

use super::*;
use crate::setup::os::Os;
use crate::setup::provision;
use crate::setup::validate::LocalDatabase;

const URL: &str = "postgres:///secretdb?host=/run/secretsock&user=secretrole";
const NAMES: [&str; 3] = ["secretrole", "secretdb", "secretsock"];

fn no_names(text: &str) {
    for name in NAMES {
        assert!(!text.contains(name), "{name} in: {text}");
    }
}

/// The flags of a frontend that has setup make the database.
fn flags() -> Raw {
    let mut r = raw(Mode::Frontend);
    r.database = Some(URL.to_owned());
    r.create_local_database = true;
    r
}

fn local() -> LocalDatabase {
    flags()
        .validate()
        .expect("valid")
        .local_database
        .expect("a local database")
}

/// A Postgres server as `psql` would show it: the role (login, superuser), the database
/// (owned by the role), and what has been asked of it.
#[derive(Default)]
struct World {
    role: Option<Row>,
    database: Option<bool>,
    /// psql exits with this status, saying the names on stderr.
    broken: Option<i32>,
    /// A creation fails.
    refuses_create: bool,
    /// The service account's login is refused.
    refuses_service: bool,
    /// Every script run, with the account that ran it.
    ran: Vec<(Option<String>, String)>,
    envs: Vec<Vec<String>>,
}

/// What `pg_roles` holds of a role, by the column the query may ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    login: bool,
    superuser: bool,
    createrole: bool,
    createdb: bool,
    replication: bool,
    bypassrls: bool,
}

impl Row {
    /// A login role with no privilege: what setup makes.
    fn plain() -> Row {
        Row {
            login: true,
            superuser: false,
            createrole: false,
            createdb: false,
            replication: false,
            bypassrls: false,
        }
    }

    /// The server answers with the columns the query names, in the query's order.
    fn answer(self, script: &str) -> String {
        let columns = [
            ("rolcanlogin", self.login),
            ("rolsuper", self.superuser),
            ("rolcreaterole", self.createrole),
            ("rolcreatedb", self.createdb),
            ("rolreplication", self.replication),
            ("rolbypassrls", self.bypassrls),
        ];
        let asked: Vec<&str> = columns
            .iter()
            .filter(|(name, _)| script.contains(name))
            .map(|(_, v)| flag(*v))
            .collect();
        format!("{}\n", asked.join("|"))
    }
}

type Shared = Rc<RefCell<World>>;

fn psql_answer(world: &Shared, cmd: &Cmd) -> io::Result<std::process::Output> {
    if cmd.program.as_str() != provision::PSQL {
        return Ok(reply(0, "", ""));
    }
    let script = String::from_utf8_lossy(&cmd.stdin).into_owned();
    let mut w = world.borrow_mut();
    w.ran
        .push((cmd.user.as_ref().map(|u| u.name.clone()), script.clone()));
    w.envs
        .push(cmd.env.iter().map(|(k, v)| format!("{k}={v}")).collect());
    if let Some(code) = w.broken {
        return Ok(reply(
            code,
            "",
            "psql: error: connection to secretsock failed for role secretrole",
        ));
    }
    let as_service = cmd.user.as_ref().is_some_and(|u| u.name == "kbf-server");
    if as_service {
        return Ok(if w.refuses_service {
            reply(2, "", "FATAL: no pg_hba.conf entry for secretrole secretdb")
        } else {
            reply(0, "1\n", "")
        });
    }
    if script.contains("CREATE ROLE") || script.contains("CREATE DATABASE") {
        if w.refuses_create {
            return Ok(reply(3, "", "ERROR: role \"secretrole\" already exists"));
        }
        if script.contains("CREATE ROLE") {
            w.role = Some(Row::plain());
        } else {
            w.database = Some(true);
        }
        return Ok(reply(0, "", ""));
    }
    let out = if script.contains("FROM pg_roles") {
        w.role.map_or(String::new(), |row| row.answer(&script))
    } else {
        w.database
            .map_or(String::new(), |mine| format!("{}\n", flag(mine)))
    };
    Ok(reply(0, &out, ""))
}

fn flag(b: bool) -> &'static str {
    if b { "t" } else { "f" }
}

fn linux() -> FakeOs {
    let mut os = FakeOs::linux();
    os.users.push(user("postgres", 120));
    os
}

fn exec_on(world: &Shared) -> FakeExec {
    let mut exec = released(host(), Platform::Linux);
    let w = Rc::clone(world);
    exec.answer = Box::new(move |cmd| psql_answer(&w, cmd));
    exec
}

/// Runs the whole of `setup` for the flags against `world` and `server`.
fn setup_on(
    world: &Shared,
    server: &Server,
    r: &Raw,
    options: Options,
) -> (FakeExec, Result<String, SetupError>) {
    let mut exec = exec_on(world);
    let out = go_on(&linux(), &mut exec, &Unwired, server, r, options);
    (exec, out)
}

fn plan_on(world: &Shared) -> Result<Vec<(&'static str, Step)>, SetupError> {
    let exec = exec_on(world);
    let conn = provision::Connection::with_env(Vec::new());
    provision::plan(&exec, &conn, &local())
}

fn refusal(result: Result<impl std::fmt::Debug, SetupError>) -> String {
    let err = result.expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator, "{err}");
    let text = err.to_string();
    no_names(&text);
    text
}

// ---- the flag ---------------------------------------------------------------------------

/// Catches a flag that takes a URL it cannot make something from, or one that makes
/// something from a name it should refuse: the recipe's URL is accepted and yields the right
/// role, database and socket; a remote host, a missing role, `postgres`, a `pg_` role, a name
/// with a character setup will not create, one over 63 bytes, a mode with no state and a
/// missing `--database` are each refused by the flag, and no refusal quotes the URL.
///
/// Mutants seen red: the socket check removed (a host name accepted); the `postgres` check
/// removed; the 63-byte limit loosened to 64; the no-role check removed.
#[test]
fn the_flag_takes_the_recipe_url_and_refuses_what_it_cannot_make() {
    let ok = local();
    assert_eq!(
        (
            ok.role.as_str(),
            ok.name.as_str(),
            ok.socket_dir.as_str(),
            ok.port
        ),
        ("secretrole", "secretdb", "/run/secretsock", None)
    );
    let mut r = flags();
    r.database = Some("postgresql:///kbf?host=/run/postgresql&user=kbf&port=5433".to_owned());
    let with_port = r.validate().expect("valid").local_database.expect("local");
    assert_eq!(with_port.port, Some(5433));
    assert_eq!(format!("{with_port:?}"), "LocalDatabase { .. }");

    let long = "a".repeat(64);
    let cases = [
        (
            "postgres://secretdb.example/secretdb?user=secretrole",
            "Unix socket directory",
        ),
        (
            "postgres://secretrole@/secretdb?host=secretsock",
            "Unix socket directory",
        ),
        (
            "postgres:///secretdb?host=/run/secretsock&user=postgres",
            "role in --database",
        ),
        (
            "postgres:///secretdb?host=/run/secretsock&user=pg_secretrole",
            "role in --database",
        ),
        (
            "postgres:///secretdb?host=/run/secretsock&user=secret.role",
            "role in --database",
        ),
        (
            "postgres:///secretdb?host=/run/secretsock&user=-secretrole",
            "role in --database",
        ),
        (
            "postgres:///secret.db?host=/run/secretsock&user=secretrole",
            "must name a database",
        ),
        (
            "postgres:///?host=/run/secretsock&user=secretrole",
            "must name a database",
        ),
    ];
    for (url, why) in cases {
        let mut r = flags();
        r.database = Some(url.to_owned());
        let problems = r.validate().expect_err(url).join("; ");
        assert!(problems.contains(why), "{url}: {problems}");
        no_names(&problems);
    }
    for (url, why) in [
        (
            format!("postgres:///secretdb?host=/run/secretsock&user={long}"),
            "role in --database",
        ),
        (
            format!("postgres:///{long}?host=/run/secretsock&user=secretrole"),
            "must name a database",
        ),
    ] {
        let mut r = flags();
        r.database = Some(url);
        assert!(r.validate().expect_err("over 63").join("; ").contains(why));
    }
    let mut at_limit = flags();
    at_limit.database = Some(format!(
        "postgres:///{}?host=/run/secretsock&user={}",
        "d".repeat(63),
        "r".repeat(63)
    ));
    assert!(at_limit.validate().is_ok());

    let mut r = flags();
    r.database = None;
    assert!(
        r.validate()
            .expect_err("no url")
            .join("; ")
            .contains("--create-local-database needs --database")
    );
    let mut r = raw(Mode::Daemon);
    r.create_local_database = true;
    let why = r.validate().expect_err("a node keeps no state").join("; ");
    assert!(
        why.contains("--create-local-database is for the modes"),
        "{why}"
    );
    let mut r = raw(Mode::Daemon);
    r.database = Some(URL.to_owned());
    r.create_local_database = true;
    let why = r.validate().expect_err("a node keeps no state").join("; ");
    assert!(
        why.contains("--create-local-database is for the modes"),
        "{why}"
    );
}

/// Catches a URL with no role being accepted: setup runs as root and the service as its own
/// account, so each would be a different role (the process's user) in one database.
///
/// Mutant seen red: the no-role check removed.
#[test]
fn a_database_url_names_its_role() {
    let mut r = raw(Mode::Frontend);
    r.database = Some("postgres:///secretdb?host=/run/secretsock".to_owned());
    let why = r.validate().expect_err("no role").join("; ");
    assert!(why.contains("names no role: add user=ROLE"), "{why}");
    no_names(&why);
}

// ---- what is made -----------------------------------------------------------------------

/// Catches the wrong things being made, or in the wrong order: on a server with nothing, the
/// plan is the role and then the database, both run as the `postgres` account, and neither
/// step prints a name (they ride in the environment and on the script's `\set` lines); the
/// role has no password and no privilege, and the database is the role's.
///
/// Mutants seen red: the two steps swapped (the database needs its owner); `LOGIN` dropped;
/// `NOSUPERUSER` dropped; the step run as the invoking user.
#[test]
fn a_fresh_server_gets_a_plain_login_role_then_its_database() {
    let world = Shared::default();
    let (exec, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    let out = out.expect("setup");
    no_names(&out);
    let w = world.borrow();
    let creates: Vec<_> = w.ran.iter().filter(|(_, s)| s.contains("CREATE")).collect();
    assert_eq!(creates.len(), 2, "{:?}", w.ran);
    assert!(creates[0].1.contains(
        "CREATE ROLE :\"role\" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION;"
    ));
    assert!(!creates[0].1.contains("PASSWORD"));
    assert!(
        creates[1]
            .1
            .contains("CREATE DATABASE :\"db\" OWNER :\"role\";")
    );
    for (user, script) in &creates {
        assert_eq!(user.as_deref(), Some("postgres"));
        assert!(script.starts_with("\\set role 'secretrole'\n\\set db 'secretdb'\n"));
    }
    assert!(
        out.contains("+ create the login role: run (as postgres) /usr/bin/psql"),
        "{out}"
    );
    assert!(
        out.contains("+ create the database: run (as postgres) /usr/bin/psql"),
        "{out}"
    );
    assert_eq!(world.borrow().role, Some(Row::plain()));
    assert_eq!(
        exec.log
            .iter()
            .filter(|s| matches!(s, Step::Run(c) if c.program.as_str() == provision::PSQL))
            .count(),
        2
    );
    // Where psql goes is in its environment, not in a printed argument.
    assert!(
        w.envs[0].contains(&"PGHOST=/run/secretsock".to_owned()),
        "{:?}",
        w.envs[0]
    );
    assert!(w.envs[0].contains(&"PGUSER=postgres".to_owned()));
    assert!(w.envs[0].contains(&"PGDATABASE=postgres".to_owned()));
}

/// Catches a re-run that makes things again, or one that stops: with the role and the
/// database there and the database the role's, setup creates nothing and goes on to the
/// schema; with only the role there, it makes only the database.
///
/// Mutants seen red: the role always created; the database always created.
#[test]
fn a_rerun_creates_only_what_is_missing() {
    let world = Shared::default();
    world.borrow_mut().role = Some(Row::plain());
    world.borrow_mut().database = Some(true);
    assert!(plan_on(&world).expect("plan").is_empty());
    let (_, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    let out = out.expect("setup");
    assert!(!out.contains("create the"), "{out}");
    assert!(out.contains("database step: created the schema"), "{out}");

    world.borrow_mut().database = None;
    let steps = plan_on(&world).expect("plan");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].0, "create the database");
}

/// Catches a role or database setup would alter: an existing role that cannot log in, or is a
/// superuser, is refused (setup alters no role); a database another role owns is refused
/// (setup changes no owner); in each case nothing is created and no refusal names the role.
///
/// Mutants seen red: the superuser test removed; the login test removed; the owner test removed.
#[test]
fn a_role_or_database_that_is_not_the_roles_own_is_refused_and_altered_not() {
    for (role, why) in [
        (
            Some(Row {
                login: false,
                ..Row::plain()
            }),
            "not a plain login role",
        ),
        (
            Some(Row {
                superuser: true,
                ..Row::plain()
            }),
            "not a plain login role",
        ),
    ] {
        let world = Shared::default();
        world.borrow_mut().role = role;
        let text = refusal(plan_on(&world));
        assert!(text.contains(why), "{text}");
        assert!(
            world
                .borrow()
                .ran
                .iter()
                .all(|(_, s)| !s.contains("CREATE"))
        );
    }
    let world = Shared::default();
    world.borrow_mut().role = Some(Row::plain());
    world.borrow_mut().database = Some(false);
    let text = refusal(plan_on(&world));
    assert!(text.contains("another role owns it"), "{text}");
    // A database with no such role (so not the role's) is refused the same way.
    let world = Shared::default();
    world.borrow_mut().database = Some(false);
    assert!(refusal(plan_on(&world)).contains("another role owns it"));
    let (exec, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    refusal(out);
    assert!(
        !exec.entries.contains_key(&hp(KBF_TOML)),
        "kbf.toml waits for the database"
    );
}

/// Catches an answer setup does not know being taken for a state: a role or a database row it
/// cannot read refuses instead of creating on a guess.
///
/// Mutants seen red: the catch-all arm of either parser returning a state; the column-count
/// or the column-value check of the role's answer removed.
#[test]
fn an_answer_it_cannot_read_is_a_refusal() {
    for (needle, row) in [
        ("pg_roles", "x|y\n"),
        // The old two-column answer, one column short, and a column that is neither t nor f.
        ("pg_roles", "t|f\n"),
        ("pg_roles", "t|f|f|f|f|x\n"),
        ("pg_database", "maybe\n"),
    ] {
        let mut exec = released(host(), Platform::Linux);
        exec.answer = Box::new(move |cmd| {
            let script = String::from_utf8_lossy(&cmd.stdin).into_owned();
            Ok(if script.contains(needle) {
                reply(0, row, "")
            } else {
                reply(0, "", "")
            })
        });
        let conn = provision::Connection::with_env(Vec::new());
        let text = refusal(provision::plan(&exec, &conn, &local()));
        assert!(text.contains("in a way setup does not know"), "{text}");
    }
}

/// Catches a dry run that changes the server or hides what it would do: it prints each step
/// as `DRY-RUN:`, creates nothing, asks no schema question (there is nothing to log in to),
/// and says so.
///
/// Mutants seen red: the dry run applying the steps; the schema asked in a dry run.
#[test]
fn a_dry_run_names_the_steps_and_creates_nothing() {
    let world = Shared::default();
    let server = Server::fresh();
    let (exec, out) = setup_on(&world, &server, &flags(), DRY);
    let out = out.expect("dry run");
    no_names(&out);
    assert!(
        out.contains("DRY-RUN: create the login role: run (as postgres)"),
        "{out}"
    );
    assert!(
        out.contains("DRY-RUN: create the database: run (as postgres)"),
        "{out}"
    );
    assert!(
        out.contains("the database does not exist yet, so its schema would be created after it"),
        "{out}"
    );
    assert!(
        world
            .borrow()
            .ran
            .iter()
            .all(|(_, s)| !s.contains("CREATE"))
    );
    assert!(exec.log.is_empty());
    assert!(server.asked.borrow().is_empty(), "{:?}", server.asked);

    // With everything there, a dry run goes on to ask the schema's state.
    world.borrow_mut().role = Some(Row::plain());
    world.borrow_mut().database = Some(true);
    let (_, out) = setup_on(&world, &server, &flags(), DRY);
    assert!(
        out.expect("dry run")
            .contains("database step: dry run: would create the schema")
    );
    assert_eq!(server.asked.borrow().len(), 2);
}

const DRY: Options = Options {
    apply: false,
    no_start: false,
};

// ---- when the server cannot be asked -----------------------------------------------------

/// Catches the tool's own words reaching the operator, and every way the superuser cannot be
/// reached being worded alike: a `psql` that exits non-zero (its stderr names the role, the
/// database and the socket), one that cannot be started, a host with no `postgres` account
/// and a Mac are each a refusal with the cause in setup's own words and none of the names.
///
/// Mutants seen red: stderr appended to the refusal; the account check removed.
#[test]
fn a_server_that_cannot_be_asked_is_refused_in_setups_words() {
    let world = Shared::default();
    world.borrow_mut().broken = Some(2);
    let (_, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    let text = refusal(out);
    assert!(text.contains("exit status 2"), "{text}");
    assert!(
        text.contains("run setup without --create-local-database"),
        "{text}"
    );

    let mut exec = released(host(), Platform::Linux);
    exec.answer = Box::new(|cmd| {
        if cmd.program.as_str() == provision::PSQL {
            Err(io::Error::from(io::ErrorKind::NotFound))
        } else {
            Ok(reply(0, "", ""))
        }
    });
    let out = go_on(
        &linux(),
        &mut exec,
        &Unwired,
        &Server::fresh(),
        &flags(),
        APPLY,
    );
    let text = refusal(out);
    assert!(
        text.contains("could not run /usr/bin/psql (entity not found)"),
        "{text}"
    );

    let mut exec = released(host(), Platform::Linux);
    let out = go_on(
        &FakeOs::linux(),
        &mut exec,
        &Unwired,
        &Server::fresh(),
        &flags(),
        APPLY,
    );
    let text = refusal(out);
    assert!(
        text.contains("has none: install the Postgres server"),
        "{text}"
    );
}

/// Catches a `psql` that was killed by a signal (the OOM killer, a `kill`) being worded as an
/// exit status it never had: it has no exit code, so the refusal says `none` and still gives
/// the way out, and names none of the operator's values.
///
/// Mutant seen red: the missing code worded as `0`.
#[test]
fn a_psql_killed_by_a_signal_is_refused_with_no_exit_code() {
    use std::os::unix::process::ExitStatusExt as _;
    let mut exec = released(host(), Platform::Linux);
    exec.answer = Box::new(|cmd| {
        if cmd.program.as_str() != provision::PSQL {
            return Ok(reply(0, "", ""));
        }
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: b"secretrole secretdb /run/secretsock".to_vec(),
        })
    });
    let out = go_on(
        &linux(),
        &mut exec,
        &Unwired,
        &Server::fresh(),
        &flags(),
        APPLY,
    );
    let text = refusal(out);
    assert!(text.contains("(exit status none)"), "{text}");
    assert!(
        text.contains("run setup without --create-local-database"),
        "{text}"
    );
    no_names(&text);
}

/// Catches `--create-local-database` on a Mac, which has no `postgres` account to switch to
/// and no `/usr/bin/psql`: a usage error, before anything is asked.
#[test]
fn the_flag_is_for_linux() {
    let world = Shared::default();
    let mut exec = released(host(), Platform::MacOs);
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| psql_answer(&w, cmd));
    let out = go_on(
        &FakeOs::mac(),
        &mut exec,
        &Unwired,
        &Server::fresh(),
        &flags(),
        APPLY,
    );
    let err = out.expect_err("refused");
    assert_eq!(err.exit(), Exit::Usage, "{err}");
    assert!(world.borrow().ran.is_empty());
}

/// Catches a failed creation leaking the server's words (`role "..." already exists`) and
/// leaving the operator no way forward: the refusal says what failed and that setup can run
/// again, and names nothing.
///
/// Mutant seen red: the step's error shown instead of setup's.
#[test]
fn a_creation_the_server_refuses_is_reported_without_its_words() {
    let world = Shared::default();
    world.borrow_mut().refuses_create = true;
    let (_, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    let text = refusal(out);
    assert!(text.contains("create the login role failed"), "{text}");
    assert!(text.contains("setup can be run again"), "{text}");
}

// ---- the logins -------------------------------------------------------------------------

/// Catches a refusal that tells the operator nothing or the wrong thing: each way the server
/// can refuse setup's own login has its own recipe. A missing server says to install and start
/// one and that the package only suggests it; a refused login gives the exact pg_hba.conf rule
/// and the two pg_ident.conf lines, with the service's account filled in and the operator's
/// names left as placeholders; a missing database says to create it; any other answer gives
/// its SQLSTATE. All are exit 5 and none names a value from the URL; nothing is fetched or
/// written after one.
///
/// Mutants seen red: the pg_ident line for root dropped; the map name dropped from the
/// hba line; the NoServer and NoDatabase texts swapped.
#[test]
fn each_refused_login_has_its_recipe_and_names_nothing() {
    let cases = [
        (Unreached::NoServer, "only suggests it"),
        (
            Unreached::NoDatabase,
            "has no database of the name --database gives",
        ),
        (Unreached::Other("57P03".to_owned()), "SQLSTATE 57P03"),
        (
            Unreached::Login,
            "`local  <database>  <role>  peer map=kbf`",
        ),
    ];
    for (why, expect) in cases {
        let server = Server {
            unreached: Some(why.clone()),
            ..Server::fresh()
        };
        // Without the flag: the check is made, and the recipe is the refusal.
        let mut plain = raw(Mode::Frontend);
        plain.database = Some(URL.to_owned());
        let (exec, out) = setup_on(&Shared::default(), &server, &plain, APPLY);
        let text = refusal(out);
        assert!(text.contains(expect), "{why:?}: {text}");
        assert!(
            text.contains("docs/deploy/kbf-installer-setup.md"),
            "{text}"
        );
        assert_eq!(
            *server.asked.borrow(),
            [format!("reach {URL}")],
            "no schema work after a refused login"
        );
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
        server.asked.borrow_mut().clear();
    }
    let server = Server {
        unreached: Some(Unreached::Login),
        ..Server::fresh()
    };
    let (_, out) = setup_on(&Shared::default(), &server, &flags(), APPLY);
    let text = refusal(out);
    assert!(
        text.contains("`kbf  root  <role>` and `kbf  kbf-server  <role>`"),
        "{text}"
    );
    assert!(text.contains("above the first `local` line"), "{text}");
    assert!(text.contains("reload the server (not restart)"), "{text}");
    // All-in-one runs as `kbf`, and the recipe maps that account.
    let mut aio = raw(Mode::AllInOne);
    aio.database = Some(URL.to_owned());
    let (_, out) = setup_on(&Shared::default(), &server, &aio, APPLY);
    assert!(refusal(out).contains("`kbf  kbf  <role>`"));
}

/// Catches an ident map that admits root and not the service: setup's own login works and the
/// frontend would fail at its first start. After the schema is reachable as root, the service
/// account's login is tried (psql as that account, with the role and database of the URL);
/// a refusal is the pg_ident recipe, and no schema is made.
///
/// Mutants seen red: the service check removed; run as the invoking user; against the wrong
/// account.
#[test]
fn the_services_own_login_is_tried_and_its_refusal_is_the_recipe() {
    let world = Shared::default();
    let (_, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
    out.expect("setup");
    let w = world.borrow();
    let service = w
        .ran
        .iter()
        .zip(&w.envs)
        .find(|((_, s), _)| s.starts_with("SELECT 1"))
        .expect("the service's login was tried");
    assert_eq!(service.0.0.as_deref(), Some("kbf-server"));
    assert!(
        service.1.contains(&"PGUSER=secretrole".to_owned()),
        "{:?}",
        service.1
    );
    assert!(service.1.contains(&"PGDATABASE=secretdb".to_owned()));
    drop(w);

    world.borrow_mut().refuses_service = true;
    let server = Server::fresh();
    let (_, out) = setup_on(&world, &server, &flags(), APPLY);
    let text = refusal(out);
    assert!(text.contains("`kbf  kbf-server  <role>`"), "{text}");
    assert_eq!(*server.asked.borrow(), [format!("reach {URL}")]);

    // A host without the service's account leaves that to the plan, which refuses it.
    let mut os = linux();
    os.users.retain(|u| u.name != "kbf-server");
    let exec = exec_on(&world);
    provision::check_service_login(&os, &exec, &local(), Mode::Frontend).expect("skipped");
}

// ---- the barrier between the URL and the script ------------------------------------------

/// The characters that would end a quoted `\set` value, start another psql command, or be
/// read by SQL or the shell, with their spellings in a URL (raw, and percent-encoded, which
/// the parser decodes before the name is checked).
const DANGEROUS: [(&str, &str); 8] = [
    ("quote", "'"),
    ("space", " "),
    ("semicolon", ";"),
    ("colon", ":"),
    ("dollar", "$"),
    ("backslash", "\\"),
    ("newline", "\n"),
    ("encoded quote", "%27"),
];

/// The names reach psql in `\set role '...'` lines on its standard input, so the name check
/// is the only barrier against a name closing the quote and running its own commands. A
/// role or a database name holding any of these is refused by the flag, in either place and
/// in either spelling, and no refusal quotes the name.
///
/// Mutant seen red: `'` added to the characters a name may hold.
#[test]
fn a_name_that_could_leave_its_quotes_is_refused_for_the_role_and_the_database() {
    for (what, c) in DANGEROUS {
        let mut spellings = vec![c.to_owned()];
        if c.len() == 1 {
            spellings.push(format!("%{:02X}", c.as_bytes()[0]));
        }
        for c in spellings {
            let urls = [
                (
                    format!("postgres:///secretdb?host=/run/secretsock&user=secret{c}role"),
                    "role in --database",
                ),
                (
                    format!("postgres:///secret{c}db?host=/run/secretsock&user=secretrole"),
                    "must name a database",
                ),
            ];
            for (url, _) in urls {
                let mut r = flags();
                r.database = Some(url.clone());
                let problems = r
                    .validate()
                    .expect_err(&format!("{what} ({c:?}) accepted in {url:?}"))
                    .join("; ");
                assert!(!problems.contains("secret"), "{what}: {problems}");
            }
        }
    }
}

// ---- a role with a privilege is not plain -------------------------------------------------

/// A role that can create roles or databases, replicate, or bypass row security is more than
/// the state needs; setup refuses it as it does a superuser, saying why and quoting no value,
/// and alters nothing (no create runs, and no ALTER ever).
///
/// Mutants seen red: each of the four columns dropped from the question or the test.
#[test]
fn a_role_with_a_privilege_it_does_not_need_is_refused_and_left_alone() {
    let rows = [
        (
            "CREATEROLE",
            Row {
                createrole: true,
                ..Row::plain()
            },
        ),
        (
            "CREATEDB",
            Row {
                createdb: true,
                ..Row::plain()
            },
        ),
        (
            "REPLICATION",
            Row {
                replication: true,
                ..Row::plain()
            },
        ),
        (
            "BYPASSRLS",
            Row {
                bypassrls: true,
                ..Row::plain()
            },
        ),
    ];
    for (attribute, row) in rows {
        let world = Shared::default();
        world.borrow_mut().role = Some(row);
        let text = refusal(plan_on(&world));
        assert!(
            text.contains("not a plain login role"),
            "{attribute}: {text}"
        );
        assert!(text.contains(attribute), "{attribute}: {text}");
        let (exec, out) = setup_on(&world, &Server::fresh(), &flags(), APPLY);
        refusal(out);
        let w = world.borrow();
        assert!(
            w.ran
                .iter()
                .all(|(_, s)| !s.contains("CREATE") && !s.contains("ALTER")),
            "{attribute}: {:?}",
            w.ran
        );
        assert_eq!(w.role, Some(row), "{attribute}: the role is as it was");
        assert!(!exec.entries.contains_key(&hp(KBF_TOML)));
    }
}

// ---- one server ----------------------------------------------------------------------------

/// Provisioning goes to the first host and port, and the login check to the whole URL: with
/// two servers named, setup could make the role on one and log in to the other. The flag
/// refuses a URL with more than one host or more than one port, with or without
/// `--create-local-database`.
///
/// Mutant seen red: the count check removed.
#[test]
fn a_database_url_with_more_than_one_host_or_port_is_refused() {
    for url in [
        "postgres://%2Frun%2Fsecretsock,%2Frun%2Fother/secretdb?user=secretrole",
        "postgres://secrethost,otherhost/secretdb?user=secretrole",
        "postgres:///secretdb?host=/run/secretsock&port=5432,5433&user=secretrole",
    ] {
        for create in [true, false] {
            let mut r = flags();
            r.database = Some(url.to_owned());
            r.create_local_database = create;
            let problems = r.validate().expect_err(url).join("; ");
            assert!(problems.contains("one server"), "{url}: {problems}");
            no_names(&problems);
        }
    }
}

// ---- the live farm's URL ---------------------------------------------------------------------

/// The farm's own spelling: a bare database name, the socket directory and the role in the
/// query. Either scheme parses to the same settings, and setup tries the logins and prints a
/// recipe only when one is refused: it never reads `pg_hba.conf` or `pg_ident.conf`, so the
/// name of the ident map (`kbfmap` there, `kbf` in the recipe) is not something it knows.
#[test]
fn both_schemes_give_the_same_settings_and_no_ident_map_name_is_read() {
    let settings = |scheme: &str| {
        let mut r = raw(Mode::Frontend);
        r.database = Some(format!(
            "{scheme}:///kbf?host=/run/postgresql&user=kbf-server"
        ));
        r.create_local_database = true;
        let valid = r.validate().expect("valid");
        let local = valid.local_database.expect("local");
        (local.role, local.name, local.socket_dir, local.port)
    };
    let expected = (
        "kbf-server".to_owned(),
        "kbf".to_owned(),
        "/run/postgresql".to_owned(),
        None,
    );
    assert_eq!(settings("postgres"), expected);
    assert_eq!(settings("postgresql"), expected);

    // The farm admits the service by a map the fake cannot see; the login is simply admitted.
    let world = Shared::default();
    world.borrow_mut().role = Some(Row::plain());
    world.borrow_mut().database = Some(true);
    let mut r = raw(Mode::Frontend);
    r.database = Some("postgres:///kbf?host=/run/postgresql&user=kbf-server".to_owned());
    r.create_local_database = true;
    let (_, out) = setup_on(&world, &Server::fresh(), &r, APPLY);
    let out = out.expect("setup");
    assert!(!out.contains("pg_ident") && !out.contains("map="), "{out}");
    let w = world.borrow();
    assert!(
        w.ran
            .iter()
            .all(|(_, s)| !s.contains("pg_ident") && !s.contains("pg_hba")),
        "{:?}",
        w.ran
    );
}

// ---- what is left to the unit ----------------------------------------------------------------

/// Catches the flag's own platform check being dropped: `provision` itself refuses a Mac, as
/// a usage error and before it asks anything, whatever the plan did before it.
///
/// Mutant seen red: the platform check removed.
#[test]
fn provision_refuses_a_mac_by_itself() {
    let world = Shared::default();
    let mut exec = exec_on(&world);
    let mut out = Vec::new();
    let err = provision::provision(&FakeOs::mac(), &mut exec, &local(), true, &mut out)
        .expect_err("refused");
    assert_eq!(err.exit(), Exit::Usage, "{err}");
    assert!(world.borrow().ran.is_empty());
}

/// Catches a port in the URL being dropped on the way to psql: provisioning would go to the
/// default port while the login check uses the URL's.
///
/// Mutant seen red: the `PGPORT` variable not set.
#[test]
fn the_urls_port_reaches_psql() {
    let mut r = flags();
    r.database =
        Some("postgres:///secretdb?host=/run/secretsock&user=secretrole&port=5433".to_owned());
    let with_port = r.validate().expect("valid").local_database.expect("local");
    let world = Shared::default();
    let exec = exec_on(&world);
    let superuser = linux()
        .user(provision::SUPERUSER)
        .expect("os")
        .expect("postgres");
    let conn = provision::Connection::local(&with_port, superuser);
    provision::plan(&exec, &conn, &with_port).expect("plan");
    let w = world.borrow();
    assert!(
        w.envs[0].contains(&"PGPORT=5433".to_owned()),
        "{:?}",
        w.envs[0]
    );
}

/// Catches an empty role or database name being taken for a name: each is refused by the flag.
///
/// Mutant seen red: the non-empty check removed.
#[test]
fn an_empty_role_or_database_name_is_refused() {
    for url in [
        "postgres:///secretdb?host=/run/secretsock&user=",
        "postgres:///?host=/run/secretsock&user=secretrole&dbname=",
    ] {
        let mut r = flags();
        r.database = Some(url.to_owned());
        let problems = r.validate().expect_err(url).join("; ");
        no_names(&problems);
    }
}

/// A writer that refuses what holds `needle`: the operator's pipe closing mid-run.
struct Closes(&'static str);

impl std::io::Write for Closes {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if String::from_utf8_lossy(buf).contains(self.0) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Catches a closed output being lost: a line setup cannot print about the database (the
/// dry run's "does not exist yet", a step it is about to run, a step it only plans) stops
/// setup with an error instead of going on unseen.
///
/// Mutant seen red: any of the three `?` dropped from its `writeln!`.
#[test]
fn a_closed_output_stops_setup_at_each_database_line() {
    for (needle, options) in [
        ("database step: dry run", DRY),
        ("create the login role", APPLY),
        ("create the login role", DRY),
    ] {
        let world = Shared::default();
        let mut exec = exec_on(&world);
        let server = Server::fresh();
        let backend = Backend {
            releases: &Unwired,
            schemas: &server,
        };
        let result = run(
            &linux(),
            &mut exec,
            &backend,
            &flags(),
            options,
            &mut Closes(needle),
        );
        let err = result.expect_err(needle);
        assert!(matches!(err, SetupError::Io(_)), "{needle}: {err}");
    }
    std::io::Write::flush(&mut Closes("")).expect("flush");
}
