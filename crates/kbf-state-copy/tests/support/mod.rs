//! What the test files share: state files, a database for each Postgres test, and ways to
//! run the tool and read what it did.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, DirEntry, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_state::postgres::{PostgresConfig, schema};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Commit, Cond, ManualClock, Store, Txn};
use kbf_state_copy::digest::{Counter, Entry};
use kbf_state_copy::{Args, CutoverArgs, Error, PlaceArgs, RollbackArgs, Seams};
use kbf_testkit::database::ddl;
use kbf_testkit::dir::TestDir;
use rusqlite::Connection;

/// The key families of the golden file and their row counts, as its `.rows` file lists them.
pub const GOLDEN_FAMILIES: [(&str, u64); 9] = [
    ("cordon/", 1),
    ("done/", 1),
    ("fate/", 1),
    ("lease/", 1),
    ("loss/", 1),
    ("meta/", 4),
    ("op/", 2),
    ("queue/", 1),
    ("session/", 2),
];

/// The head revision of the golden file: the cursor its `.rows` file ends with.
pub const GOLDEN_HEAD: i64 = 36;

/// The highest version a row of the golden file holds.
pub const GOLDEN_HIGHEST: i64 = 36;

/// A scratch directory for one test, removed when it is dropped.
pub fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

/// The state file the repository keeps as the golden of schema version 1.
pub fn golden() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../kbf-server/tests/golden/state/v1.db")
}

/// A copy of the golden file in `dir`, named `state.db`, with no lock file beside it.
pub fn golden_copy(dir: &Path) -> PathBuf {
    let copy = dir.join("state.db");
    fs::copy(golden(), &copy).expect("copy the golden file");
    copy
}

/// The key/value rows of the golden file as its `.rows` file lists them, with expiry in
/// microseconds: read from the text, not from the file, so it is independent of the reader.
pub fn golden_rows() -> Vec<Entry> {
    let text = fs::read_to_string(golden().with_extension("rows")).expect("the rows file");
    text.lines()
        .filter_map(|line| line.strip_prefix("kv "))
        .map(rows_entry)
        .collect()
}

fn rows_entry(line: &str) -> Entry {
    let mut parts = line.split(' ');
    let key = parts.next().expect("a key");
    let version = field(parts.next(), "version=");
    let expires = field(parts.next(), "expires_ns=");
    let value = field(parts.next(), "value=");
    Entry {
        key: key.to_owned(),
        value: hex::decode(value).expect("hex"),
        version: version.parse().expect("a number"),
        expires_us: (expires != "-").then(|| expires.parse::<i64>().expect("a number") / 1000),
    }
}

fn field<'a>(part: Option<&'a str>, name: &str) -> &'a str {
    part.and_then(|text| text.strip_prefix(name))
        .expect("the field is there")
}

/// Asserts that `text` contains `needle`.
pub fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} is not in {text:?}");
}

/// Asserts that `text` does not contain `needle`.
pub fn lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "{needle:?} is in {text:?}");
}

/// Asserts that `text` starts with `prefix`.
pub fn starts(text: &str, prefix: &str) {
    assert!(text.starts_with(prefix), "{text:?} lacks {prefix:?}");
}

/// A counter.
pub fn counter(name: &str, value: i64) -> Counter {
    Counter {
        name: name.to_owned(),
        value,
    }
}

/// Lowers the head revision a state file records to 0, as a file whose head is below its
/// highest version would be.
pub fn lower_head(db: &Path) {
    let conn = Connection::open(db).expect("open the state file");
    conn.execute("UPDATE meta SET value = 0 WHERE name = 'head'", [])
        .expect("lower the head");
    drop(conn);
    settle(db);
}

/// The names in `dir`.
pub fn names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .expect("list the directory")
        .map(name_of)
        .collect()
}

fn name_of(entry: io::Result<DirEntry>) -> String {
    let entry = entry.expect("a directory entry");
    entry.file_name().to_string_lossy().into_owned()
}

