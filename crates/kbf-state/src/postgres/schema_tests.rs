//! The migration lint and the embedded set. No database: the tests that need a server are
//! `tests/postgres_schema.rs`.

use super::{Kind, MIN_SUPPORTED, Migration, SchemaError, Supported, embedded, judge, latest};

fn expand(body: &str) -> Result<Migration, SchemaError> {
    Migration::new(2, "t", &format!("-- kbf-migration: expand\n{body}"))
}

/// Catches an embedded migration that would break the release still serving, or a set with
/// a hole in its numbering, shipping in a release: the lint is the review rule made
/// mechanical. Mutant seen red: the embedded file given a `DROP` (`Destructive`).
#[test]
fn every_embedded_migration_is_numbered_in_order_and_expand_only() {
    let set = embedded();
    assert_eq!(u32::try_from(set.len()).unwrap(), latest());
    for (i, m) in set.iter().enumerate() {
        assert_eq!(m.version(), u32::try_from(i + 1).unwrap());
        assert_eq!(m.kind(), Kind::Expand, "{}", m.name());
    }
    assert!(MIN_SUPPORTED >= 1 && MIN_SUPPORTED <= latest());
}

/// Catches an expand migration that drops or renames being accepted. Each statement is one
/// the previous release cannot run on.
#[test]
fn an_expand_migration_that_removes_or_changes_something_is_refused() {
    for (sql, what) in [
        ("DROP TABLE kv;", "a DROP"),
        ("drop index kv_expiry;", "a DROP"),
        ("ALTER TABLE kv DROP COLUMN expires_us;", "a DROP"),
        ("ALTER TABLE kv RENAME COLUMN value TO v;", "a RENAME"),
        ("ALTER TABLE kv RENAME TO kv2;", "a RENAME"),
        ("TRUNCATE feed;", "a TRUNCATE"),
        ("DELETE FROM counters;", "a DELETE"),
        (
            "ALTER TABLE kv ALTER COLUMN version TYPE integer;",
            "a column type change",
        ),
        (
            "ALTER TABLE kv ALTER COLUMN expires_us SET NOT NULL;",
            "SET NOT NULL",
        ),
        (
            "ALTER TABLE kv ADD COLUMN note text NOT NULL;",
            "a NOT NULL column added without a default",
        ),
        ("DO $$ BEGIN DROP TABLE kv; END $$;", "a DROP"),
        ("/* hidden */ DROP TABLE kv;", "a DROP"),
    ] {
        assert_eq!(
            expand(sql).unwrap_err(),
            SchemaError::Destructive { version: 2, what },
            "{sql}"
        );
    }
}

/// Catches the lint passing a statement that can still break the release that is serving,
/// though it neither drops nor renames: a constraint added to rows that exist (the serving
/// release's writes then fail), a unique index on a table that already has rows, an UPDATE
/// of rows, a function replaced under the callers of the old one, a changed default, and a
/// NOT NULL column that a default on another clause of the same ALTER used to excuse.
///
/// Mutants seen red: each of these statements passed before the rules were added.
#[test]
fn an_expand_migration_that_can_still_break_the_serving_release_is_refused() {
    for (sql, what) in [
        (
            "ALTER TABLE kv ADD CONSTRAINT v CHECK (version > 0);",
            "ADD CONSTRAINT on an existing table",
        ),
        (
            "ALTER TABLE kv ADD UNIQUE (value);",
            "ADD CONSTRAINT on an existing table",
        ),
        (
            "ALTER TABLE kv ADD COLUMN a int, ADD FOREIGN KEY (a) REFERENCES feed (rev);",
            "ADD CONSTRAINT on an existing table",
        ),
        (
            "CREATE UNIQUE INDEX kv_value ON kv (value);",
            "a CREATE UNIQUE INDEX on a table not created in this file",
        ),
        (
            "CREATE TABLE a (x int);\nCREATE UNIQUE INDEX kv_value ON kv (value);",
            "a CREATE UNIQUE INDEX on a table not created in this file",
        ),
        (
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS i ON \"kv\" (value);",
            "a CREATE UNIQUE INDEX on a table not created in this file",
        ),
        ("UPDATE counters SET value = 0;", "an UPDATE of rows"),
        (
            "WITH u AS (UPDATE counters SET value = 0 RETURNING 1) SELECT * FROM u;",
            "an UPDATE of rows",
        ),
        (
            "DO $$ BEGIN UPDATE counters SET value = 0; END $$;",
            "an UPDATE of rows",
        ),
        (
            "CREATE OR REPLACE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;",
            "CREATE OR REPLACE",
        ),
        ("CREATE OR REPLACE VIEW v AS SELECT 1;", "CREATE OR REPLACE"),
        (
            "ALTER TABLE kv ALTER COLUMN version SET DEFAULT 7;",
            "SET DEFAULT",
        ),
        (
            "ALTER TABLE kv ADD COLUMN a bigint DEFAULT 0, ADD COLUMN b text NOT NULL;",
            "a NOT NULL column added without a default",
        ),
        (
            "ALTER TABLE kv ADD COLUMN a bigint NOT NULL, ADD COLUMN b text DEFAULT 'x';",
            "a NOT NULL column added without a default",
        ),
    ] {
        assert_eq!(
            expand(sql).unwrap_err(),
            SchemaError::Destructive { version: 2, what },
            "{sql}"
        );
    }
}

