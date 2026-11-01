//! `installer.toml`'s database setting (no server), and `setup` and the update step against
//! a real server (`KBF_TEST_POSTGRES_URL` names it; those tests fail without it).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_state::postgres::schema::{self, Migration, Outcome, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase, Unreached};
use kbf_state::{Cond, Store, Txn};

use super::{
    Database, DatabaseError, SchemaOps, SchemaStep, apply_line, database_step, dry_run_line,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh `installer.toml` in a directory of its own. The name carries the test's thread
/// name (libtest and nextest name it after the test): nextest runs each test in its own
/// process, where `NEXT` starts at 0 again, and `scratch` empties the directory it hands out.
fn toml_file(body: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "-");
    let dir = kbf_host::testkit::scratch(&format!("database-{test}-{n}"));
    let file = dir.join("installer.toml");
    fs::write(&file, body).expect("written");
    file
}

/// Catches two tests sharing one scratch file: under nextest every test is a process of its
/// own with the counter at 0, so a name from the counter alone made two tests write (and
/// `scratch` delete) the same `installer.toml`, and each read the other's or none.
///
/// Catches: the thread name dropped from the directory name (the first file is gone).
#[test]
fn two_tests_starting_their_count_at_zero_get_their_own_files() {
    let make = |test: &str, body: &'static str| {
        std::thread::Builder::new()
            .name(test.to_owned())
            .spawn(move || {
                NEXT.store(0, Ordering::Relaxed);
                toml_file(body)
            })
            .expect("spawned")
            .join()
            .expect("joined")
    };
    let first = make("database::tests::first", "first = 1\n");
    let second = make("database::tests::second", "second = 2\n");
    assert_ne!(first, second);
    assert_eq!(
        fs::read_to_string(&first).expect("first kept"),
        "first = 1\n"
    );
    assert_eq!(fs::read_to_string(&second).expect("second"), "second = 2\n");
}

/// Catches an installer.toml read wrongly: the one key is found among keys other readers
/// own, and every way it can be wrong is a refusal that does not quote the URL.
///
/// Catches: the password check removed (`Password` never returned); the error
/// for a bad URL formatted with the URL.
#[test]
fn the_database_setting_is_read_and_every_fault_is_refused_without_quoting_the_url() {
    let ok = toml_file(
        "release_source = \"url\"\ndatabase = \"postgres:///kbf?host=/run/postgresql\"\n",
    );
    assert!(Database::from_installer_toml(&ok).is_ok());

    let missing = kbf_host::testkit::scratch("database-none").join("no-such-installer.toml");
    assert!(matches!(
        Database::from_installer_toml(&missing),
        Err(DatabaseError::Unreadable { .. })
    ));
    assert!(matches!(
        Database::from_installer_toml(&toml_file("database = = =")),
        Err(DatabaseError::NotToml(_))
    ));
    for body in ["x = 1", "database = 5"] {
        assert!(matches!(
            Database::from_installer_toml(&toml_file(body)),
            Err(DatabaseError::NoDatabase(_))
        ));
    }
    for (url, secret) in [
        ("postgres://kbf:hunter2@db/kbf", "hunter2"),
        (
            "postgres:///kbf?host=/run/postgresql&password=hunter2",
            "hunter2",
        ),
    ] {
        let err = Database::from_installer_toml(&toml_file(&format!("database = \"{url}\"")))
            .unwrap_err();
        assert!(matches!(err, DatabaseError::Password(_)), "{url}");
        assert!(
            !err.to_string().contains(secret) && !err.to_string().contains("postgres://"),
            "{err}"
        );
    }
    let tls = "postgres://db.example/kbf?sslmode=require";
    let err =
        Database::from_installer_toml(&toml_file(&format!("database = \"{tls}\""))).unwrap_err();
    assert!(matches!(err, DatabaseError::BadUrl { .. }));
    assert!(!err.to_string().contains("db.example"), "{err}");
}

// ---- against a server -----------------------------------------------------------------

