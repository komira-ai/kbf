//! The swap's database migrator ([`crate::migrator`]): its refusal words and its stored
//! facts without a server, and the swap's whole path over a real schema in `database`
//! (`KBF_TEST_POSTGRES_URL` names the server; those tests fail without it, and each makes a
//! database of its own). The macOS job skips them by their module name, `::database::`.

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema::{self, SchemaError};

use std::cell::RefCell;
use std::time::Duration;

use kbf_state::postgres::schema::Outcome;

use super::fakes::*;
use super::model::{Mode, Phase, PortError, Step};
use super::ports::{Migrator, Ports};
use super::swap::Swap;
use crate::database::Database;
use crate::fixtures::{FakeRunner, ran, version_json};
use crate::migrator::{DbMigrator, MigrateError, Refusal, installed_schema};
use crate::testkit::{A, B, C, c};

/// A password and names that must never come back in a message.
const URL: &str = "postgres://kbf_marker_user:hunter2_marker@127.0.0.1:1/kbf_marker_db";

fn unreachable_db() -> Database {
    let config = PostgresConfig::parse(URL).expect("parses");
    Database::new(config, schema::embedded())
}

/// Catches a refusal word that moves: scripts and operators match them. Every variant of
/// [`SchemaError`] has a word, and the words are these.
///
/// Mutants seen red: `ContractPending` mapped to `database_error`; `Busy` mapped to
/// `migration_failed`.
#[test]
fn every_schema_refusal_has_its_pinned_word() {
    let cases = [
        (
            SchemaError::NotShipped {
                wanted: 3,
                newest: 2,
            },
            "installer_too_old",
        ),
        (
            SchemaError::DatabaseAhead {
                found: 4,
                newest: 2,
            },
            "installer_too_old",
        ),
        (SchemaError::ContractPending { version: 3 }, "not_additive"),
        (
            SchemaError::Destructive {
                version: 3,
                what: "a DROP",
            },
            "not_additive",
        ),
        (
            SchemaError::Forbidden {
                version: 3,
                what: "EXECUTE",
            },
            "not_additive",
        ),
        (SchemaError::NoKind { version: 3 }, "not_additive"),
        (
            SchemaError::Numbering {
                expected: 2,
                found: 3,
            },
            "schema_invalid",
        ),
        (
            SchemaError::Gap {
                before: 1,
                after: 3,
            },
            "schema_invalid",
        ),
        (SchemaError::NotInitialised, "schema_invalid"),
        (
            SchemaError::TooOld {
                found: 1,
                min: 2,
                max: 3,
            },
            "schema_invalid",
        ),
        (
            SchemaError::TooNew {
                found: 5,
                min: 1,
                max: 3,
                contract: 4,
            },
            "schema_invalid",
        ),
        (SchemaError::Busy, "database_busy"),
        (
            SchemaError::Failed {
                version: 2,
                name: "x".into(),
                why: "y".into(),
            },
            "migration_failed",
        ),
        (SchemaError::Database("down".into()), "database_error"),
    ];
    for (err, word) in cases {
        assert_eq!(Refusal::of(&err).word(), word, "{err}");
    }
}

/// Catches a migration run for a release that is not the staged one (the schema it needs
/// is then unknown, and a guess could migrate past what the release runs on), and
/// `status.json` being unreadable passing as "no migration needed". The database is
/// unreachable here, so any attempt to use it would show as `database_error`.
///
/// Mutant seen red: the `facts.commit == *release` guard removed.
#[test]
fn a_release_that_is_not_the_staged_one_is_refused_before_the_database_is_asked() {
    let (w, _) = World::with_old();
    let m = DbMigrator::new(unreachable_db(), &w);
    let refused = m.migrate_forward(&c(A)).unwrap_err().0;
    assert!(refused.starts_with("not_staged: "), "{refused}");

    w.st.borrow_mut().staged = None;
    let refused = m.migrate_forward(&c(B)).unwrap_err().0;
    assert!(refused.starts_with("not_staged: "), "{refused}");

    w.st.borrow_mut().staged_err = true;
    let refused = m.migrate_forward(&c(B)).unwrap_err().0;
    assert!(
        refused.starts_with("not_staged: status.json: "),
        "{refused}"
    );
}