/// Opens (or creates) a SQLite state file the way a frontend does, on a clock that stays
/// at zero, so every expiry is its time to live.
pub fn open(path: &Path) -> SqliteStore {
    SqliteStore::open(path, Arc::new(ManualClock::default()), Options::default())
        .expect("a SQLite state store")
}

/// Folds the write-ahead log into the file, so that the file alone holds every commit.
pub fn settle(db: &Path) {
    let conn = Connection::open(db).expect("open the state file");
    let sql = "PRAGMA wal_checkpoint(TRUNCATE)";
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .expect("checkpoint");
}

fn commit(store: &SqliteStore, txn: Txn) -> Commit {
    store
        .txn(txn)
        .expect("the store works")
        .expect("the transaction commits")
}

fn version_of(commit: &Commit) -> i64 {
    i64::try_from(commit.revision.0).expect("a small revision")
}

fn counter_of((name, value): (String, i64)) -> Counter {
    Counter { name, value }
}

/// A state file written through the SQLite store, and what was written to it.
pub struct Built {
    pub db: PathBuf,
    pub entries: Vec<Entry>,
    pub counters: Vec<Counter>,
    pub head: i64,
}

/// What the writes of [`built`] have done so far, as the file should hold it.
struct Log<'a> {
    store: &'a SqliteStore,
    entries: BTreeMap<String, Entry>,
    counters: BTreeMap<String, i64>,
    head: i64,
}

impl Log<'_> {
    fn put(&mut self, key: &str, value: &[u8]) {
        let txn = Txn::new().put(key, value, Cond::Any);
        let version = version_of(&commit(self.store, txn));
        self.remember(key, value, version, None);
    }

    fn put_ttl(&mut self, key: &str, value: &[u8], ttl: Duration) {
        let txn = Txn::new().put_ttl(key, value, ttl, Cond::Any);
        let version = version_of(&commit(self.store, txn));
        let expires_us = i64::try_from(ttl.as_micros()).expect("a small time");
        self.remember(key, value, version, Some(expires_us));
    }

    fn remember(&mut self, key: &str, value: &[u8], version: i64, expires_us: Option<i64>) {
        let entry = Entry {
            key: key.to_owned(),
            value: value.to_vec(),
            version,
            expires_us,
        };
        self.entries.insert(key.to_owned(), entry);
        self.head = version;
    }

    fn delete(&mut self, key: &str) {
        let txn = Txn::new().delete(key, Cond::Any);
        self.head = version_of(&commit(self.store, txn));
        self.entries.remove(key);
    }

    fn next(&mut self, counter: &str) {
        let made = commit(self.store, Txn::new().next(counter));
        let value = i64::try_from(made.counters[0]).expect("a small count");
        self.counters.insert(counter.to_owned(), value);
    }
}

/// Writes a state file in `dir` with rows in several families: a row written twice, a
/// binary value, an empty value, a key with no slash, a key with a non-ASCII family, a
/// row with a time to live of 90 s and one of 1500 ns (not a whole number of microseconds),
/// a row that was deleted, and counters in two families. The store is closed and its log
/// folded in, so the file alone holds all of it.
pub fn built(dir: &Path) -> Built {
    let db = dir.join("state.db");
    let store = open(&db);
    let mut log = Log {
        store: &store,
        entries: BTreeMap::new(),
        counters: BTreeMap::new(),
        head: 0,
    };
    log.put("op/0001", b"queued");
    log.put("op/0002", b"queued");
    log.put("op/0001", b"running");
    log.put("lease/a", &[0, 255, 128]);
    log.put("queue/0001", b"");
    log.put("plain", b"no slash");
    log.put("caf\u{e9}/cl\u{e9}", b"a family that is not ASCII");
    log.put_ttl("member/n1", b"node one", Duration::from_secs(90));
    log.put_ttl("member/n2", b"node two", Duration::from_nanos(1_500));
    log.put("gone/1", b"soon deleted");
    log.delete("gone/1");
    log.next("seq/ops");
    log.next("seq/ops");
    log.next("seq/ops");
    log.next("ids/x");
    let head = log.head;
    let entries = log.entries.into_values().collect();
    let counters = log.counters.into_iter().map(counter_of).collect();
    drop(store);
    settle(&db);
    Built {
        db,
        entries,
        counters,
        head,
    }
}