/// The test server's URL, which names its own role and the database it administers.
fn url() -> String {
    kbf_testkit::database::server_url()
}

/// A database made for one test and dropped with it, with no schema.
struct Scratch(TestDatabase);

impl Scratch {
    fn new() -> Scratch {
        Scratch(TestDatabase::empty())
    }

    fn url(&self) -> String {
        self.0.url()
    }

    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&self.url()).expect("parses")
    }
}

fn next_release() -> Vec<Migration> {
    let mut set = kbf_pack_index::schema::embedded();
    set.push(
        Migration::new(
            kbf_pack_index::schema::latest() + 1,
            "note",
            "-- kbf-migration: expand\nALTER TABLE kv ADD COLUMN note text;",
        )
        .expect("lints"),
    );
    set
}

fn put(store: &PostgresStore, key: &str) {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
}

/// Catches `setup` leaving a database kbf refuses, and a dry run that writes. Dry run on a
/// fresh database reports and creates nothing; `setup` creates the schema; a store opens
/// on it; `setup` again changes nothing.
///
/// Catches: `plan` made to call `setup` (the dry run created the schema).
#[test]
fn setup_creates_the_schema_once_and_a_dry_run_creates_nothing() {
    let s = Scratch::new();
    let latest = kbf_pack_index::schema::latest();
    let db = Database::new(s.config(), kbf_pack_index::schema::embedded());
    assert_eq!(db.plan(), Ok((None, latest)));
    assert_eq!(
        schema::current(&s.config()),
        Err(SchemaError::NotInitialised)
    );
    let out = db.setup().expect("setup");
    assert_eq!((out.from, out.to), (None, latest));
    assert_eq!(db.plan(), Ok((Some(latest), latest)));
    PostgresStore::open(&s.config()).expect("kbf starts on what setup made");
    assert!(db.setup().expect("again").ran.is_empty());
}

/// Catches the update step migrating to the wrong version or breaking the serving child.
/// Child N is serving on schema N, the newest this build embeds; the update step for a
/// release that needs schema N+1 runs the new migration; child N keeps writing; a release
/// that needs N+2 (an installer older than the release) is refused and the schema stays at
/// N+1; a release that needs N changes nothing.
///
/// Catches: the step applying the installer's newest instead of the release's
/// (the schema moved past what the release asked for).
#[test]
fn the_update_step_migrates_to_what_the_release_needs_while_the_old_child_serves() {
    let s = Scratch::new();
    let n = kbf_pack_index::schema::latest();
    let (n1, n2) = (n + 1, n + 2);
    Database::new(s.config(), kbf_pack_index::schema::embedded())
        .setup()
        .expect("N installed");
    let child = PostgresStore::open(&s.config().with_supported_schema(1, n)).expect("child N");
    put(&child, "a");

    let mut db = Database::new(s.config(), next_release());
    let stays = format!("the schema is at version {n}, nothing to apply");
    assert_eq!(db.before_start(n).expect("nothing to do"), stays);
    assert_eq!(schema::current(&s.config()), Ok(n));
    let moves = format!("migrated the schema from {n} to {n1}");
    assert_eq!(db.before_start(n1).expect("migrates"), moves);
    put(&child, "b");
    assert_eq!(schema::current(&s.config()), Ok(n1));

    let err = db.before_start(n2).unwrap_err();
    let wants = format!("needs state schema {n2}");
    assert!(err.contains(&wants), "{err}");
    assert!(err.contains("update the kbf-installer package"), "{err}");
    assert!(!err.contains('\n'));
    assert_eq!(schema::current(&s.config()), Ok(n1));
}