/// Catches the credential leaking. An unreachable server is a refusal in fixed words, and
/// neither it nor the database's `Debug` carries the password, the user, the database or
/// the host of the URL.
///
/// Mutants seen red: `PostgresConfig`'s `Debug` made to print its connection settings;
/// the refusal text made to carry the `Debug` of the database.
#[test]
fn an_unreachable_database_is_refused_without_the_url_in_any_message() {
    let (w, _) = World::with_old();
    w.st.borrow_mut().staged_schema = 1;
    let db = unreachable_db();
    let shown = format!("{db:?}");
    let refused = DbMigrator::new(db, &w)
        .with_lock_wait(std::time::Duration::from_millis(50))
        .migrate_forward(&c(B))
        .unwrap_err()
        .0;
    assert!(refused.starts_with("database_error: "), "{refused}");
    for secret in [
        "hunter2_marker",
        "kbf_marker_user",
        "kbf_marker_db",
        "127.0.0.1",
    ] {
        assert!(!refused.contains(secret), "{secret} in {refused}");
        assert!(!shown.contains(secret), "{secret} in {shown}");
    }
}

/// Catches the bounds not reaching the migration: a swap that waits for a lock as long as
/// the day. The default wait handed to the schema code is exactly 30 s, a configured one
/// is passed as it is, and the schema is the staged release's.
///
/// Mutants seen red: `LOCK_WAIT` made 3000 s; `with_lock_wait` ignored.
#[test]
fn the_lock_wait_is_handed_to_the_migration() {
    use kbf_state::postgres::schema::Outcome;
    use std::cell::RefCell;
    use std::time::Duration;

    let (w, _) = World::with_old();
    w.st.borrow_mut().staged_schema = 2;
    let seen = RefCell::new(Vec::new());
    let record = |_: &Database, schema: u32, wait: Duration| {
        seen.borrow_mut().push((schema, wait));
        Ok(Outcome {
            from: None,
            to: schema,
            ran: Vec::new(),
        })
    };
    DbMigrator::new(unreachable_db(), &w)
        .with_apply(Box::new(record))
        .migrate_forward(&c(B))
        .expect("the recorder accepts");
    DbMigrator::new(unreachable_db(), &w)
        .with_lock_wait(Duration::from_secs(7))
        .with_apply(Box::new(record))
        .migrate_forward(&c(B))
        .expect("the recorder accepts");
    assert_eq!(
        *seen.borrow(),
        [(2, Duration::from_secs(30)), (2, Duration::from_secs(7))]
    );
}

/// What `apply` hands a recorder: nothing is migrated, the call is written down.
fn outcome(schema: u32) -> Result<Outcome, SchemaError> {
    Ok(Outcome {
        from: None,
        to: schema,
        ran: Vec::new(),
    })
}

