//! The state database's schema: versioned migrations, the `schema_version` table, and the
//! check a kbf process makes before it serves.
//!
//! `kbf-installer` owns the schema. Its `setup` creates it and every update applies the new
//! release's migrations before the new child starts ([`apply`]); `kbf boot-check` applies
//! this build's before a unit of the kbf package starts it ([`super::boot`]). The kbf that
//! serves never creates or changes a table:
//! [`PostgresStore::open`](super::PostgresStore::open) judges the version
//! once, on its first connection, and refuses with one line when it is outside what the
//! binary supports. The check runs at start only: a child that is serving keeps its pooled
//! and new connections when the installer migrates under it.
//!
//! **Migrations.** Each is a SQL file embedded in the binary (`migrations/NNNN_name.sql`),
//! numbered from 1 with no gap, applied in order, each in its own transaction that also
//! records the row in `schema_version`, under an advisory lock so two installers do not
//! race. The file names no schema: the runner sets `search_path` to the store's schema.
//! The first line of a file says `-- kbf-migration: expand` or `contract`.
//!
//! **Expand only.** While release N serves, release N+1 may only add: an `expand`
//! migration passes a lint that refuses `DROP`, `RENAME`, `TRUNCATE`, `DELETE`, a column
//! type change, `SET NOT NULL` and a `NOT NULL` column added without a default. Any
//! migration is refused if it uses dynamic SQL (`EXECUTE`) or controls transactions. A
//! `contract` step (the removal an earlier release made unused) belongs to a later
//! release; [`apply`] never runs one. The lint reads text: a review guard for attested
//! release content, not a security boundary.
//!
//! **Supported range.** A binary supports versions `min..=max` ([`Supported`]); `max` is the
//! newest migration it embeds. A schema above `max` is still served when every row above
//! it is `expand`, which is what lets a release that was reverted after its successor
//! migrated start again. A `contract` row above `max`, a version below `min`, or no schema
//! at all is refused.
//!
//! **Extending.** A crate that keeps tables of its own in the schema ships its migrations
//! (the same files, lint and `schema_version` rows) and a release puts them after this
//! crate's with [`extend`]; the numbers and names it gives them are what an existing database
//! has recorded, so they never change. This crate's own set is [`embedded`].

use std::fmt;
use std::time::Duration;

use postgres::error::SqlState;

use super::conn::Conn;
use super::{PostgresConfig, describe};

/// The oldest schema version this build runs against. Raise it in the release that follows
/// a `contract` migration, in the same commit.
///
/// The start check accepts every version from here up to the newest this build embeds, not
/// only the newest: a schema that is behind (in range, older than [`latest`]) is served as it
/// is, because each migration since this one is an expand step and the installer brings the
/// schema forward before it activates a release that needs more. The test
/// `a_schema_behind_the_newest_but_in_range_is_accepted` pins it.
pub const MIN_SUPPORTED: u32 = 1;

/// The migrations of this crate: version, name, file. A crate that keeps tables of its own
/// in the schema ships its migration itself and continues the numbering ([`extend`]); the set
/// a release runs is this one plus theirs, put together by the binary that depends on them.
const EMBEDDED: [(u32, &str, &str); 1] = [(1, "state", include_str!("migrations/0001_state.sql"))];

/// Taken (transaction-scoped) by every schema change: the installer's `apply` and `setup`
/// serialise on it, so two installers racing apply a migration once. The key is
/// `hashtext('kbf-state-schema')`, a 32-bit hash widened to the `bigint` key space; it is a
/// fixed string, so every installer and every release computes the same key, and nothing
/// else in the database may take an advisory lock on that value. Changing the string splits
/// the lock between releases (an old and a new installer would no longer exclude each
/// other), so it never changes.
const LOCK: &str = "SELECT pg_advisory_xact_lock(hashtext('kbf-state-schema'))";

/// Whether a migration leaves the previous release working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Only adds; the previous release keeps working.
    Expand,
    /// Removes what an earlier release stopped using.
    Contract,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Expand => "expand",
            Kind::Contract => "contract",
        }
    }
}

/// One numbered, linted schema change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    version: u32,
    name: String,
    kind: Kind,
    sql: String,
}