/// Catches the plan and the update-step line mishandling what the server said, without a
/// server: no schema yet is a plan (`None`), any other fault stays an error, and the step's
/// line is the outcome's text or the error's one line.
#[test]
fn the_plan_and_the_step_line_follow_what_the_server_said() {
    use super::{plan_of, step_line};
    assert_eq!(plan_of(Ok(2), 3).unwrap(), (Some(2), 3));
    assert_eq!(
        plan_of(Err(SchemaError::NotInitialised), 3).unwrap(),
        (None, 3)
    );
    assert!(plan_of(Err(SchemaError::Database("down".into())), 3).is_err());
    let done = schema::Outcome {
        from: Some(1),
        to: 3,
        ran: vec![2, 3],
    };
    assert_eq!(
        step_line(Ok(done)).unwrap(),
        "migrated the schema from 1 to 3"
    );
    assert_eq!(
        step_line(Err(SchemaError::NotInitialised)).unwrap_err(),
        SchemaError::NotInitialised.to_string()
    );
}

/// Catches the step against an unreachable server (the socket directory is missing) hiding
/// the failure: `setup`, `plan` and the update step each return the database's reason.
#[test]
fn setup_plan_and_the_update_step_report_an_unreachable_server() {
    let file = toml_file("database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n");
    let mut db = Database::from_installer_toml(&file).expect("a usable url");
    assert!(db.setup().is_err());
    assert!(db.plan().is_err());
    assert!(!db.before_start(1).unwrap_err().is_empty());
}

/// Catches the database step's three dry-run lines and its apply line going wrong: a fresh
/// database would be created, an older one migrated, a current one left alone, and an apply
/// says what the schema runner did (the lines need no server, so they are tested here).
///
/// Catches: the `v < newest` guard flipped (a behind schema says nothing to do).
#[test]
fn the_database_step_lines_say_create_migrate_or_nothing() {
    assert_eq!(
        dry_run_line("frontend", None, 3),
        "kbf-installer setup (frontend): database step: dry run: would create the schema at version 3"
    );
    assert_eq!(
        dry_run_line("frontend", Some(1), 3),
        "kbf-installer setup (frontend): database step: dry run: would migrate the schema from 1 to 3"
    );
    assert_eq!(
        dry_run_line("all-in-one", Some(3), 3),
        "kbf-installer setup (all-in-one): database step: dry run: the schema is at 3, nothing to do"
    );
    let outcome = Outcome {
        from: None,
        to: 2,
        ran: vec![1, 2],
    };
    assert_eq!(
        apply_line("frontend", &outcome),
        "kbf-installer setup (frontend): database step: created the schema at version 2"
    );
}

/// A schema runner that answers as told, standing in for a server.
struct FakeSchema {
    setup: Result<Outcome, String>,
    plan: Result<(Option<u32>, u32), String>,
}

impl SchemaOps for FakeSchema {
    fn reach(&self) -> Result<(), Unreached> {
        Ok(())
    }

    fn setup(&self) -> Result<Outcome, String> {
        self.setup.clone()
    }

    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        self.plan.clone()
    }
}

/// Catches the database step printing the wrong line once the schema runner answers, or
/// swallowing its error: `--apply` prints the outcome, a dry run prints create, migrate or
/// nothing to do, and a runner's error is the failure. The fake answers an error to
/// the question the step must not ask, so asking it fails the test.
///
/// Catches: `apply` flipped; the error mapped to an empty line.
#[test]
fn the_database_step_prints_what_the_schema_runner_answered() {
    let outcome = Outcome {
        from: None,
        to: 2,
        ran: vec![1, 2],
    };
    let fake = |plan| FakeSchema {
        setup: Ok(outcome.clone()),
        plan,
    };
    assert_eq!(
        database_step(&fake(Err("unused".into())), "frontend", true)
            .unwrap_or_else(|f| panic!("applied: {f}")),
        "kbf-installer setup (frontend): database step: created the schema at version 2"
    );
    for (found, tail) in [
        (None, "would create the schema at version 3"),
        (Some(1), "would migrate the schema from 1 to 3"),
        (Some(3), "the schema is at 3, nothing to do"),
    ] {
        assert_eq!(
            database_step(&fake(Ok((found, 3))), "all-in-one", false)
                .unwrap_or_else(|f| panic!("planned: {f}")),
            format!("kbf-installer setup (all-in-one): database step: dry run: {tail}")
        );
    }
    let broken = FakeSchema {
        setup: Err("no server".into()),
        plan: Err("cannot ask".into()),
    };
    let apply = database_step(&broken, "frontend", true).expect_err("setup failed");
    assert_eq!(apply, "no server");
    let plan = database_step(&broken, "frontend", false).expect_err("plan failed");
    assert_eq!(plan, "cannot ask");
}