/// Catches a cold start that migrates for a release it knows nothing about, or never asks
/// the installed release what it needs. The installed release (A; the staged one is B) is
/// migrated to the schema it declares, with the 30 s bound; a commit that is neither (C) is
/// refused without asking anyone; and with no probe given, even the installed one is
/// refused as before. `status.json` unreadable or naming no installed release refuses too.
///
/// Mutants seen red: the `installed` comparison dropped (C was asked); the probe called for
/// the staged release too (B asked); the `Some(ask)` guard dropped (A refused).
#[test]
fn the_installed_release_is_migrated_for_the_schema_it_declares() {
    let (w, _) = World::with_old();
    let asked = RefCell::new(Vec::new());
    let applied = RefCell::new(Vec::new());
    let m = DbMigrator::new(unreachable_db(), &w)
        .knowing_installed(|release| {
            asked.borrow_mut().push(release.clone());
            Ok(4)
        })
        .with_apply(Box::new(|_, schema, wait| {
            applied.borrow_mut().push((schema, wait));
            outcome(schema)
        }));
    m.migrate_forward(&c(A)).expect("the installed release");
    assert_eq!(*asked.borrow(), [c(A)]);
    assert_eq!(*applied.borrow(), [(4, Duration::from_secs(30))]);

    // The staged release is known from status.json: nobody is asked.
    w.st.borrow_mut().staged_schema = 2;
    m.migrate_forward(&c(B)).expect("the staged release");
    assert_eq!(
        asked.borrow().len(),
        1,
        "the staged release was asked about"
    );
    assert_eq!(applied.borrow()[1].0, 2);

    // Neither staged nor installed, or no way to tell.
    let why = m.migrate_forward(&c(C)).unwrap_err().0;
    assert!(why.starts_with("not_staged: "), "{why}");
    w.st.borrow_mut().installed = None;
    assert!(
        m.migrate_forward(&c(A))
            .unwrap_err()
            .0
            .starts_with("not_staged: ")
    );
    w.st.borrow_mut().installed = Some(c(A));
    w.st.borrow_mut().installed_err = true;
    assert!(
        m.migrate_forward(&c(A))
            .unwrap_err()
            .0
            .starts_with("not_staged: ")
    );
    assert_eq!((asked.borrow().len(), applied.borrow().len()), (1, 2));

    // Without a probe the installed release stays refused.
    w.st.borrow_mut().installed_err = false;
    let bare = DbMigrator::new(unreachable_db(), &w);
    let why = bare.migrate_forward(&c(A)).unwrap_err().0;
    assert!(why.starts_with("not_staged: "), "{why}");
}

/// Catches an installed release that cannot say what it needs being migrated for anyway:
/// the probe's refusal is the migrator's, verbatim, and the database is not asked.
///
/// Mutant seen red: the probe's error swallowed into a default schema.
#[test]
fn a_release_that_cannot_say_its_schema_is_refused_and_the_database_untouched() {
    let (w, _) = World::with_old();
    let applied = RefCell::new(0);
    let m = DbMigrator::new(unreachable_db(), &w)
        .knowing_installed(|_| Err(MigrateError("schema_unknown: no".into())))
        .with_apply(Box::new(|_, schema, _| {
            *applied.borrow_mut() += 1;
            outcome(schema)
        }));
    assert_eq!(
        m.migrate_forward(&c(A)),
        Err(PortError("schema_unknown: no".into()))
    );
    assert_eq!(*applied.borrow(), 0);
}

const PROGRAM: &str = "/var/lib/kbf-installer/releases/aaaa/kbf";

fn probe(runner: &FakeRunner) -> Result<u32, MigrateError> {
    installed_schema(runner, std::path::Path::new(PROGRAM), &c(A))
}

fn says(stdout: String) -> FakeRunner {
    FakeRunner::new(move |_| Ok(ran(0, &stdout, "")))
}

/// Catches the probe running the wrong thing, or running it with the proxy's environment
/// (the database URL is in no variable, but a release is not given the proxy's), as another
/// user, or without a bound. It runs the release's own binary, `--version --json`, with an
/// empty environment, in `/`, as the caller, for at most 10 s, and reads `state_schema`.
///
/// Mutants seen red: env passed through; `run_as` set; the bound removed; `--json` dropped.
#[test]
fn the_probe_runs_the_release_with_nothing_but_its_flags_and_reads_the_schema() {
    let runner = says(version_json(A, "release"));
    assert_eq!(probe(&runner), Ok(7));
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let spawn = &calls[0];
    assert_eq!(spawn.program, std::path::Path::new(PROGRAM));
    assert_eq!(spawn.args, ["--version", "--json"]);
    assert!(spawn.env.is_empty());
    assert_eq!(spawn.cwd, std::path::Path::new("/"));
    assert_eq!(spawn.run_as, None);
    assert_eq!(spawn.timeout, Duration::from_secs(10));
}