impl Migration {
    /// Reads a migration file.
    ///
    /// # Errors
    /// [`SchemaError`] when the first line does not give the kind, when the file controls
    /// transactions or runs dynamic SQL, or when an `expand` file drops, renames or
    /// otherwise breaks the release that is still serving.
    pub fn new(version: u32, name: &str, sql: &str) -> Result<Migration, SchemaError> {
        let first = sql.lines().map(str::trim).find(|l| !l.is_empty());
        let kind = match first.and_then(|l| l.strip_prefix("-- kbf-migration:")) {
            Some(k) if k.trim() == "expand" => Kind::Expand,
            Some(k) if k.trim() == "contract" => Kind::Contract,
            _ => return Err(SchemaError::NoKind { version }),
        };
        let found = lint(sql);
        if let Some(what) = found.always {
            return Err(SchemaError::Forbidden { version, what });
        }
        if kind == Kind::Expand
            && let Some(what) = found.breaking
        {
            return Err(SchemaError::Destructive { version, what });
        }
        Ok(Migration {
            version,
            name: name.to_owned(),
            kind,
            sql: sql.to_owned(),
        })
    }

    /// The number, from 1.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The name recorded in `schema_version`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Expand or contract.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }
}

/// The migrations of this crate alone, in order. A release's set is these plus the migrations
/// of the crates that extend the schema ([`extend`]).
///
/// # Panics
/// When an embedded file fails its own lint (a test refuses that, so a release never
/// ships one).
#[must_use]
pub fn embedded() -> Vec<Migration> {
    EMBEDDED
        .iter()
        .map(|(v, name, sql)| Migration::new(*v, name, sql).expect("an embedded migration lints"))
        .collect()
}

/// The newest schema version of this crate's own migrations; [`latest_of`] a set that is
/// extended.
#[must_use]
pub fn latest() -> u32 {
    EMBEDDED.last().map_or(0, |m| m.0)
}

/// The newest version of `set`, 0 when it is empty.
#[must_use]
pub fn latest_of(set: &[Migration]) -> u32 {
    set.last().map_or(0, |m| m.version)
}

/// The migrations of `base` followed by those of `more`. The numbers are the extending
/// crate's to fix and never change once a release has shipped them: the database records a
/// migration by its number and name, so an applied one is recognised by both.
///
/// # Errors
/// [`SchemaError::Numbering`] when the result is not numbered 1, 2, 3 in order.
pub fn extend(
    base: Vec<Migration>,
    more: impl IntoIterator<Item = Migration>,
) -> Result<Vec<Migration>, SchemaError> {
    let mut set = base;
    set.extend(more);
    numbered(&set)?;
    Ok(set)
}

/// The versions a binary runs against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supported {
    /// The oldest.
    pub min: u32,
    /// The newest it embeds.
    pub max: u32,
}

impl Supported {
    /// What this crate's own migrations support ([`embedded`]); a release that extends the
    /// set reads its own `max` from [`latest_of`].
    #[must_use]
    pub fn this_build() -> Supported {
        Supported {
            min: MIN_SUPPORTED,
            max: latest(),
        }
    }
}

