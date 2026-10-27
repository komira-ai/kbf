//! `kbf boot-check` against a real server, as a frontend unit runs it before the
//! start: a database with no schema is given it and the check passes, one already at this
//! build's schema is left alone, one ahead of this build by an expand step passes too, one with
//! a contract step above this build is refused when the store opens, and a login the server
//! refuses refuses the start (exit 1). Each test makes a database of its own on the server and
//! drops it.
//!
//! The tests run in every test run (none is `#[ignore]`d), and fail, not skip, when
//! `KBF_TEST_POSTGRES_URL` names no server; it names a server where they may create and drop
//! databases. As in the
//! frontend process test, the process is given the URL without its password (the `database`
//! key refuses one), so the server the job runs must trust its loopback clients.

mod cluster_support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use cluster_support::kbf;
use kbf_state::StoreError;
use kbf_state::postgres::schema::{self, Migration, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore};
use kbf_testkit::database::ddl;
use kbf_testkit::dir::TestDir;
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

static NEXT: AtomicU64 = AtomicU64::new(0);

fn admin_url() -> String {
    std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("{URL_VAR} must name a postgres:// URL the tests may use"))
}

/// `admin` with the password of its user information removed, as the `database` key requires,
/// and its database replaced by `database`; `role` replaces the user when it is given.
fn url_of(admin: &str, database: &str, role: Option<&str>) -> String {
    let (scheme, rest) = admin.split_once("://").expect("a URL");
    let (login, place) = rest.split_once('@').expect("user information");
    let admin_user = login.split_once(':').map_or(login, |(name, _)| name);
    let host = place.split_once('/').map_or(place, |(host, _)| host);
    let user = role.unwrap_or(admin_user);
    format!("{scheme}://{user}@{host}/{database}")
}

/// A database on the server no other test uses, with an `installer.toml` that names it; the
/// database is dropped and the file removed with the guard.
struct Database {
    name: String,
    installer_toml: PathBuf,
    _dir: TestDir,
}

impl Database {
    /// A new database, and an `installer.toml` for `test` that names it as `role` (the
    /// server's own when `None`).
    fn new(test: &str, role: Option<&str>) -> Database {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("kbf_bc{}_{n}", std::process::id());
        // A database belongs to the whole server: `ddl` makes it under the shared-catalog guard.
        ddl(&admin_url(), &format!("CREATE DATABASE {name}")).expect("creates the database");
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("boot-check-{test}"));
        let installer_toml = dir.join("installer.toml");
        let url = url_of(&admin_url(), &name, role);
        std::fs::write(&installer_toml, format!("database = \"{url}\"\n")).expect("written");
        Database {
            name,
            installer_toml,
            _dir: dir,
        }
    }

    /// The configuration of the database as the server's own role reads it.
    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&url_of(&admin_url(), &self.name, None)).expect("the URL parses")
    }

    /// A session of its own on the database.
    fn client(&self) -> Client {
        Client::connect(&url_of(&admin_url(), &self.name, None), NoTls).expect("connects")
    }

    /// The rows of `schema_version`, with the time each was applied.
    fn versions(&self) -> Vec<(i32, String)> {
        let query = "SELECT version, applied_at::text \
                     FROM kbf_state.schema_version ORDER BY version";
        let mut client = self.client();
        let rows = client.query(query, &[]).expect("the table is there");
        rows.iter().map(|r| (r.get(0), r.get(1))).collect()
    }

    /// `kbf boot-check --mode=<mode>` over this database: the exit status and standard error.
    fn boot_check(&self, mode: &str) -> (Option<i32>, String) {
        let mode = format!("--mode={mode}");
        let config = format!("--installer-config={}", self.installer_toml.display());
        let (code, _, err) = kbf(&["boot-check", mode.as_str(), config.as_str()]);
        (code, err)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let statement = format!("DROP DATABASE IF EXISTS {}", self.name);
        let _ = ddl(&admin_url(), &statement);
    }
}