/// Catches the lint refusing what an expand migration is for, which would make every
/// release a refusal.
#[test]
fn additive_statements_pass_and_words_in_text_or_comments_do_not_count() {
    for sql in [
        "ALTER TABLE kv ADD COLUMN note text;",
        "ALTER TABLE kv ADD COLUMN n bigint NOT NULL DEFAULT 0;",
        "CREATE TABLE extra (id bigint PRIMARY KEY, a text NOT NULL);",
        "CREATE INDEX kv_note ON kv (key);",
        "CREATE TYPE flavour AS ENUM ('drop', 'rename');",
        "COMMENT ON TABLE kv IS 'we never DROP or RENAME this';",
        "-- DROP TABLE kv;\nALTER TABLE kv ADD COLUMN x text; /* DELETE */",
        "ALTER TABLE kv ADD COLUMN \"drop\" text;",
        "INSERT INTO counters (name, value) VALUES ('a', 1) ON CONFLICT (name) DO UPDATE SET value = 2;",
        "CREATE TABLE ref (k text REFERENCES kv (key) ON UPDATE CASCADE);",
        "SELECT value FROM counters FOR UPDATE;",
        "ALTER TABLE kv ADD COLUMN a bigint DEFAULT 0, ADD COLUMN b text;",
        "ALTER TABLE kv ADD COLUMN a bigint NOT NULL DEFAULT 0, ADD COLUMN b text NOT NULL DEFAULT '';",
        "CREATE TABLE extra (id bigint PRIMARY KEY, a text);\nCREATE UNIQUE INDEX extra_a ON extra (a);",
        "CREATE TABLE IF NOT EXISTS extra (id bigint);\nCREATE UNIQUE INDEX extra_id ON ONLY extra USING btree (id);",
        "CREATE INDEX kv_value ON kv (value);",
    ] {
        assert!(expand(sql).is_ok(), "{sql}: {:?}", expand(sql));
    }
}

/// Catches a file that can end the runner's transaction or hide a statement in a string.
#[test]
fn transaction_control_and_dynamic_sql_are_refused_even_in_a_contract_step() {
    for (sql, what) in [
        ("COMMIT;", "transaction control"),
        (
            "ALTER TABLE kv ADD COLUMN a text; ROLLBACK;",
            "transaction control",
        ),
        (
            "DO $$ BEGIN EXECUTE 'DROP TABLE kv'; END $$;",
            "dynamic SQL (EXECUTE)",
        ),
    ] {
        for kind in ["expand", "contract"] {
            assert_eq!(
                Migration::new(2, "t", &format!("-- kbf-migration: {kind}\n{sql}")).unwrap_err(),
                SchemaError::Forbidden { version: 2, what },
                "{kind}: {sql}"
            );
        }
    }
}

/// Catches a migration with no declared kind (the lint would not know what to hold it to).
#[test]
fn a_migration_must_say_what_kind_it_is() {
    for sql in [
        "",
        "ALTER TABLE kv ADD COLUMN a text;",
        "-- kbf-migration: shrink\nSELECT 1;",
    ] {
        assert_eq!(
            Migration::new(2, "t", sql).unwrap_err(),
            SchemaError::NoKind { version: 2 }
        );
    }
}

#[test]
fn a_contract_step_may_drop_but_is_not_an_expand_step() {
    let m = Migration::new(
        2,
        "t",
        "-- kbf-migration: contract\nALTER TABLE kv DROP COLUMN expires_us;",
    )
    .expect("a contract step may remove");
    assert_eq!(m.kind(), Kind::Contract);
}

fn rows(kinds: &[&str]) -> Vec<(u32, String)> {
    kinds
        .iter()
        .enumerate()
        .map(|(i, k)| (u32::try_from(i + 1).unwrap(), (*k).to_owned()))
        .collect()
}

/// Catches the start check tightened to `found == max`, which would refuse a release on the
/// schema its predecessor left (every update starts there until the installer migrates):
/// a schema behind the newest this build embeds, but not below its minimum, is accepted.
/// Also pins the other edges of the range so that the tolerance is the only thing loosened:
/// below the minimum, no rows, and a contract row above the maximum are refused; an expand
/// row above it is accepted (a reverted release).
///
/// Mutant seen red: `found < supported.min` changed to `found < supported.max`.
#[test]
fn a_schema_behind_the_newest_but_in_range_is_accepted() {
    let range = Supported { min: 2, max: 4 };
    let ex = "expand";
    assert_eq!(judge(&rows(&[ex, ex]), range), Ok(2), "at min, behind max");
    assert_eq!(
        judge(&rows(&[ex, ex, ex]), range),
        Ok(3),
        "in range, behind"
    );
    assert_eq!(judge(&rows(&[ex, ex, ex, ex]), range), Ok(4), "at max");
    assert_eq!(
        judge(&rows(&[ex, ex, ex, ex, ex]), range),
        Ok(5),
        "an expand row above max"
    );
    assert_eq!(
        judge(&rows(&[ex]), range),
        Err(SchemaError::TooOld {
            found: 1,
            min: 2,
            max: 4
        })
    );
    assert_eq!(judge(&[], range), Err(SchemaError::NotInitialised));
    assert_eq!(
        judge(&rows(&[ex, ex, ex, ex, "contract"]), range),
        Err(SchemaError::TooNew {
            found: 5,
            min: 2,
            max: 4,
            contract: 5
        })
    );
}