/// Why the schema was refused, or could not be read or changed. Each message is one line
/// and never carries the connection URL, and a server's refusal of the connection is
/// reduced to its SQLSTATE (`describe_refusal`), so it names no role, database or host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// No `schema_version` rows.
    #[error("the state database has no schema: run `kbf-installer setup`")]
    NotInitialised,
    /// Below the oldest version this binary runs against.
    #[error(
        "the state schema is version {found} and this kbf supports {min} to {max}: run `kbf-installer setup`, or update this host's kbf-installer first"
    )]
    TooOld {
        /// The database's version.
        found: u32,
        /// The oldest supported.
        min: u32,
        /// The newest supported.
        max: u32,
    },
    /// Above the newest, and changed in a way the previous release cannot run on.
    #[error(
        "the state schema is version {found}, past the {min} to {max} this kbf supports, and version {contract} removed something this kbf uses: run the kbf release that matches the database"
    )]
    TooNew {
        /// The database's version.
        found: u32,
        /// The oldest supported.
        min: u32,
        /// The newest supported.
        max: u32,
        /// The first contract migration above `max`.
        contract: u32,
    },
    /// The installer embeds older migrations than the database has.
    #[error(
        "the state schema is version {found} and this kbf-installer only knows up to {newest}: update the kbf-installer package"
    )]
    DatabaseAhead {
        /// The database's version.
        found: u32,
        /// The newest this installer embeds.
        newest: u32,
    },
    /// The release needs a schema this installer does not embed.
    #[error(
        "the release needs state schema {wanted} and this kbf-installer only embeds up to {newest}: update the kbf-installer package"
    )]
    NotShipped {
        /// What the release asked for.
        wanted: u32,
        /// The newest this installer embeds.
        newest: u32,
    },
    /// A pending migration is a contract step.
    #[error(
        "migration {version} is a contract step (it removes something) and an update never runs one: ship it in a later release"
    )]
    ContractPending {
        /// The migration.
        version: u32,
    },
    /// An expand migration that is not.
    #[error(
        "migration {version} is marked expand but has {what}: expand-only, contract comes in a later release"
    )]
    Destructive {
        /// The migration.
        version: u32,
        /// What the lint found.
        what: &'static str,
    },
    /// Refused in any migration.
    #[error("migration {version} has {what}, which a migration may not")]
    Forbidden {
        /// The migration.
        version: u32,
        /// What the lint found.
        what: &'static str,
    },
    /// No `-- kbf-migration: expand|contract` first line.
    #[error("migration {version} must start with `-- kbf-migration: expand` or `contract`")]
    NoKind {
        /// The migration.
        version: u32,
    },
    /// The set is not numbered 1, 2, 3... in order.
    #[error(
        "the migrations are not numbered 1, 2, 3 in order (found {found} where {expected} belongs)"
    )]
    Numbering {
        /// The number that belongs.
        expected: u32,
        /// The number found.
        found: u32,
    },
    /// The database's rows skip a version.
    #[error("the schema_version table skips from {before} to {after}")]
    Gap {
        /// The row before.
        before: u32,
        /// The row after.
        after: u32,
    },
    /// A migration's own statements failed; its transaction was rolled back.
    #[error("state database: migration {version} ({name}) failed: {why}")]
    Failed {
        /// The migration.
        version: u32,
        /// Its name.
        name: String,
        /// What the server said, never the connection URL.
        why: String,
    },
    /// Another installer holds the schema lock, or a table is in use, for longer than the
    /// caller waits ([`apply_within`]). Nothing was changed by the migration that waited.
    #[error(
        "the state database is busy: another installer is migrating, or a table is in use, past the wait"
    )]
    Busy,
    /// The server cannot be reached, or refused something outside a migration.
    #[error("state database: {0}")]
    Database(String),
}

impl From<postgres::Error> for SchemaError {
    fn from(err: postgres::Error) -> SchemaError {
        SchemaError::Database(describe_refusal(&err))
    }
}

/// What the server said, for a schema error. Unlike [`describe`], which the store keeps for
/// its own paths, a server's FATAL (a connection refused: `role "x" does not exist`,
/// `database "y" does not exist`, a `pg_hba` rejection that quotes host, user and database)
/// is reduced to a fixed phrase and its SQLSTATE, because these errors end up in
/// `failure.reason`, which is shown to operators and written to `update.json`. Other
/// server errors keep their text: they name tables and statements of the migration, not
/// the connection.
fn describe_refusal(err: &postgres::Error) -> String {
    match err.as_db_error() {
        Some(db) if db.severity() == "FATAL" => format!(
            "the server refused the connection or ended the session (SQLSTATE {})",
            db.code().code()
        ),
        _ => describe(err),
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The version before: `None` on a database with no schema.
    pub from: Option<u32>,
    /// The version after.
    pub to: u32,
    /// The migrations this call ran.
    pub ran: Vec<u32>,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.from, self.ran.is_empty()) {
            (None, _) => write!(f, "created the schema at version {}", self.to),
            (Some(from), true) => write!(f, "the schema is at version {from}, nothing to apply"),
            (Some(from), false) => write!(f, "migrated the schema from {from} to {}", self.to),
        }
    }
}

// ---- the lint ---------------------------------------------------------------------------

struct Found {
    /// Refused in any migration.
    always: Option<&'static str>,
    /// Refused in an expand migration.
    breaking: Option<&'static str>,
}

