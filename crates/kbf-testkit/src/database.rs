//! [`TestDatabase`]: a Postgres database a test owns, made on the test server and dropped when
//! the guard is. Feature `postgres`.
//!
//! The server is the one [`URL_VAR`] names, a setting of the test harness and not of the
//! product (CI's service container sets it). A test that needs Postgres fails, and does not
//! skip, when it is not set: [`server_url`] panics with a message that says how to set it.
//! A [`TestDatabase`] is a fresh database with a name of its own (process id, time and a
//! counter), so two tests, in one process or in two, never share rows, tables, schemas or
//! locks (an advisory lock is per database).
//!
//! The database is made empty: this crate has no kbf dependency, so it cannot apply the state
//! schema. `kbf_state::postgres::test_database` makes one and applies it.
//!
//! The calls block on the server, so make the database before entering an async runtime (the
//! blocking client refuses to start inside one).
//!
//! # The rule for shared-catalog DDL
//!
//! Roles, and the databases of the server with their privileges (`CREATE ROLE`, `DROP ROLE`,
//! `ALTER ROLE`, `GRANT ... ON DATABASE`, `CREATE DATABASE`, `DROP DATABASE`), live in catalogs
//! the whole server shares. Two sessions that change one row of them at once fail with
//! "tuple concurrently updated" (SQLSTATE XX000). The test runner puts every test in a process
//! of its own and runs them in parallel against one server, so a lock inside a process cannot
//! keep them apart.
//!
//! Shared-catalog DDL in a test therefore goes through [`CatalogGuard`], which holds a
//! server-wide advisory lock for as long as the DDL (and any product call that does the DDL,
//! such as a provisioning step) takes, and runs the statements with
//! [`batch_execute_retrying`], which retries the one error a client outside the guard can still
//! cause. [`ddl`] is the two in one call for a single statement. A guard is not re-entrant: a
//! thread that holds one does not take another (or call [`ddl`], or make a [`TestDatabase`])
//! until it has dropped the first.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use postgres::error::SqlState;
use postgres::{Client, Config, NoTls};

/// The variable that names the Postgres server the tests may create and drop databases on.
pub const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

/// How long a connection to the server may take: a server that does not answer in this long
/// fails the test (and ends the guard's drop) instead of waiting for the system's own timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The key of the advisory lock a [`CatalogGuard`] holds: one for the whole server, so every
/// test that changes roles or databases, in whatever process, waits for the one before it.
/// The bytes are "kbfcat".
pub const CATALOG_LOCK_KEY: i64 = 0x6b62_6663_6174;

/// The database every guard connects to. An advisory lock belongs to one database of the
/// server, so guards that connected to different ones would not exclude each other.
const LOCK_DATABASE: &str = "postgres";

/// How long a guard waits for the lock: a test that never lets go fails the others with a
/// message, and does not hang them.
const LOCK_WAIT: Duration = Duration::from_secs(300);

/// How many times [`retry_catalog_race`] runs the statements before it gives up.
pub const CATALOG_RACE_TRIES: u32 = 50;

/// How long [`batch_execute_retrying`] waits between two runs.
const CATALOG_RACE_PAUSE: Duration = Duration::from_millis(20);

/// The server is not named: what a failed test says.
struct NoServer;

impl fmt::Display for NoServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{URL_VAR} is not set: this test needs a Postgres server it may create and drop \
             databases on. Start one and set {URL_VAR}=postgres://USER@HOST:PORT/postgres \
             (CI's service container sets it)"
        )
    }
}

/// `expect` prints the error with `Debug`: the message is the whole of the report.
impl fmt::Debug for NoServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// `value`, the contents of [`URL_VAR`] when it is set, as the server's URL.
///
/// # Panics
/// When `value` is `None`: the message names [`URL_VAR`] and says how to set it.
#[must_use]
pub fn server_or_fail(value: Option<String>) -> String {
    value.ok_or(NoServer).expect("a Postgres test server")
}

/// The URL of the test server, from [`URL_VAR`].
///
/// # Panics
/// When [`URL_VAR`] is not set (see [`server_or_fail`]).
#[must_use]
pub fn server_url() -> String {
    server_or_fail(std::env::var(URL_VAR).ok())
}

/// `url` with its database (the path after the host) replaced by `database`; the user,
/// password, host, port and query stay.
///
/// # Panics
/// When `url` is not a `scheme://...` URL.
#[must_use]
pub fn with_database(url: &str, database: &str) -> String {
    let (head, query) = url
        .split_once('?')
        .map_or((url, None), |(h, q)| (h, Some(q)));
    let (scheme, rest) = head.split_once("://").expect("a postgres:// URL");
    let authority = rest.split_once('/').map_or(rest, |(a, _)| a);
    let mut out = format!("{scheme}://{authority}/{database}");
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    out
}

/// A client of the server at `server`, which gives up after [`CONNECT_TIMEOUT`].
///
/// # Panics
/// When `server` is not a Postgres URL.
fn connect(server: &str) -> Result<Client, postgres::Error> {
    let mut config: Config = server.parse().expect("a postgres:// URL");
    config.connect_timeout(CONNECT_TIMEOUT).connect(NoTls)
}

