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

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use postgres::{Client, Config, NoTls};

/// The variable that names the Postgres server the tests may create and drop databases on.
pub const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

/// How long a connection to the server may take: a server that does not answer in this long
/// fails the test (and ends the guard's drop) instead of waiting for the system's own timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
        connect(server)
            .expect("the test server answers at the URL given")
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .expect("create the test's database");
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
        let _ = connect(&self.server).and_then(|mut admin| admin.batch_execute(&statement));
    }
}