/// Catches the real `Database` not being what `setup`'s database step asks (the fake-runner
/// tests drive `database_step` alone): through `SchemaOps`, with a socket directory that does
/// not exist (so no server is needed), both the plan and the setup reach the database and
/// answer with its own reason, never the URL.
///
/// Catches: `Database::plan` or `Database::setup` swapped in `impl SchemaOps`.
#[test]
fn the_real_database_answers_the_schema_ops_with_its_own_error() {
    let url = "postgres:///kbf?host=/nonexistent-kbf-test-socket";
    let db = Database::new(
        PostgresConfig::parse(url).expect("a url"),
        kbf_pack_index::schema::embedded(),
    );
    let planned = SchemaOps::plan(&db).expect_err("no server to ask");
    let applied = SchemaOps::setup(&db).expect_err("no server to apply to");
    for why in [planned, applied] {
        assert!(
            !why.is_empty() && !why.contains("nonexistent-kbf-test-socket"),
            "{why}"
        );
    }
}

/// Catches the real `Database` answering `reach` with something else than the login's own
/// outcome, and a server's words reaching `setup`'s lines: with no server behind the socket,
/// `reach` is `NoServer`; and a statement the server refused (its message quotes the role and
/// the database) is printed without them, while every error about versions is kept whole.
///
/// Catches: `reach` made to return `Ok`; the `Database` arm of `redacted` removed.
#[test]
fn the_real_database_reaches_or_says_why_and_redacts_the_servers_words() {
    let url = "postgres:///kbf?host=/nonexistent-kbf-test-socket&user=kbf";
    let db = Database::new(
        PostgresConfig::parse(url).expect("a url"),
        kbf_pack_index::schema::embedded(),
    );
    assert_eq!(SchemaOps::reach(&db), Err(Unreached::NoServer));

    let said = SchemaError::Database(
        "FATAL: role \"secretrole\" cannot connect to database \"secretdb\" (SQLSTATE 42501)"
            .to_owned(),
    );
    let shown = super::redacted(said);
    assert!(
        !shown.contains("secretrole") && !shown.contains("secretdb") && !shown.is_empty(),
        "{shown}"
    );
    assert_eq!(
        super::redacted(SchemaError::NotInitialised),
        SchemaError::NotInitialised.to_string()
    );
}

// ---- the role and database `--create-local-database` makes, on a real server -----------------

mod provisioned {
    use kbf_host::{Exec, HostExec, Root};
    use kbf_testkit::database::{CatalogGuard, batch_execute_retrying};
    use postgres::NoTls;

    use super::{NEXT, Ordering, url};
    use crate::database::Database;
    use crate::setup::provision::{Connection, plan};
    use crate::setup::validate::LocalDatabase;
    use kbf_state::postgres::{PostgresConfig, reach};

    /// The server `KBF_TEST_POSTGRES_URL` names, in the pieces psql takes.
    struct Server {
        host: String,
        port: String,
        user: String,
        password: String,
    }

    fn server() -> Server {
        let url = url();
        let rest = url.strip_prefix("postgres://").expect("a postgres:// URL");
        let (login, place) = rest.split_once('@').expect("user:password@host");
        let (user, password) = login.split_once(':').unwrap_or((login, ""));
        let (hostport, _db) = place.split_once('/').unwrap_or((place, ""));
        let (host, port) = hostport.split_once(':').unwrap_or((hostport, "5432"));
        Server {
            host: host.to_owned(),
            port: port.to_owned(),
            user: user.to_owned(),
            password: password.to_owned(),
        }
    }

