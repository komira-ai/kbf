//! The swap's database migrator ([`super::migrator`]): its refusal words and its stored
//! facts without a server, and the swap's whole path over a real schema in `database`
//! (`KBF_TEST_POSTGRES_URL`, tests `#[ignore]`d like the state crate's; the `state-postgres`
//! job runs them, and its filter `database::` finds them by their module name).

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema::{self, SchemaError};

use super::fakes::*;
use super::migrator::{DbMigrator, Refusal};
use super::ports::Migrator;
use crate::database::Database;
use crate::testkit::{A, B, c};

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

/// Real schema, real swap: the tests that need a server.
mod database {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, PoisonError, mpsc};
    use std::thread;
    use std::time::Duration;

    use kbf_state::postgres::PostgresStore;
    use kbf_state::postgres::schema::{self, Migration};
    use kbf_state::{Cond, Store, Txn};

    use super::super::fakes::*;
    use super::super::migrator::DbMigrator;
    use super::super::model::{Mode, Phase, PortError, StagedFacts, Step};
    use super::super::ports::{Migrator, RootOrders};
    use super::super::swap::Swap;
    use super::*;
    use crate::CommitId;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// The schema lock is one per database, whatever the schema: the tests that hold it, or
    /// that a held lock would turn into `database_busy`, take turns (process-wide, #1168).
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn url() -> String {
        std::env::var("KBF_TEST_POSTGRES_URL").expect("KBF_TEST_POSTGRES_URL names a test database")
    }

    /// What of the URL must never appear in a message: its user information.
    fn userinfo() -> Option<String> {
        url()
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('@'))
            .map(|(info, _)| info.to_owned())
    }

    fn assert_no_url(text: &str) {
        assert!(!text.contains(&url()), "the URL is in {text}");
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

    struct Scratch(String);

    impl Scratch {
        fn new() -> Scratch {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            Scratch(format!("kbf_m{}_{n}", std::process::id()))
        }

        fn config(&self) -> PostgresConfig {
            PostgresConfig::parse(&url())
                .and_then(|c| c.with_schema(&self.0))
                .expect("parses")
        }

        fn rows(&self, query: &str) -> Vec<String> {
            let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connects");
            client
                .batch_execute(&format!("SET search_path TO {}", self.0))
                .ok();
            client
                .query(query, &[])
                .expect("the query runs")
                .iter()
                .map(|r| r.get::<_, String>(0))
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Ok(mut c) = postgres::Client::connect(&url(), postgres::NoTls) {
                let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.0));
            }
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
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_fresh_database_gets_the_schema_and_a_repeat_changes_nothing() {
        let _turn = turn();
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
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn an_additive_migration_leaves_the_serving_child_reading_and_writing() {
        let _turn = turn();
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
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_release_that_needs_a_contract_step_is_refused_and_nothing_runs() {
        let _turn = turn();
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
        assert_no_url(&refused);
        assert_eq!(schema::current(&s.config()), Ok(1));
    }

    /// Catches a release that needs a schema the installer does not embed being started on
    /// anyway: `installer_too_old`, nothing changed.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_release_newer_than_the_installer_is_refused_as_installer_too_old() {
        let _turn = turn();
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
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_migrator_waits_for_the_schema_lock_and_gives_up_as_busy() {
        let _turn = turn();
        let s = Scratch::new();
        let hold = || {
            let mut other = postgres::Client::connect(&url(), postgres::NoTls).expect("connects");
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
        assert_no_url(&busy);
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

    /// Catches a half-applied migration and a swap that goes on after a failed one. The
    /// release's migration fails on its second statement: the first is rolled back, the
    /// schema stays at 1, the old child still reads and writes, the swap ends `failed` at
    /// step `migrate` with the stable word, spawns nothing, and drops the staged release.
    /// Neither the swap's notes nor the recorded reason carry the URL.
    ///
    /// Mutants seen red: `Refusal::of` mapping `Failed` to `database_error`; the swap
    /// ignoring the migrator's error (the candidate spawned).
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn a_failed_migration_rolls_back_and_the_swap_refuses_with_the_old_child_serving() {
        let _turn = turn();
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
        assert_no_url(&failure.reason);
        for line in w.log() {
            assert_no_url(&line);
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
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn the_swap_migrates_before_the_candidate_starts() {
        let _turn = turn();
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