/// A state file in `dir` with `rows` key/value rows in one family and `counters` counters,
/// each set written in one transaction: more of each than one INSERT of the tool carries.
pub fn bulk(dir: &Path, rows: usize, counters: usize) -> PathBuf {
    let db = dir.join("state.db");
    let store = open(&db);
    let mut puts = Txn::new();
    for i in 0..rows {
        let byte = u8::try_from(i % 251).expect("a small number");
        puts = puts.put(format!("bulk/{i:05}"), vec![byte; 3], Cond::Any);
    }
    commit(&store, puts);
    let mut counts = Txn::new();
    for i in 0..counters {
        counts = counts.next(format!("count/{i:04}"));
    }
    commit(&store, counts);
    drop(store);
    settle(&db);
    db
}

/// The arguments of a run.
pub fn args(db: &Path, url: &str, apply: bool) -> Args {
    Args {
        state_db: db.to_path_buf(),
        postgres_url: url.to_owned(),
        apply,
    }
}

/// A run through the library, with the report it printed.
pub fn run_with(args: &Args, seams: &Seams) -> (Result<(), Error>, String) {
    let mut out: Vec<u8> = Vec::new();
    let result = kbf_state_copy::run(args, seams, &mut out);
    (result, String::from_utf8(out).expect("the report is UTF-8"))
}

/// What a run of the command line did.
pub struct Ran {
    pub code: u8,
    pub out: String,
    pub err: String,
}

/// A run of the command line, in this process.
pub fn run_main(args: &[&str]) -> Ran {
    let program = std::iter::once("kbf-state-copy");
    let argv = program.chain(args.iter().copied()).map(OsString::from);
    let (mut out, mut err): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    let code = kbf_state_copy::main_with(argv, &mut out, &mut err);
    Ran {
        code,
        out: String::from_utf8(out).expect("the output is UTF-8"),
        err: String::from_utf8(err).expect("the errors are UTF-8"),
    }
}

/// A run of the command line with the places a test reaches into passed in.
pub fn run_main_with(seams: &Seams, args: &[&str]) -> Ran {
    let program = std::iter::once("kbf-state-copy");
    let argv = program.chain(args.iter().copied()).map(OsString::from);
    let (mut out, mut err): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    let code = kbf_state_copy::main_with_seams(argv, seams, &mut out, &mut err);
    Ran {
        code,
        out: String::from_utf8(out).expect("the output is UTF-8"),
        err: String::from_utf8(err).expect("the errors are UTF-8"),
    }
}

/// The URL a frontend reaches its database by in the runbook: no password, a local socket.
pub const FRONTEND_URL: &str = "postgres:///kbf?host=/run/postgresql&user=kbf";

/// A kbf.toml whose `[scheduler] state-store` is `value`, with comments, tabs, trailing
/// spaces, a line ending in CR LF and no final newline around it: text that a rewrite must
/// leave as it is, byte for byte.
pub fn config_with(value: &str) -> String {
    format!(
        "# the frontend of this farm\n\
         mode = \"frontend\"\r\n\
         \n\
         [listener]\n\
         \tlisten = \"0.0.0.0:8980\"   \n\
         \n\
         [scheduler]\n\
         # where the scheduler keeps its state\n\
         state-store = \"{value}\"   # moved by the cutover\n\
         heartbeat-interval-ms = 500"
    )
}

/// A kbf.toml whose state store is the SQLite file `db`.
pub fn config_for(db: &Path) -> String {
    config_with(&sqlite_url(db))
}