/// `run`, again while it fails with "tuple concurrently updated" (SQLSTATE XX000, which
/// `code_of` reads from the error): that is two sessions changing one row of a shared
/// catalog at once, and the statement that lost has changed nothing. The wait between two runs
/// is `pause`; the last run is the [`CATALOG_RACE_TRIES`]th.
///
/// # Errors
/// The error of `run` at once when its SQLSTATE is anything else (or it has none), and the
/// error of the last run when every run met XX000.
pub fn retry_catalog_race<T, E>(
    pause: Duration,
    code_of: impl Fn(&E) -> Option<&SqlState>,
    mut run: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let mut tries = 1;
    loop {
        let error = match run() {
            Ok(done) => return Ok(done),
            Err(error) => error,
        };
        let raced = code_of(&error) == Some(&SqlState::INTERNAL_ERROR);
        if !raced || tries == CATALOG_RACE_TRIES {
            return Err(error);
        }
        tries += 1;
        thread::sleep(pause);
    }
}

/// Runs `statements` on `client` (several are one transaction, so a run that fails has
/// changed nothing), again when the server answers "tuple concurrently updated". Hold a
/// [`CatalogGuard`] around it: the retry covers only a client that does not.
///
/// # Errors
/// The server's error: at once when it is any other, and after [`CATALOG_RACE_TRIES`] runs
/// when it is that one every time.
pub fn batch_execute_retrying(
    client: &mut Client,
    statements: &str,
) -> Result<(), postgres::Error> {
    let run = || client.batch_execute(statements);
    retry_catalog_race(CATALOG_RACE_PAUSE, postgres::Error::code, run)
}

/// A server-wide lock on changing the shared catalogs, held until the guard is dropped (see
/// the rule at the top of this module). It is a session-level advisory lock on a connection of
/// the guard's own, so it is released when the guard is dropped, and by the server when the
/// connection dies.
pub struct CatalogGuard {
    session: Client,
}

impl CatalogGuard {
    /// Waits for the lock on the test server, then holds it.
    ///
    /// # Panics
    /// When [`URL_VAR`] is not set, the server cannot be reached, or the lock is not free
    /// within five minutes.
    #[must_use]
    pub fn take() -> CatalogGuard {
        CatalogGuard::on(&server_url()).expect("the lock on the shared catalogs")
    }

    /// Waits for the lock on the server at `server`, then holds it.
    ///
    /// # Errors
    /// The server's error when it cannot be reached or the lock is not free within five
    /// minutes.
    ///
    /// # Panics
    /// When `server` is not a Postgres URL.
    pub fn on(server: &str) -> Result<CatalogGuard, postgres::Error> {
        connect(&with_database(server, LOCK_DATABASE)).and_then(|mut session| {
            let wait = LOCK_WAIT.as_secs();
            session
                .batch_execute(&format!(
                    "SET lock_timeout = '{wait}s'; SELECT pg_advisory_lock({CATALOG_LOCK_KEY})"
                ))
                .map(|()| CatalogGuard { session })
        })
    }
}

impl Drop for CatalogGuard {
    /// Lets go of the lock. Best effort: a connection that is gone has let go already, as the
    /// server ends a session's locks with the session.
    fn drop(&mut self) {
        let unlock = format!("SELECT pg_advisory_unlock({CATALOG_LOCK_KEY})");
        let _ = self.session.batch_execute(&unlock);
    }
}

/// Runs `statements` on a connection of its own to the server at `server`, under a
/// [`CatalogGuard`] and with the retry of [`batch_execute_retrying`]: one call for the DDL of a
/// test that holds no guard. Do not call it while holding one (the lock would wait for itself).
///
/// # Errors
/// The server's error when it cannot be reached, the lock is not free in time, or the
/// statements fail.
///
/// # Panics
/// When `server` is not a Postgres URL.
pub fn ddl(server: &str, statements: &str) -> Result<(), postgres::Error> {
    CatalogGuard::on(server).and_then(|_guard| {
        connect(server).and_then(|mut admin| batch_execute_retrying(&mut admin, statements))
    })
}

/// A database made for one test and dropped with it, with the connections still open to it.
pub struct TestDatabase {
    name: String,
    server: String,
}

impl TestDatabase {
    /// A new, empty database on the test server.
    ///
    /// # Panics
    /// When [`URL_VAR`] is not set, or the server cannot be reached or refuses the database.
    #[must_use]
    pub fn empty() -> TestDatabase {
        TestDatabase::on(&server_url())
    }

    /// A new, empty database on the server at `server`, named
    /// `kbf_t_<pid>_<time>_<n>`: no other test, of this process or another, gets the name.
    ///
    /// # Panics
    /// When the server cannot be reached or refuses the database.
    #[must_use]
    pub fn on(server: &str) -> TestDatabase {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = UNIX_EPOCH.elapsed().unwrap_or_default().as_nanos();
        let name = format!("kbf_t_{}_{nanos:x}_{n}", std::process::id());
        ddl(server, &format!("CREATE DATABASE \"{name}\"")).expect("create the test's database");
        TestDatabase {
            name,
            server: server.to_owned(),
        }
    }

    /// The database's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The URL of this database: the server's, with this database in place of its own.
    #[must_use]
    pub fn url(&self) -> String {
        with_database(&self.server, &self.name)
    }
}

impl Drop for TestDatabase {
    /// Drops the database, and the connections still open to it. Best effort: a database left
    /// behind is only clutter on a throwaway server.
    fn drop(&mut self) {
        let statement = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name);
        let _ = ddl(&self.server, &statement);
    }
}