/// Asserts that a frontend unit on a database with no schema passes the check,
/// which says it created the schema at this build's version, and that a store of this build
/// then opens on it: the unit starts on the schema it needs.
#[test]
fn a_database_with_no_schema_is_given_it_and_the_unit_starts() {
    let latest = kbf_pack_index::schema::latest();
    let created = format!("created the schema at version {latest}");
    let db = Database::new("fresh", None);
    let (code, err) = db.boot_check("frontend");
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains(&created), "{err}");
    assert_eq!(
        schema::current(&db.config()),
        Ok(kbf_pack_index::schema::latest())
    );
    PostgresStore::open(&db.config()).expect("a store opens on what the check made");
}

/// Asserts that a database already at this build's schema is left alone: the check passes and
/// says there is nothing to apply, and the rows of `schema_version` are the same, with the
/// time they were applied, before and after.
#[test]
fn a_database_at_the_schema_of_this_build_is_left_alone() {
    let db = Database::new("current", None);
    schema::apply(
        &db.config(),
        &kbf_pack_index::schema::embedded(),
        kbf_pack_index::schema::latest(),
    )
    .expect("installed");
    let before = db.versions();
    assert!(!before.is_empty());
    let (code, err) = db.boot_check("frontend");
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains("nothing to apply"), "{err}");
    assert_eq!(db.versions(), before);
}

/// Asserts that a database ahead of this build by an expand step (a later release migrated it,
/// and this release was installed again) passes the check and is left as it was: exit 0, the
/// line says nothing was applied and names no installer package, the schema is still the later
/// one, and a store of this build opens on it.
#[test]
fn a_database_ahead_by_an_expand_step_passes_the_check_and_is_left_alone() {
    let db = Database::new("ahead", None);
    let n1 = kbf_pack_index::schema::latest() + 1;
    let later = Migration::new(
        n1,
        "note",
        "-- kbf-migration: expand\nALTER TABLE kv ADD COLUMN note text;",
    )
    .expect("an additive migration lints");
    let mut set = kbf_pack_index::schema::embedded();
    set.push(later);
    schema::apply(&db.config(), &set, n1).expect("the later release is installed");
    let (code, err) = db.boot_check("frontend");
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains("nothing to apply"), "{err}");
    assert!(!err.contains("installer"), "{err}");
    assert_eq!(schema::current(&db.config()), Ok(n1));
    PostgresStore::open(&db.config()).expect("this build serves what an expand step added");
}

/// Asserts that a database a later release changed incompatibly (a contract step above this
/// build) still does not start: the check leaves it as it was, and the store of this build
/// refuses to open on it with the line that names the release to run.
#[test]
fn a_contract_step_above_this_build_is_refused_when_the_store_opens() {
    let db = Database::new("contract", None);
    let n1 = kbf_pack_index::schema::latest() + 1;
    schema::apply(
        &db.config(),
        &kbf_pack_index::schema::embedded(),
        kbf_pack_index::schema::latest(),
    )
    .expect("installed");
    let row = format!(
        "INSERT INTO kbf_state.schema_version (version, name, kind) \
         VALUES ({n1}, 'later', 'contract')"
    );
    db.client()
        .batch_execute(&row)
        .expect("the later release's row");
    let (code, err) = db.boot_check("frontend");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(schema::current(&db.config()), Ok(n1));
    match PostgresStore::open(&db.config()) {
        Err(StoreError::Failed(why)) => assert!(why.contains("matches the database"), "{why}"),
        Err(other) => panic!("not a failure: {other:?}"),
        Ok(_) => panic!("the store opened"),
    }
}

/// Asserts that a role the server does not know refuses the start without naming the role: exit
/// 1, the reason reduced to the server's SQLSTATE, and no schema made.
#[test]
fn a_login_the_server_refuses_refuses_the_start_without_naming_the_role() {
    let role = "kbf_no_such_role_zz";
    let db = Database::new("login", Some(role));
    let (code, err) = db.boot_check("frontend");
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("kbf boot-check: state schema: "), "{err}");
    assert!(!err.contains(role), "{err}");
    let found = schema::current(&db.config());
    assert_eq!(found, Err(SchemaError::NotInitialised));
}