/// The upper-case words of `sql`, one list per statement, without comments, quoted strings
/// and quoted identifiers. A dollar-quoted body is kept: a `DO $$ ... DROP ... $$` must not
/// hide a drop.
fn words(sql: &str) -> Vec<Vec<String>> {
    fn flush(word: &mut String, statements: &mut [Vec<String>]) {
        if !word.is_empty() {
            statements
                .last_mut()
                .expect("one statement")
                .push(word.to_ascii_uppercase());
            word.clear();
        }
    }
    let b = sql.as_bytes();
    let mut statements = vec![Vec::new()];
    let mut word = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        match (c, next) {
            (b'-', Some(b'-')) => {
                flush(&mut word, &mut statements);
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                flush(&mut word, &mut statements);
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    match (b[i], b.get(i + 1).copied()) {
                        (b'/', Some(b'*')) => {
                            depth += 1;
                            i += 1;
                        }
                        (b'*', Some(b'/')) => {
                            depth -= 1;
                            i += 1;
                        }
                        _ => {}
                    }
                    i += 1;
                }
                continue;
            }
            (b'\'' | b'"', _) => {
                flush(&mut word, &mut statements);
                i += 1;
                while i < b.len() {
                    if b[i] == c {
                        if b.get(i + 1) == Some(&c) {
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    i += 1;
                }
            }
            (b';', _) => {
                flush(&mut word, &mut statements);
                statements.push(Vec::new());
            }
            // Kept as words of their own: the lint reads clauses (split at a comma outside
            // parentheses) and what comes after an opening parenthesis.
            (b',' | b'(' | b')', _) => {
                flush(&mut word, &mut statements);
                statements
                    .last_mut()
                    .expect("one statement")
                    .push(char::from(c).to_string());
            }
            (c, _) if c.is_ascii_alphanumeric() || c == b'_' => word.push(char::from(c)),
            _ => flush(&mut word, &mut statements),
        }
        i += 1;
    }
    flush(&mut word, &mut statements);
    statements.retain(|s| !s.is_empty());
    statements
}