/// Catches each way a release can fail to say its schema being taken for an answer. All are
/// `schema_unknown`: it does not run; times out; exits non-zero; prints something else; is
/// another commit's build; is a debug build; or declares a schema that is not a `u32`.
///
/// Mutants seen red: exit status not looked at; `check` skipped (another commit's schema
/// used); the `u32` conversion saturating.
#[test]
fn every_way_a_release_fails_to_say_its_schema_is_schema_unknown() {
    let runs = |answer: std::io::Result<crate::run::Ran>| {
        let cell = RefCell::new(Some(answer));
        FakeRunner::new(move |_| cell.borrow_mut().take().expect("asked once"))
    };
    let mut timed_out = ran(0, &version_json(A, "release"), "");
    timed_out.timed_out = true;
    let mut signalled = ran(0, "", "");
    signalled.code = None;
    let huge =
        version_json(A, "release").replace("\"state_schema\":7", "\"state_schema\":4294967296");
    let cases = [
        runs(Err(std::io::ErrorKind::NotFound.into())),
        runs(Ok(timed_out)),
        runs(Ok(signalled)),
        runs(Ok(ran(1, &version_json(A, "release"), ""))),
        runs(Ok(ran(0, "not json", ""))),
        runs(Ok(ran(0, &version_json(B, "release"), ""))),
        runs(Ok(ran(0, &version_json(A, "debug"), ""))),
        runs(Ok(ran(0, &huge, ""))),
    ];
    for runner in cases {
        let why = probe(&runner).unwrap_err().0;
        assert!(why.starts_with("schema_unknown: "), "{why}");
        assert!(!why.contains(PROGRAM), "the path is in {why}");
    }
}

/// Catches the swap and the migrator not meeting: the swap hands the DbMigrator the staged
/// release, which asks `status.json`, migrates to what it declares with the bound, and does
/// it before the candidate starts. A refusal ends the update at `migrate` with its word, the
/// old child serving and nothing spawned. (The real schema version of this is in `database`.)
///
/// Mutants seen red: the swap's `migrate_forward` call moved after the spawn; the refusal
/// ignored.
#[test]
fn the_swap_runs_the_db_migrator_before_the_candidate_and_stops_on_its_refusal() {
    let seen = RefCell::new(Vec::new());
    let (w, old) = World::with_old();
    w.st.borrow_mut().staged_schema = 2;
    let m = DbMigrator::new(unreachable_db(), &w).with_apply(Box::new(|_, schema, wait| {
        w.log_event("migrate");
        seen.borrow_mut().push((schema, wait));
        outcome(schema)
    }));
    let ports = Ports {
        migrator: &m,
        ..w.ports()
    };
    let phase = Swap::begin(ports, plan(Mode::Frontend), Some(old))
        .expect("begins")
        .run();
    assert_eq!(phase, Ok(Phase::Done));
    let log = w.log();
    let at = |what: &str| log.iter().position(|l| l.starts_with(what)).expect(what);
    assert!(at("migrate") < at("spawn"), "{log:?}");
    assert_eq!(*seen.borrow(), [(2, Duration::from_secs(30))]);

    let (w, old) = World::with_old();
    let m = DbMigrator::new(unreachable_db(), &w)
        .with_apply(Box::new(|_, _, _| Err(SchemaError::Busy)));
    let ports = Ports {
        migrator: &m,
        ..w.ports()
    };
    let phase = Swap::begin(ports, plan(Mode::Frontend), Some(old))
        .expect("begins")
        .run();
    assert_eq!(phase, Ok(Phase::Failed));
    let failure = w.last_intent().failure.expect("a failure");
    assert_eq!(failure.step, Step::Migrate);
    assert!(
        failure.reason.starts_with("database_busy: "),
        "{}",
        failure.reason
    );
    assert_eq!(w.count("spawn"), 0, "a candidate started on a refusal");
}

/// Real schema, real swap: the tests that need a server.
mod database {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use kbf_state::postgres::schema::{self, Migration};
    use kbf_state::postgres::{PostgresStore, TestDatabase};
    use kbf_state::{Cond, Store, Txn};

    use super::super::model::{Mode, Phase, PortError, StagedFacts, Step};
    use super::super::ports::{Migrator, RootOrders};
    use super::super::swap::Swap;
    use super::*;
    use crate::CommitId;

    /// The test server's URL, which names its own database and role.
    fn url() -> String {
        kbf_testkit::database::server_url()
    }