    fn admin() -> Connection {
        let s = server();
        Connection::with_env(vec![
            ("PGHOST".into(), s.host),
            ("PGPORT".into(), s.port),
            ("PGUSER".into(), s.user),
            ("PGPASSWORD".into(), s.password),
            ("PGDATABASE".into(), "postgres".into()),
        ])
    }

    /// Runs each statement of `statement` on the test server, again when the server answers
    /// "tuple concurrently updated". Only a test that holds a [`Names`] calls it, and a
    /// `Names` holds the guard of the shared catalogs.
    fn sql(statement: &str) {
        // One statement per call: several in one are a transaction, where CREATE and DROP
        // DATABASE are refused.
        let mut client = postgres::Client::connect(&url(), NoTls).expect("the test server");
        for one in statement.split(';').filter(|s| !s.trim().is_empty()) {
            batch_execute_retrying(&mut client, one).expect("statement");
        }
    }

    /// A role and a database name unique to one test; both are dropped with it. Roles and
    /// databases belong to the whole server, and these tests make and drop them, with the
    /// provisioning steps (psql, run for real) doing the same: the guard of the shared
    /// catalogs is held from the names until the test ends, so no other test, in this process
    /// or another, changes them meanwhile. The names carry the process id besides.
    struct Names {
        local: LocalDatabase,
        _lock: CatalogGuard,
    }

    impl Names {
        fn new() -> Names {
            let lock = CatalogGuard::take();
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("kbf_b5_{}_{n}", std::process::id());
            let s = server();
            let names = Names {
                local: LocalDatabase {
                    role: format!("{name}_r"),
                    name: format!("{name}_d"),
                    socket_dir: "/unused".to_owned(),
                    port: s.port.parse().ok(),
                },
                _lock: lock,
            };
            names.drop_all();
            names
        }

        fn drop_all(&self) {
            let (role, db) = (&self.local.role, &self.local.name);
            sql(&format!(
                "DROP DATABASE IF EXISTS \"{db}\"; DROP ROLE IF EXISTS \"{role}\";"
            ));
        }

        fn url(&self) -> String {
            let s = server();
            format!(
                "postgres://{}@{}:{}/{}",
                self.local.role, s.host, s.port, self.local.name
            )
        }

        /// The steps `plan` makes, carried out for real.
        fn make(&self) -> usize {
            let steps = plan(&host(), &admin(), &self.local).expect("plan");
            let mut exec = host();
            for (_, step) in &steps {
                exec.apply(step).expect("the step runs");
            }
            steps.len()
        }
    }

    impl Drop for Names {
        fn drop(&mut self) {
            self.drop_all();
        }
    }

    fn host() -> HostExec {
        HostExec::new(Root::new("/").expect("root"))
    }

    fn one(query: &str) -> Vec<String> {
        postgres::Client::connect(&url(), NoTls)
            .expect("the test server")
            .query(query, &[])
            .expect("query")
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    }

    /// Catches provisioning that leaves a server kbf cannot use, or one that is not
    /// repeatable: on a server with neither, `plan` and its steps (psql, run for real) make a
    /// login role with no password and no privilege and a database it owns; a second plan is
    /// empty; the role then logs in to its database, and `setup`'s schema step creates the
    /// schema there and again changes nothing.
    ///
    /// Catches: the role made `SUPERUSER`; `OWNER` dropped from the database; the
    /// role-exists test inverted (the second plan makes the role again).
    #[test]
    fn fresh_create_then_rerun_changes_nothing_and_the_schema_follows() {
        let names = Names::new();
        assert_eq!(names.make(), 2, "a role and a database");
        let (role, db) = (&names.local.role, &names.local.name);
        let state = one(&format!(
            "SELECT rolcanlogin::text || rolsuper::text || rolcreatedb::text || rolcreaterole::text \
             || (rolpassword IS NULL)::text FROM pg_authid WHERE rolname = '{role}'"
        ));
        assert_eq!(state, ["truefalsefalsefalsetrue"]);
        let owner = one(&format!(
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = '{db}'"
        ));
        assert_eq!(owner, std::slice::from_ref(role));

        assert_eq!(names.make(), 0, "everything is there: nothing to make");

        let config = PostgresConfig::parse(&names.url()).expect("parses");
        assert_eq!(reach(&config), Ok(()), "the role logs in to its database");
        let database = Database::new(config, kbf_pack_index::schema::embedded());
        let first = database.setup().expect("schema");
        assert_eq!(first.from, None);
        assert!(database.setup().expect("again").ran.is_empty());
    }