/// The clauses of an `ALTER`-style statement: split at the commas outside parentheses.
fn clauses(stmt: &[String]) -> Vec<&[String]> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0_usize, 0);
    for (i, w) in stmt.iter().enumerate() {
        match w.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                out.push(&stmt[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&stmt[start..]);
    out
}

/// The words that name a table after `from` (an index of the word before the name), up to
/// the first of `stops`. Empty when a quoted name left nothing there: the caller then
/// cannot tell which table it is and fails closed.
fn table_name(stmt: &[String], from: usize, stops: &[&str]) -> String {
    stmt[from..]
        .iter()
        .take_while(|w| !stops.contains(&w.as_str()))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(".")
}

fn lint(sql: &str) -> Found {
    let mut always = None;
    let mut breaking = None;
    // The tables this file creates: an index on one of them cannot lock out the release
    // that is still serving, a unique index on any other can.
    let mut created: Vec<String> = Vec::new();
    for stmt in words(sql) {
        let has = |w: &str| stmt.iter().any(|t| t == w);
        let seq = |words: [&str; 3]| {
            stmt.windows(3)
                .any(|w| w.iter().zip(words).all(|(a, b)| a == b))
        };
        let pair = |a: &str, b: &str| stmt.windows(2).any(|w| w[0] == a && w[1] == b);
        if ["COMMIT", "ROLLBACK", "ABORT", "SAVEPOINT"]
            .iter()
            .any(|w| has(w))
        {
            always.get_or_insert("transaction control");
        }
        if has("EXECUTE") {
            always.get_or_insert("dynamic SQL (EXECUTE)");
        }
        for (word, what) in [
            ("DROP", "a DROP"),
            ("RENAME", "a RENAME"),
            ("TRUNCATE", "a TRUNCATE"),
            ("DELETE", "a DELETE"),
        ] {
            if has(word) {
                breaking.get_or_insert(what);
            }
        }
        if has("ALTER") && has("TYPE") && !has("CREATE") {
            breaking.get_or_insert("a column type change");
        }
        if seq(["SET", "NOT", "NULL"]) {
            breaking.get_or_insert("SET NOT NULL");
        }
        if pair("SET", "DEFAULT") {
            breaking.get_or_insert("SET DEFAULT");
        }
        // Per clause: `ADD COLUMN a int DEFAULT 0, ADD COLUMN b text NOT NULL` must not let
        // the first clause's default excuse the second.
        for clause in clauses(&stmt) {
            let in_clause = |w: &str| clause.iter().any(|t| t == w);
            let not_null = clause.windows(2).any(|w| w[0] == "NOT" && w[1] == "NULL");
            if in_clause("ADD") && not_null && !in_clause("DEFAULT") {
                breaking.get_or_insert("a NOT NULL column added without a default");
            }
            if clause.windows(2).any(|w| {
                w[0] == "ADD"
                    && [
                        "CONSTRAINT",
                        "PRIMARY",
                        "UNIQUE",
                        "CHECK",
                        "FOREIGN",
                        "EXCLUDE",
                    ]
                    .contains(&w[1].as_str())
            }) {
                breaking.get_or_insert("ADD CONSTRAINT on an existing table");
            }
        }
        if seq(["CREATE", "OR", "REPLACE"]) {
            breaking.get_or_insert("CREATE OR REPLACE");
        }
        // An UPDATE that does something to rows: it starts a statement, or follows what
        // starts one inside a body (`BEGIN`, `THEN`, `ELSE`, `LOOP`) or a parenthesis (a
        // CTE). `ON CONFLICT DO UPDATE`, `ON UPDATE CASCADE`, `FOR UPDATE` and `GRANT UPDATE`
        // do not rewrite rows of their own.
        if stmt.iter().enumerate().any(|(i, w)| {
            w == "UPDATE"
                && (i == 0
                    || ["BEGIN", "THEN", "ELSE", "LOOP", "("].contains(&stmt[i - 1].as_str()))
        }) {
            breaking.get_or_insert("an UPDATE of rows");
        }
        if let Some(i) = stmt.iter().position(|w| w == "TABLE")
            && stmt.first().is_some_and(|f| f == "CREATE")
        {
            let skip = if stmt[i + 1..].starts_with(&["IF".into(), "NOT".into(), "EXISTS".into()]) {
                3
            } else {
                0
            };
            let name = table_name(&stmt, i + 1 + skip, &["(", "AS", "PARTITION", "OF"]);
            if !name.is_empty() {
                created.push(name);
            }
        }
        if let Some(i) = stmt
            .windows(3)
            .position(|w| w == ["CREATE", "UNIQUE", "INDEX"])
        {
            let on = stmt[i..].iter().position(|w| w == "ON").map(|k| i + k + 1);
            let on = on.map(|k| {
                if stmt.get(k).is_some_and(|w| w == "ONLY") {
                    k + 1
                } else {
                    k
                }
            });
            let table = on.map(|k| table_name(&stmt, k, &["(", "USING"]));
            if !table.is_some_and(|t| !t.is_empty() && created.contains(&t)) {
                breaking.get_or_insert("a CREATE UNIQUE INDEX on a table not created in this file");
            }
        }
    }
    Found { always, breaking }
}

// ---- reading the version ------------------------------------------------------------------

/// Refuses a server that would acknowledge a commit it has not flushed.
pub(super) fn require_durable(conn: &mut Conn) -> Result<(), SchemaError> {
    let setting: String = conn.query_one("SHOW synchronous_commit", &[])?.get(0);
    if matches!(setting.as_str(), "on" | "remote_apply") {
        Ok(())
    } else {
        Err(SchemaError::Database(format!(
            "synchronous_commit is {setting:?} on this connection; the state store needs \
             \"on\" (or \"remote_apply\"), or an acknowledged write can be lost"
        )))
    }
}

/// Every `schema_version` row, in order: version and kind. Empty when there is no table.
fn rows(conn: &mut Conn, schema: &str) -> Result<Vec<(u32, String)>, SchemaError> {
    let exists: Option<String> = conn
        .query_one(
            "SELECT to_regclass($1::text)::text",
            &[&format!("{schema}.schema_version")],
        )?
        .get(0);
    if exists.is_none() {
        return Ok(Vec::new());
    }
    let found = conn.query(
        &format!("SELECT version, kind FROM {schema}.schema_version ORDER BY version"),
        &[],
    )?;
    Ok(found
        .iter()
        .map(|r| (u32::try_from(r.get::<_, i32>(0)).unwrap_or(0), r.get(1)))
        .collect())
}

fn contiguous(rows: &[(u32, String)]) -> Result<(), SchemaError> {
    let mut before = 0;
    for (v, _) in rows {
        if *v != before + 1 {
            return Err(SchemaError::Gap { before, after: *v });
        }
        before = *v;
    }
    Ok(())
}

/// Judges the database's schema against `supported`; returns the newest version.
pub(super) fn check_conn(
    conn: &mut Conn,
    schema: &str,
    supported: Supported,
) -> Result<u32, SchemaError> {
    judge(&rows(conn, schema)?, supported)
}

/// The start check on the rows of `schema_version`, oldest first.
///
/// **Behind is accepted.** A version in `min..max` (older than the newest this build embeds
/// but not older than [`MIN_SUPPORTED`]) passes: every migration since `min` is an expand
/// step, so this build still runs on it, and the installer brings the schema forward when
/// it activates a release that needs more. Refusing `found < max` instead would stop a
/// release from starting on the schema its predecessor left, which is every update. Only
/// below `min`, nothing at all, or a `contract` row above `max` is refused.
fn judge(rows: &[(u32, String)], supported: Supported) -> Result<u32, SchemaError> {
    let Some(&(found, _)) = rows.last() else {
        return Err(SchemaError::NotInitialised);
    };
    if found < supported.min {
        return Err(SchemaError::TooOld {
            found,
            min: supported.min,
            max: supported.max,
        });
    }
    if let Some((contract, _)) = rows
        .iter()
        .find(|(v, k)| *v > supported.max && k != Kind::Expand.as_str())
    {
        return Err(SchemaError::TooNew {
            found,
            min: supported.min,
            max: supported.max,
            contract: *contract,
        });
    }
    Ok(found)
}

/// The schema version of the database `config` names.
///
/// # Errors
/// [`SchemaError::NotInitialised`] when there is none; the server's refusal otherwise.
pub fn current(config: &PostgresConfig) -> Result<u32, SchemaError> {
    let mut conn = Conn::connect(&config.pg)?;
    let rows = rows(&mut conn, &config.schema)?;
    rows.last()
        .map(|(v, _)| *v)
        .ok_or(SchemaError::NotInitialised)
}

// ---- changing it ------------------------------------------------------------------------

fn numbered(set: &[Migration]) -> Result<(), SchemaError> {
    for (i, m) in set.iter().enumerate() {
        let expected = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if m.version != expected {
            return Err(SchemaError::Numbering {
                expected,
                found: m.version,
            });
        }
    }
    Ok(())
}

/// Brings the database up to `upto` with the migrations of `set`, creating the schema and
/// the `schema_version` table on a database that has none. Idempotent: what is applied is
/// skipped.
///
/// Nothing is run unless every pending migration up to `upto` is an `expand`. A database
/// already past `upto` is left alone.
///
/// # Errors
/// [`SchemaError`]: the set is not numbered from 1, `upto` is not in the set, the database
/// is ahead of the set, a pending migration is a contract step, or the server refused one
/// (that migration's transaction is rolled back; earlier ones stay applied).
pub fn apply(
    config: &PostgresConfig,
    set: &[Migration],
    upto: u32,
) -> Result<Outcome, SchemaError> {
    apply_with(config, set, upto, None)
}

/// [`apply`] that does not wait for ever: a connect takes at most [`CONNECT_LIMIT`], and
/// every lock a migration's transaction waits for (the schema lock, and the table locks of
/// its DDL) at most `lock_wait`. A longer wait is [`SchemaError::Busy`] with that
/// migration rolled back, so a caller that holds something (a swap) is not stuck behind
/// another installer or a long read on the old child.
///
/// # Errors
/// Those of [`apply`], and [`SchemaError::Busy`].
pub fn apply_within(
    config: &PostgresConfig,
    set: &[Migration],
    upto: u32,
    lock_wait: Duration,
) -> Result<Outcome, SchemaError> {
    apply_with(config, set, upto, Some(lock_wait))
}

/// The longest [`apply_within`] waits to reach the server.
pub const CONNECT_LIMIT: Duration = Duration::from_secs(10);

/// How a call connects: a bounded call (`lock_wait` given) gives up reaching the server
/// after [`CONNECT_LIMIT`], an unbounded one waits as the operating system lets it.
fn connect_settings(pg: &postgres::Config, lock_wait: Option<Duration>) -> postgres::Config {
    let mut settings = pg.clone();
    if lock_wait.is_some() {
        settings.connect_timeout(CONNECT_LIMIT);
    }
    settings
}

fn apply_with(
    config: &PostgresConfig,
    set: &[Migration],
    upto: u32,
    lock_wait: Option<Duration>,
) -> Result<Outcome, SchemaError> {
    numbered(set)?;
    let newest = set.last().map_or(0, |m| m.version);
    if upto > newest {
        return Err(SchemaError::NotShipped {
            wanted: upto,
            newest,
        });
    }
    let schema = config.schema.as_str();
    let mut conn = Conn::connect(&connect_settings(&config.pg, lock_wait))?;
    require_durable(&mut conn)?;
    let have = rows(&mut conn, schema)?;
    contiguous(&have)?;
    let from = have.last().map(|(v, _)| *v);
    let at = from.unwrap_or(0);
    if at > newest {
        return Err(SchemaError::DatabaseAhead { found: at, newest });
    }
    let pending: Vec<&Migration> = set
        .iter()
        .filter(|m| m.version > at && m.version <= upto)
        .collect();
    if let Some(m) = pending.iter().find(|m| m.kind == Kind::Contract) {
        return Err(SchemaError::ContractPending { version: m.version });
    }
    let mut ran = Vec::new();
    for m in pending {
        if run_one(&mut conn, schema, m, lock_wait)? {
            ran.push(m.version);
        }
    }
    Ok(Outcome {
        from,
        to: at.max(ran.last().copied().unwrap_or(0)),
        ran,
    })
}

/// One migration in one transaction with its `schema_version` row. `false` when another
/// installer applied it while this one waited for the lock.
fn run_one(
    conn: &mut Conn,
    schema: &str,
    m: &Migration,
    lock_wait: Option<Duration>,
) -> Result<bool, SchemaError> {
    let result = run_in_transaction(conn, schema, m, lock_wait);
    if result.is_err() {
        // The caller drops the session; this leaves it clean if it does not.
        let _ = conn.batch_execute("ROLLBACK");
    }
    result
}

/// A server refusal as the caller sees it: a lock wait that ran out is [`SchemaError::Busy`].
fn lock_or(err: &postgres::Error) -> Option<SchemaError> {
    (err.code() == Some(&SqlState::LOCK_NOT_AVAILABLE)).then_some(SchemaError::Busy)
}

fn run_in_transaction(
    conn: &mut Conn,
    schema: &str,
    m: &Migration,
    lock_wait: Option<Duration>,
) -> Result<bool, SchemaError> {
    // `SET LOCAL` lasts for this transaction: the wait applies to the schema lock and to
    // the locks the migration's own DDL takes.
    let wait = lock_wait.map_or_else(String::new, |d| {
        format!("SET LOCAL lock_timeout = '{}ms'; ", d.as_millis().max(1))
    });
    conn.batch_execute(&format!(
        "BEGIN; {wait}{LOCK}; CREATE SCHEMA IF NOT EXISTS {schema}; SET LOCAL search_path TO {schema}; \
         CREATE TABLE IF NOT EXISTS schema_version (\
             version integer PRIMARY KEY, \
             name text NOT NULL, \
             kind text NOT NULL CHECK (kind IN ('expand', 'contract')), \
             applied_at timestamptz NOT NULL DEFAULT now())"
    ))
    .map_err(|e| lock_or(&e).unwrap_or_else(|| e.into()))?;
    let version = i32::try_from(m.version).unwrap_or(i32::MAX);
    let taken = conn.query(
        "SELECT 1 FROM schema_version WHERE version = $1",
        &[&version],
    )?;
    if !taken.is_empty() {
        conn.batch_execute("ROLLBACK")?;
        return Ok(false);
    }
    if let Err(err) = conn.batch_execute(&m.sql) {
        if let Some(busy) = lock_or(&err) {
            return Err(busy);
        }
        let hint = if err.code() == Some(&SqlState::DUPLICATE_TABLE) {
            " (tables from before the schema was versioned? the farm starts fresh: use a new database or schema)"
        } else {
            ""
        };
        return Err(SchemaError::Failed {
            version: m.version,
            name: m.name.clone(),
            why: format!("{}{hint}", describe_refusal(&err)),
        });
    }
    conn.execute(
        "INSERT INTO schema_version (version, name, kind) VALUES ($1, $2, $3)",
        &[&version, &m.name, &m.kind.as_str()],
    )?;
    conn.batch_execute("COMMIT")?;
    Ok(true)
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