    /// What of the URL must never appear in a message: its user information.
    fn userinfo() -> Option<String> {
        url()
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .map(|(info, _)| info.to_owned())
    }

    /// `text` does not quote `url`, the URL of the database the migrator was given, nor the
    /// user information or password of the server's.
    fn assert_no_url(url: &str, text: &str) {
        assert!(!text.contains(url), "the URL is in {text}");
        if let Some(info) = userinfo() {
            assert!(!text.contains(&info), "the user information is in {text}");
            if let Some((_, password)) = info.split_once(':') {
                assert!(
                    password.is_empty() || !text.contains(password),
                    "the password is in {text}"
                );
            }
        }
    }

    /// A database made for one test and dropped with it, with no schema: the schema lock, an
    /// advisory lock of the database, is the test's own, so a test that holds it stops no other.
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

        fn rows(&self, query: &str) -> Vec<String> {
            let url = self.url();
            let tables = PostgresConfig::DEFAULT_SCHEMA;
            let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connects");
            client
                .batch_execute(&format!("SET search_path TO {tables}"))
                .ok();
            client
                .query(query, &[])
                .expect("the query runs")
                .iter()
                .map(|r| r.get::<_, String>(0))
                .collect()
        }
    }

    /// What a release brings: 1 the state tables, 2 an added column, 3 a removal (a
    /// contract step that belongs to a later release).
    fn releases() -> Vec<Migration> {
        let mut set = schema::embedded();
        set.push(
            Migration::new(
                2,
                "note",
                "-- kbf-migration: expand\nALTER TABLE kv ADD COLUMN note text;",
            )
            .expect("additive"),
        );
        set.push(
            Migration::new(
                3,
                "drop_note",
                "-- kbf-migration: contract\nALTER TABLE kv DROP COLUMN note;",
            )
            .expect("a contract step lints as one"),
        );
        set
    }

    /// Release 2 with a migration that fails after its first statement.
    fn broken() -> Vec<Migration> {
        let mut set = schema::embedded();
        set.push(
            Migration::new(
                2,
                "broken",
                "-- kbf-migration: expand\nCREATE TABLE half (a int);\nALTER TABLE nosuch ADD COLUMN b int;",
            )
            .expect("lints"),
        );
        set
    }

    /// `status.json` as a value that can cross threads: a release is staged, needing
    /// schema `schema`.
    struct Staged {
        commit: CommitId,
        schema: u32,
    }

    impl RootOrders for Staged {
        fn frontend_update(&self, _: &super::super::model::Locator) -> Result<(), PortError> {
            unreachable!("the migrator reads status.json only")
        }
        fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
            Ok(Some(StagedFacts {
                commit: self.commit.clone(),
                protocol: 7,
                min_protocol: 5,
                state_schema: self.schema,
                profile: None,
            }))
        }
        fn installed(&self) -> Result<Option<CommitId>, PortError> {
            unreachable!("the migrator reads status.json only")
        }
        fn confirm(&self, _: &CommitId) -> Result<(), PortError> {
            unreachable!("the migrator reads status.json only")
        }
        fn revert(&self, _: &CommitId) -> Result<(), PortError> {
            unreachable!("the migrator reads status.json only")
        }
    }

    fn staged(schema: u32) -> Staged {
        Staged {
            commit: c(B),
            schema,
        }
    }

    fn put(store: &PostgresStore, key: &str) {
        store
            .txn(Txn::new().put(key, "v", Cond::Any))
            .expect("reachable")
            .expect("commits");
    }

    fn applied_at(s: &Scratch) -> Vec<String> {
        s.rows(
            "SELECT version::text || ' ' || applied_at::text FROM schema_version ORDER BY version",
        )
    }

    /// A migrator that says "migrate" in the swap's log first, as the fake does, so the
    /// order against "spawn" can be read.
    struct Logged<'a>(&'a World, DbMigrator<'a>);

    impl Migrator for Logged<'_> {
        fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
            self.0.log_event("migrate");
            self.1.migrate_forward(release)
        }
    }

    fn swap(
        w: &World,
        m: &dyn Migrator,
        old: super::super::model::ProcIdent,
    ) -> Result<Phase, super::super::model::Halt> {
        let ports = super::super::ports::Ports {
            migrator: m,
            ..w.ports()
        };
        Swap::begin(ports, plan(Mode::Frontend), Some(old))?.run()
    }

    /// Catches a migrator that does not create the schema on a fresh database, one that
    /// repeats itself, and a repeat that is not a no-op. Fresh: the schema appears at the
    /// release's version and kbf opens on it. Again: nothing runs, the recorded row (and
    /// its time) is the same one.
    ///
    /// Mutants seen red: `migrate_forward` returning `Ok` without migrating; the
    /// `schema_version` INSERT run twice (a second run changed the row).
    #[test]
    fn a_fresh_database_gets_the_schema_and_a_repeat_changes_nothing() {
        let s = Scratch::new();
        let orders = staged(1);
        let m = DbMigrator::new(Database::new(s.config(), releases()), &orders);
        m.migrate_forward(&c(B)).expect("creates");
        assert_eq!(schema::current(&s.config()), Ok(1));
        PostgresStore::open(&s.config()).expect("kbf starts on what the migrator made");
        let before = applied_at(&s);
        assert_eq!(before.len(), 1);
        m.migrate_forward(&c(B)).expect("again");
        assert_eq!(applied_at(&s), before, "a repeat touched the schema");
    }

    /// Catches an additive migration breaking the child that is still serving, and a
    /// migration past what the release asked for. Child N serves on schema 1; the swap
    /// migrates to 2 (a release that needs 2 while 3, a contract step, exists in the
    /// installer); N keeps reading what it wrote and writing; the schema is at 2, not 3.
    ///
    /// Mutant seen red: the target taken as the installer's newest instead of the staged
    /// release's (the contract step was refused for a release that does not need it).
    #[test]
    fn an_additive_migration_leaves_the_serving_child_reading_and_writing() {
        let s = Scratch::new();
        let first = staged(1);
        DbMigrator::new(Database::new(s.config(), releases()), &first)
            .migrate_forward(&c(B))
            .expect("N installed");
        let child = PostgresStore::open(&s.config().with_supported_schema(1, 1)).expect("child N");
        put(&child, "a");

        let second = staged(2);
        DbMigrator::new(Database::new(s.config(), releases()), &second)
            .migrate_forward(&c(B))
            .expect("migrates");
        assert_eq!(schema::current(&s.config()), Ok(2));
        put(&child, "b");
        assert!(child.get("a").expect("reads").is_some());
        assert!(child.get("b").expect("reads").is_some());
        assert_eq!(s.rows("SELECT column_name::text FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'kv' AND column_name = 'note'"), ["note"]);
    }

    /// Catches a release whose migration is not additive being run, or run in part. The
    /// release needs 3, a contract step: the update is refused as `not_additive` and
    /// migration 2, which is additive and pending before it, is not run either.
    ///
    /// Mutant seen red: the contract check removed from `apply` (3 ran: the column was
    /// added and dropped, and the version was 3).
    #[test]
    fn a_release_that_needs_a_contract_step_is_refused_and_nothing_runs() {
        let s = Scratch::new();
        let first = staged(1);
        DbMigrator::new(Database::new(s.config(), releases()), &first)
            .migrate_forward(&c(B))
            .expect("N installed");
        let third = staged(3);
        let refused = DbMigrator::new(Database::new(s.config(), releases()), &third)
            .migrate_forward(&c(B))
            .unwrap_err()
            .0;
        assert!(refused.starts_with("not_additive: "), "{refused}");
        assert!(refused.contains("migration 3"), "{refused}");
        assert_no_url(&s.url(), &refused);
        assert_eq!(schema::current(&s.config()), Ok(1));
    }

    /// Catches a release that needs a schema the installer does not embed being started on
    /// anyway: `installer_too_old`, nothing changed.
    #[test]
    fn a_release_newer_than_the_installer_is_refused_as_installer_too_old() {
        let s = Scratch::new();
        let orders = staged(9);
        let refused = DbMigrator::new(Database::new(s.config(), releases()), &orders)
            .migrate_forward(&c(B))
            .unwrap_err()
            .0;
        assert!(refused.starts_with("installer_too_old: "), "{refused}");
        assert_eq!(
            schema::current(&s.config()),
            Err(SchemaError::NotInitialised)
        );
    }

    /// Catches a migrator that waits for ever on another installer (the swap has nothing
    /// started, but the old child's operators wait on it), and one that runs without the
    /// lock. A second installer holds the schema lock: a short wait is `database_busy` and
    /// creates nothing; a long wait stays blocked until the lock is released, then
    /// applies, and a third migrator (the one that raced) finds nothing to do.
    ///
    /// Mutants seen red: `lock_wait` ignored in `Database::migrate_within` (the short wait
    /// hung until the holder was released: the 5 s receive timed out); the schema lock
    /// statement removed from the migration transaction (the long wait did not block).
    #[test]
    fn a_migrator_waits_for_the_schema_lock_and_gives_up_as_busy() {
        let s = Scratch::new();
        let hold = || {
            let mut other =
                postgres::Client::connect(&s.url(), postgres::NoTls).expect("connects");
            other
                .batch_execute("BEGIN; SELECT pg_advisory_xact_lock(hashtext('kbf-state-schema'))")
                .expect("the lock is free");
            other
        };
        let run = |wait: Duration| {
            let (tx, rx) = mpsc::channel();
            let config = s.config();
            thread::spawn(move || {
                let orders = staged(2);
                let m = DbMigrator::new(Database::new(config, releases()), &orders)
                    .with_lock_wait(wait);
                let _ = tx.send(m.migrate_forward(&c(B)));
            });
            rx
        };

        let mut other = hold();
        let short = run(Duration::from_millis(300)).recv_timeout(Duration::from_secs(5));
        other.batch_execute("ROLLBACK").expect("released");
        let busy = short.expect("it gives up").unwrap_err().0;
        assert!(busy.starts_with("database_busy: "), "{busy}");
        assert_no_url(&s.url(), &busy);
        assert_eq!(
            schema::current(&s.config()),
            Err(SchemaError::NotInitialised),
            "a migration that waited left a schema"
        );

        let mut other = hold();
        let long = run(Duration::from_secs(60));
        assert!(
            long.recv_timeout(Duration::from_millis(500)).is_err(),
            "the migrator ran while another installer held the lock"
        );
        other.batch_execute("ROLLBACK").expect("released");
        long.recv_timeout(Duration::from_secs(30))
            .expect("it runs once the lock is free")
            .expect("and applies");
        assert_eq!(schema::current(&s.config()), Ok(2));
        let orders = staged(2);
        DbMigrator::new(Database::new(s.config(), releases()), &orders)
            .migrate_forward(&c(B))
            .expect("the racing installer finds it done");
        assert_eq!(applied_at(&s).len(), 2);
    }

    /// The test URL with its role or its database replaced by one that does not exist.
    fn url_with(role: Option<&str>, database: Option<&str>) -> String {
        let (scheme, rest) = url()
            .split_once("://")
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .expect("a URL");
        let (host, path) = rest.rsplit_once('/').expect("a database");
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        let at = role.map_or_else(String::new, |r| format!("{r}@"));
        let at = if role.is_none() {
            userinfo().map_or(at, |i| format!("{i}@"))
        } else {
            at
        };
        format!("{scheme}://{at}{host}/{}", database.unwrap_or(path))
    }

    /// Catches a server's refusal of the connection leaking the role or database name
    /// (`FATAL: role "x" does not exist`) into the reason, its `Display` or its `Debug`.
    /// A role that does not exist and a database that does not exist are each refused as
    /// `database_error` with the SQLSTATE and neither name.
    ///
    /// Mutant seen red: `describe_refusal` made to return `describe` (the FATAL text with
    /// both names in it).
    #[test]
    fn a_refused_connection_names_neither_role_nor_database() {
        let (w, _) = World::with_old();
        w.st.borrow_mut().staged_schema = 1;
        for (url, secret) in [
            (
                url_with(Some("kbf_norole_marker"), None),
                "kbf_norole_marker",
            ),
            (url_with(None, Some("kbf_nodb_marker")), "kbf_nodb_marker"),
        ] {
            let config = PostgresConfig::parse(&url).expect("parses");
            let db = Database::new(config, releases());
            let shown = format!("{db:?}");
            let err = db
                .migrate_within(1, std::time::Duration::from_secs(5))
                .unwrap_err();
            let reason = DbMigrator::new(db, &w)
                .migrate_forward(&c(B))
                .unwrap_err()
                .0;
            assert!(reason.starts_with("database_error: "), "{reason}");
            assert!(reason.contains("SQLSTATE"), "{reason}");
            for text in [reason, err.to_string(), format!("{err:?}"), shown] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }

    /// Catches a half-applied migration and a swap that goes on after a failed one. The
    /// release's migration fails on its second statement: the first is rolled back, the
    /// schema stays at 1, the old child still reads and writes, the swap ends `failed` at
    /// step `migrate` with the stable word, spawns nothing, and drops the staged release.
    /// Neither the swap's notes nor the recorded reason carry the URL.
    ///
    /// Mutants seen red: `Refusal::of` mapping `Failed` to `database_error`; the swap
    /// ignoring the migrator's error (the candidate spawned).
    #[test]
    fn a_failed_migration_rolls_back_and_the_swap_refuses_with_the_old_child_serving() {
        let s = Scratch::new();
        let first = staged(1);
        DbMigrator::new(Database::new(s.config(), broken()), &first)
            .migrate_forward(&c(B))
            .expect("N installed");
        let child = PostgresStore::open(&s.config().with_supported_schema(1, 1)).expect("child N");
        put(&child, "a");

        let (w, old) = World::with_old();
        w.st.borrow_mut().staged_schema = 2;
        let m = Logged(&w, DbMigrator::new(Database::new(s.config(), broken()), &w));
        assert_eq!(swap(&w, &m, old), Ok(Phase::Failed));

        let failure = w.last_intent().failure.expect("a failure");
        assert_eq!(failure.step, Step::Migrate);
        assert!(
            failure.reason.starts_with("migration_failed: "),
            "{}",
            failure.reason
        );
        assert!(failure.reason.contains("nosuch"), "{}", failure.reason);
        let own = s.url();
        assert_no_url(&own, &failure.reason);
        for line in w.log() {
            assert_no_url(&own, &line);
        }
        assert_eq!(
            w.count("spawn"),
            0,
            "a candidate started on a failed migration"
        );
        assert_eq!(w.count("order revert"), 1);
        assert_eq!(schema::current(&s.config()), Ok(1));
        assert_eq!(
            s.rows("SELECT count(*)::text FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = 'half'"),
            ["0"],
            "the first statement of the failed migration remained"
        );
        put(&child, "b");
    }

    /// Catches the swap starting its candidate before the schema is ready, and a good
    /// migration not reaching the database: over the same real schema, a release that needs
    /// 2 migrates first, then the candidate starts, and the swap completes.
    ///
    /// Mutant seen red: the swap's `migrate_forward` call moved after the spawn.
    #[test]
    fn the_swap_migrates_before_the_candidate_starts() {
        let s = Scratch::new();
        let first = staged(1);
        DbMigrator::new(Database::new(s.config(), releases()), &first)
            .migrate_forward(&c(B))
            .expect("N installed");

        let (w, old) = World::with_old();
        w.st.borrow_mut().staged_schema = 2;
        let m = Logged(
            &w,
            DbMigrator::new(Database::new(s.config(), releases()), &w),
        );
        assert_eq!(swap(&w, &m, old), Ok(Phase::Done));
        let log = w.log();
        let migrate = log.iter().position(|l| l == "migrate").expect("migrated");
        let spawn = log
            .iter()
            .position(|l| l.starts_with("spawn"))
            .expect("spawned");
        assert!(migrate < spawn, "{log:?}");
        assert_eq!(schema::current(&s.config()), Ok(2));
    }
}
