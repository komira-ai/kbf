//! `TestDatabase` against the test server (`KBF_TEST_POSTGRES_URL`), and the parts of it that
//! need no server.
#![cfg(feature = "postgres")]

use std::process::Command;
use std::time::Duration;

use kbf_testkit::database::{
    CATALOG_RACE_TRIES, URL_VAR, retry_catalog_race, server_or_fail, server_url, with_database,
};
use postgres::error::SqlState;

/// Asserts that a test that needs the server fails, and does not skip, when none is named,
/// and that the failure names the variable and says what to set it to.
#[test]
fn a_missing_server_fails_the_test_with_how_to_set_it() {
    let failure = std::panic::catch_unwind(|| server_or_fail(None))
        .expect_err("a missing server fails the test");
    let said = failure
        .downcast_ref::<String>()
        .expect("a formatted message");
    assert!(said.contains(URL_VAR), "{said}");
    assert!(said.contains("postgres://"), "{said}");
}

/// Asserts that a named server is used as it is.
#[test]
fn a_named_server_is_used_as_it_is() {
    let url = "postgres://u:p@h:5432/postgres";
    assert_eq!(server_or_fail(Some(url.to_owned())), url);
}

/// Asserts that only the database of a URL is replaced: the user, password, host, port and
/// query stay.
#[test]
fn with_database_replaces_only_the_database() {
    for (from, to) in [
        ("postgres://u:p@h:5432/postgres", "postgres://u:p@h:5432/d"),
        (
            "postgres://u@h/x?sslmode=disable",
            "postgres://u@h/d?sslmode=disable",
        ),
        ("postgres://u@h", "postgres://u@h/d"),
    ] {
        assert_eq!(with_database(from, "d"), to);
    }
}

/// A stand-in for the server's error: the SQLSTATE it carries, if any.
#[derive(Debug, PartialEq)]
struct Failure(Option<SqlState>);

fn code_of(failure: &Failure) -> Option<&SqlState> {
    failure.0.as_ref()
}

/// The error two sessions changing one catalog row at once get.
fn raced() -> Failure {
    Failure(Some(SqlState::INTERNAL_ERROR))
}

/// Asserts that a run that succeeds is not repeated, and gives its value.
#[test]
fn a_run_that_succeeds_is_not_repeated() {
    let mut runs = 0;
    let result = retry_catalog_race(Duration::ZERO, code_of, || {
        runs += 1;
        Ok::<_, Failure>("done")
    });
    assert_eq!(result, Ok("done"));
    assert_eq!(runs, 1);
}

/// Asserts that "tuple concurrently updated" (XX000) is retried: two runs that meet it and a
/// third that succeeds give the third's value, after exactly three runs.
#[test]
fn a_tuple_concurrently_updated_is_retried_until_the_run_succeeds() {
    let mut script = vec![Err(raced()), Err(raced()), Ok(7)].into_iter();
    let mut runs = 0;
    let result = retry_catalog_race(Duration::ZERO, code_of, || {
        runs += 1;
        script.next().expect("no more runs than the script has")
    });
    assert_eq!(result, Ok(7));
    assert_eq!(runs, 3);
}

/// Asserts that any other error is returned at once, not retried: an error with another
/// SQLSTATE (a unique violation) and one with none each end the call after one run, with that
/// error, though a success would have followed.
#[test]
fn any_other_error_is_returned_at_once() {
    for other in [Some(SqlState::UNIQUE_VIOLATION), None] {
        let mut script = vec![Err(Failure(other.clone())), Ok(1)].into_iter();
        let mut runs = 0;
        let result = retry_catalog_race(Duration::ZERO, code_of, || {
            runs += 1;
            script.next().expect("no more runs than the script has")
        });
        assert_eq!(result, Err(Failure(other)));
        assert_eq!(runs, 1);
    }
}

/// Asserts that the retry is bounded: an error that is XX000 every time ends the call with
/// that error after exactly `CATALOG_RACE_TRIES` runs.
#[test]
fn a_tuple_concurrently_updated_every_time_gives_up_after_the_bound() {
    let mut runs = 0;
    let result = retry_catalog_race(Duration::ZERO, code_of, || {
        runs += 1;
        Err::<(), _>(raced())
    });
    assert_eq!(result, Err(raced()));
    assert_eq!(runs, CATALOG_RACE_TRIES);
}

/// This test binary, to be run again as a child that runs only `asks_for_server`.
fn child() -> Command {
    let mut child = Command::new(std::env::current_exe().expect("this binary"));
    child.args(["--ignored", "--exact", "asks_for_server", "--nocapture"]);
    child
}

/// Asserts that the server is named: `server_url` returns. It is the body of the children of
/// the test below, which run it with the variable unset and set; on its own it needs the
/// variable, and the test below is the one that runs it.
#[test]
#[ignore = "run as a child by server_url_reads_the_variable_and_fails_without_it"]
fn asks_for_server() {
    let _server = server_url();
}

/// Asserts that `server_url` reads `KBF_TEST_POSTGRES_URL` and nothing else: a child of this
/// binary that asks for the server with the variable unset fails, and its message names the
/// variable and says what to set it to, and with the variable set it succeeds.
#[test]
fn server_url_reads_the_variable_and_fails_without_it() {
    let unset = child().env_remove(URL_VAR).output().expect("runs");
    assert!(!unset.status.success());
    let said = String::from_utf8_lossy(&unset.stderr);
    assert!(said.contains(URL_VAR), "{said}");
    assert!(said.contains("postgres://"), "{said}");
    let set = child()
        .env(URL_VAR, "postgres://u@h/postgres")
        .output()
        .expect("runs");
    let told = String::from_utf8_lossy(&set.stderr);
    assert!(set.status.success(), "{told}");
}

