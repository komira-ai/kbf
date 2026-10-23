//! `TestDatabase` against the test server (`KBF_TEST_POSTGRES_URL`), and the parts of it that
//! need no server.
#![cfg(feature = "postgres")]

use std::process::Command;

use kbf_testkit::database::{URL_VAR, server_or_fail, server_url, with_database};

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
    use std::thread;

    use kbf_testkit::database::{TestDatabase, server_url};
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
}