/// The `sqlite://` URL of a state file.
pub fn sqlite_url(db: &Path) -> String {
    format!("sqlite://{}", db.display())
}

/// The text of a systemd drop-in whose command line carries the state store of `db`.
pub fn unit_for(db: &Path) -> String {
    let url = sqlite_url(db);
    format!(
        "[Service]\n\
         ExecStart=\n\
         ExecStart=/usr/bin/kbf --mode=frontend --state-store={url} \\\n  \
         --listen=0.0.0.0:8980\n"
    )
}

/// The file `path` with `text` in it and `mode` as its permission bits.
pub fn write_with_mode(path: &Path, text: &str, mode: u32) {
    fs::write(path, text).expect("write the file");
    fs::set_permissions(path, Permissions::from_mode(mode)).expect("set the mode");
}

/// The permission bits of `path`.
pub fn mode_of(path: &Path) -> u32 {
    let meta = fs::metadata(path).expect("the file is there");
    meta.permissions().mode() & 0o7777
}

/// The name of the backup that goes with the setting file `file`.
pub fn backup_of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".pre-postgres");
    PathBuf::from(name)
}

/// The name the new text of the setting file `file` has until it is renamed into place.
pub fn temp_of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".postgres-new");
    PathBuf::from(name)
}

/// The bytes of the file at `path`.
pub fn read(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read the file")
}

/// The text of the file at `path`.
pub fn read_text(path: &Path) -> String {
    fs::read_to_string(path).expect("read the file")
}

/// The setting files of a farm: a state file, the kbf.toml that names it, and what the
/// kbf.toml said when the test began.
pub struct Farm {
    pub dir: TestDir,
    pub db: PathBuf,
    pub config: PathBuf,
    pub text: String,
}

impl Farm {
    /// A farm around the state file `db` in `dir`, with its kbf.toml (mode 0640).
    pub fn new(dir: TestDir, db: PathBuf) -> Farm {
        let config = dir.join("kbf.toml");
        let text = config_for(&db);
        write_with_mode(&config, &text, 0o640);
        Farm {
            dir,
            db,
            config,
            text,
        }
    }

    /// A farm around a copy of the golden state file, which no frontend has open.
    pub fn golden(name: &str) -> Farm {
        let dir = scratch(name);
        let db = golden_copy(&dir);
        Farm::new(dir, db)
    }

    /// The names in the farm's directory.
    pub fn names(&self) -> BTreeSet<String> {
        names(&self.dir)
    }

    /// The text of the kbf.toml now.
    pub fn config_now(&self) -> String {
        fs::read_to_string(&self.config).expect("read the kbf.toml")
    }

    /// Where the backup of the kbf.toml goes.
    pub fn backup(&self) -> PathBuf {
        let file = fs::canonicalize(&self.config).expect("the kbf.toml is there");
        backup_of(&file)
    }
}

/// The kbf.toml at `path` as the place of the setting.
pub fn config_place(path: &Path) -> PlaceArgs {
    PlaceArgs {
        config: Some(path.to_path_buf()),
        unit_file: None,
    }
}

/// The unit file at `path` as the place of the setting.
pub fn unit_place(path: &Path) -> PlaceArgs {
    PlaceArgs {
        config: None,
        unit_file: Some(path.to_path_buf()),
    }
}

/// The arguments of a cutover, with no unit and no URL of the frontend's own.
pub fn cutover_args(db: &Path, url: &str, place: PlaceArgs, apply: bool) -> CutoverArgs {
    CutoverArgs {
        state_db: db.to_path_buf(),
        postgres_url: url.to_owned(),
        place,
        unit: None,
        frontend_url: None,
        apply,
    }
}

/// A cutover through the library, with the report it printed.
pub fn run_cutover(args: &CutoverArgs, seams: &Seams) -> (Result<(), Error>, String) {
    let mut out: Vec<u8> = Vec::new();
    let result = kbf_state_copy::cutover::run(args, seams, &mut out);
    (result, String::from_utf8(out).expect("the report is UTF-8"))
}