/// The tests that need a Postgres server. The module is named `database` so the macOS job,
/// which has none, skips them by that name (`--skip database::`).
mod database {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    use kbf_testkit::database::{CATALOG_LOCK_KEY, CatalogGuard, TestDatabase, server_url};
    use postgres::{Client, NoTls};

    /// Makes a table `rows` in `db` and writes `marker` to it.
    fn write(db: &TestDatabase, marker: &str) {
        let mut client = Client::connect(&db.url(), NoTls).expect("connects");
        client
            .batch_execute("CREATE TABLE rows (marker text)")
            .expect("creates");
        client
            .execute("INSERT INTO rows VALUES ($1)", &[&marker])
            .expect("writes");
    }

    /// What the table `rows` of `db` holds.
    fn read(db: &TestDatabase) -> Vec<String> {
        let mut client = Client::connect(&db.url(), NoTls).expect("connects");
        let rows = client
            .query("SELECT marker FROM rows", &[])
            .expect("reads back");
        rows.iter().map(|row| row.get(0)).collect()
    }

    /// Whether the server has a database called `name`.
    fn exists(name: &str) -> bool {
        let mut client = Client::connect(&server_url(), NoTls).expect("connects");
        let found = client
            .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])
            .expect("catalog");
        !found.is_empty()
    }

    /// Asserts that two databases made at the same moment, by two threads, are different
    /// databases that never see each other's rows: both get a table of the same name and a
    /// row of their own, and each reads back only its own.
    #[test]
    fn two_tests_in_parallel_get_different_databases_and_never_see_each_others_rows() {
        let (one, two) = thread::scope(|scope| {
            let one = scope.spawn(TestDatabase::empty);
            let two = scope.spawn(TestDatabase::empty);
            (one.join().expect("one"), two.join().expect("two"))
        });
        assert_ne!(one.name(), two.name());
        write(&one, "one");
        write(&two, "two");
        assert_eq!(read(&one), ["one"]);
        assert_eq!(read(&two), ["two"]);
    }

    /// Asserts that the database exists while its guard is alive and is gone once the guard
    /// is dropped, even with a connection to it still open.
    #[test]
    fn the_database_is_dropped_with_the_guard() {
        let db = TestDatabase::empty();
        let name = db.name().to_owned();
        let _open = Client::connect(&db.url(), NoTls).expect("connects");
        assert!(exists(&name), "the database was not made");
        drop(db);
        assert!(!exists(&name), "the database outlived its guard");
    }

    /// Asserts that the URL of a database reaches that database: a client that connects with
    /// it is in the database of the guard's name, and not in the one the server's URL names.
    #[test]
    fn the_url_of_a_database_reaches_it() {
        let db = TestDatabase::empty();
        let mut client = Client::connect(&db.url(), NoTls).expect("connects");
        let row = client
            .query_one("SELECT current_database()", &[])
            .expect("asks");
        assert_eq!(row.get::<_, String>(0), db.name());
    }

    /// Asserts that role and database-privilege DDL from many connections at once, each under
    /// the guard, never fails: 8 threads, each on a connection of its own, create a role of
    /// their own, grant it a privilege on the test database and drop it, 25 times. The
    /// statements run without the retry, so a "tuple concurrently updated" (SQLSTATE XX000)
    /// fails the test; without the guard the threads meet it.
    #[test]
    fn role_ddl_from_many_connections_under_the_guard_never_meets_a_catalog_race() {
        thread::scope(|scope| {
            for thread_number in 0..8 {
                scope.spawn(move || {
                    let mut admin = Client::connect(&server_url(), NoTls).expect("connects");
                    let database: String = admin
                        .query_one("SELECT current_database()", &[])
                        .expect("asks")
                        .get(0);
                    let role = format!("kbf_tk_ddl_{}_{thread_number}", std::process::id());
                    for round in 0..25 {
                        let _guard = CatalogGuard::take();
                        admin
                            .batch_execute(&format!(
                                "CREATE ROLE {role} LOGIN; \
                                 GRANT CREATE ON DATABASE \"{database}\" TO {role}"
                            ))
                            .unwrap_or_else(|error| panic!("round {round}, create: {error}"));
                        admin
                            .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
                            .unwrap_or_else(|error| panic!("round {round}, drop: {error}"));
                    }
                });
            }
        });
    }

    /// Asserts that two guards are never held at once, whatever connection each is on: 4
    /// threads take the guard 5 times each and count how many are inside while they hold it,
    /// which must be only themselves; and every guard is let go of when it is dropped, or the
    /// next thread would wait for the lock until it gave up.
    #[test]
    fn two_guards_are_never_held_at_once() {
        let inside = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..5 {
                        let _guard = CatalogGuard::take();
                        let before = inside.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(before, 0, "another guard is held at the same time");
                        thread::sleep(Duration::from_millis(5));
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
    }

    /// Asserts that the lock is let go of when the connection that holds it dies: the holder's
    /// backend is terminated from another connection (it is found by the lock it holds, which
    /// no one else can hold at the time), and a second guard is then taken, which returns only
    /// when the lock is free.
    #[test]
    fn the_lock_is_released_when_the_holders_connection_dies() {
        let held = CatalogGuard::take();
        let mut admin = Client::connect(&server_url(), NoTls).expect("connects");
        let holder: i32 = admin
            .query_one(
                "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
                 AND ((classid::bigint << 32) | objid::bigint) = $1",
                &[&CATALOG_LOCK_KEY],
            )
            .expect("the lock is held by one backend")
            .get(0);
        admin
            .execute("SELECT pg_terminate_backend($1)", &[&holder])
            .expect("terminates the holder");
        let _second = CatalogGuard::take();
        drop(held);
    }
}