    /// Catches a missing role being told as anything but a login refusal (the recipe for it
    /// is the role and the pg_hba rule), and a missing database as anything but its own
    /// answer; the text of each names neither. (Moved here from the state crate, whose
    /// coverage the real-server lines would lower.)
    ///
    /// Catches: the class-28 operand dropped; `3D000` mapped to a login refusal.
    #[test]
    fn a_missing_role_is_a_login_refusal_and_a_missing_database_is_told_apart() {
        let s = server();
        let at = |user: &str, db: &str| {
            let url = format!(
                "postgres://{user}:{}@{}:{}/{db}",
                s.password, s.host, s.port
            );
            PostgresConfig::parse(&url).expect("parses")
        };
        let err = reach(&at("kbf_no_such_role_zz", "postgres")).unwrap_err();
        assert_eq!(err, kbf_state::postgres::Unreached::Login);
        assert!(!err.to_string().contains("kbf_no_such_role_zz"), "{err}");
        let err = reach(&at(&s.user, "kbf_no_such_db_zz")).unwrap_err();
        assert_eq!(err, kbf_state::postgres::Unreached::NoDatabase);
        assert!(!err.to_string().contains("kbf_no_such_db_zz"), "{err}");
    }

    /// Catches setup altering what an operator made: a role that cannot log in and a
    /// superuser role are each refused, a database another role owns is refused, and in each
    /// case nothing is created and the refusal names neither the role nor the database.
    ///
    /// Catches: the superuser check removed; the owner check removed; the login
    /// check removed; one of the four privilege columns dropped from the question.
    #[test]
    fn a_role_that_is_not_plain_or_a_database_owned_by_another_is_refused_and_left_alone() {
        let names = Names::new();
        let (role, db) = (names.local.role.clone(), names.local.name.clone());
        let refused = |names: &Names| {
            let err = plan(&host(), &admin(), &names.local).expect_err("refused");
            let text = err.to_string();
            assert!(
                !text.contains(&names.local.role) && !text.contains(&names.local.name),
                "{text}"
            );
            text
        };

        sql(&format!("CREATE ROLE \"{role}\" NOLOGIN"));
        assert!(refused(&names).contains("not a plain login role"));
        assert_eq!(
            one(&format!(
                "SELECT datname FROM pg_database WHERE datname = '{db}'"
            ))
            .len(),
            0
        );
        sql(&format!(
            "DROP ROLE \"{role}\"; CREATE ROLE \"{role}\" LOGIN SUPERUSER"
        ));
        assert!(refused(&names).contains("not a plain login role"));
        sql(&format!("DROP ROLE \"{role}\""));
        // Each privilege setup does not give is refused the same way, and altered not.
        for attribute in ["CREATEROLE", "CREATEDB", "REPLICATION", "BYPASSRLS"] {
            sql(&format!("CREATE ROLE \"{role}\" LOGIN {attribute}"));
            let text = refused(&names);
            assert!(
                text.contains("not a plain login role"),
                "{attribute}: {text}"
            );
            assert!(text.contains(attribute), "{attribute}: {text}");
            sql(&format!("DROP ROLE \"{role}\""));
        }

        // The database exists and the server's own account owns it.
        sql(&format!(
            "CREATE ROLE \"{role}\" LOGIN; CREATE DATABASE \"{db}\""
        ));
        assert!(refused(&names).contains("another role owns it"));
        let owner = one(&format!(
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = '{db}'"
        ));
        assert_ne!(owner, std::slice::from_ref(&role), "setup changed no owner");
    }
}