/// A rollback through the library, with the report it printed.
pub fn run_rollback(
    place: PlaceArgs,
    unit: Option<&str>,
    apply: bool,
    seams: &Seams,
) -> (Result<(), Error>, String) {
    let args = RollbackArgs {
        place,
        unit: unit.map(str::to_owned),
        apply,
    };
    let mut out: Vec<u8> = Vec::new();
    let result = kbf_state_copy::cutover::rollback(&args, seams, &mut out);
    (result, String::from_utf8(out).expect("the report is UTF-8"))
}

/// A URL nothing listens on: a connection to it is refused at once.
pub const UNREACHABLE: &str = "postgres://nobody@127.0.0.1:1/none";

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

fn server_url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create databases in")
    })
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A database made for one test and dropped with it: no other test reads or writes it. The
/// store lives in the default schema of the URL it is given, as the frontend's flag takes no
/// other, so each test needs a database of its own.
pub struct Database {
    name: String,
}

impl Database {
    /// A new database with nothing in it, not even the store's schema.
    pub fn bare() -> Database {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("kbf_copy_{}_{n}", std::process::id());
        // A database belongs to the whole server: `ddl` makes it under the shared-catalog guard.
        let create = format!("CREATE DATABASE \"{name}\"");
        ddl(&server_url(), &create).expect("create the test's database");
        Database { name }
    }

    /// A new database with the store's schema, as the host's setup leaves it.
    pub fn with_schema() -> Database {
        let database = Database::bare();
        let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
        let migrations = schema::embedded();
        schema::apply(&config, &migrations, schema::latest()).expect("the installer's step");
        database
    }

    /// `KBF_TEST_POSTGRES_URL` with this database in place of its own.
    pub fn url(&self) -> String {
        with_database(&server_url(), &self.name)
    }

    /// A connection to the database.
    pub fn client(&self) -> postgres::Client {
        postgres::Client::connect(&self.url(), postgres::NoTls).expect("connect")
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let statement = format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name);
        let _ = ddl(&server_url(), &statement);
    }
}

/// `url` with its database (the path after the host) replaced, its query kept.
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

/// The rows in `table` of the store's schema.
pub fn count(client: &mut postgres::Client, table: &str) -> i64 {
    let sql = format!("SELECT count(*) FROM kbf_state.{table}");
    client.query_one(sql.as_str(), &[]).expect("count").get(0)
}

/// The head revision.
pub fn head(client: &mut postgres::Client) -> i64 {
    let sql = "SELECT rev FROM kbf_state.head";
    client.query_one(sql, &[]).expect("the head").get(0)
}

/// Key/value rows, feed entries, counters and the head revision: all zero for a store that
/// has never been written to.
pub fn shape(client: &mut postgres::Client) -> [i64; 4] {
    let kv = count(client, "kv");
    let feed = count(client, "feed");
    let counters = count(client, "counters");
    [kv, feed, counters, head(client)]
}

/// The key/value rows the store holds, in key order.
pub fn pg_entries(client: &mut postgres::Client) -> Vec<Entry> {
    let sql = "SELECT key, value, version, expires_us FROM kbf_state.kv ORDER BY key";
    let rows = client.query(sql, &[]).expect("read the rows");
    rows.iter().map(pg_entry).collect()
}

fn pg_entry(row: &postgres::Row) -> Entry {
    Entry {
        key: row.get(0),
        value: row.get(1),
        version: row.get(2),
        expires_us: row.get(3),
    }
}

/// The counters the store holds, in name order.
pub fn pg_counters(client: &mut postgres::Client) -> Vec<Counter> {
    let sql = "SELECT name, value FROM kbf_state.counters ORDER BY name";
    let rows = client.query(sql, &[]).expect("read the counters");
    rows.iter().map(pg_counter).collect()
}

fn pg_counter(row: &postgres::Row) -> Counter {
    Counter {
        name: row.get(0),
        value: row.get(1),
    }
}
