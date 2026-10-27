//! The migration lint and the embedded set. No database: the tests that need a server are
//! `tests/postgres_schema.rs`.

use std::time::Duration;

use super::{
    CONNECT_LIMIT, Kind, MIN_SUPPORTED, Migration, SchemaError, Supported, connect_settings,
    embedded, extend, judge, latest, latest_of,
};
use crate::postgres::PostgresConfig;

fn expand(body: &str) -> Result<Migration, SchemaError> {
    Migration::new(2, "t", &format!("-- kbf-migration: expand\n{body}"))
}

/// Catches an embedded migration that would break the release still serving, or a set with
/// a hole in its numbering, shipping in a release: the lint is the review rule made
/// mechanical. Catches: the embedded file given a `DROP` (`Destructive`).
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

/// A migration of a crate that extends the schema, as such a crate ships it.
fn extra(version: u32, name: &str) -> Migration {
    Migration::new(
        version,
        name,
        "-- kbf-migration: expand\nCREATE TABLE extra_t (id bigint PRIMARY KEY);\n",
    )
    .expect("an additive migration lints")
}

/// Catches an extension set that is not the base followed by the extension in order: the
/// base migrations must keep their numbers and names (an existing database has recorded them)
/// and the extension continues the numbering; `latest_of` is the newest of the extended set.
#[test]
fn an_extension_continues_the_numbering_and_leaves_the_base_as_it_was() {
    let base = embedded();
    let set = extend(base.clone(), [extra(latest() + 1, "extra")]).expect("continues");
    assert_eq!(&set[..base.len()], base.as_slice());
    assert_eq!(set.len(), base.len() + 1);
    assert_eq!(set[base.len()].name(), "extra");
    assert_eq!(latest_of(&set), latest() + 1);
    assert_eq!(latest_of(&[]), 0);
}

/// Catches an extension that repeats a number of the base or leaves a gap being accepted
/// (the runner records a migration by its number, so either would skip or collide).
#[test]
fn an_extension_that_repeats_or_skips_a_number_is_refused() {
    let repeated = extend(embedded(), [extra(latest(), "extra")]);
    assert_eq!(
        repeated,
        Err(SchemaError::Numbering {
            expected: latest() + 1,
            found: latest()
        })
    );
    let gap = extend(embedded(), [extra(latest() + 2, "extra")]);
    assert_eq!(
        gap,
        Err(SchemaError::Numbering {
            expected: latest() + 1,
            found: latest() + 2
        })
    );
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
/// Catches: each of these statements passed before the rules were added.
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
/// Catches: `found < supported.min` changed to `found < supported.max`.
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

/// Catches a migration kind written to `schema_version` under the wrong name, and the
/// accessors reading the wrong field: the recorded kind text is what `judge` compares.
#[test]
fn a_migration_reports_its_name_and_the_kind_text_recorded() {
    assert_eq!(Kind::Expand.as_str(), "expand");
    assert_eq!(Kind::Contract.as_str(), "contract");
    let m = Migration::new(2, "widen", "-- kbf-migration: contract\nDROP TABLE old;")
        .expect("a contract step may drop");
    assert_eq!(m.name(), "widen");
    assert_eq!(m.version(), 2);
}

/// Catches the outcome text of `setup` naming the wrong case: created, current, migrated.
#[test]
fn the_outcome_says_created_current_or_migrated() {
    use super::Outcome;
    let text = |from, ran: Vec<u32>| Outcome { from, to: 3, ran }.to_string();
    assert_eq!(text(None, vec![1, 2, 3]), "created the schema at version 3");
    assert_eq!(
        text(Some(3), vec![]),
        "the schema is at version 3, nothing to apply"
    );
    assert_eq!(text(Some(1), vec![2, 3]), "migrated the schema from 1 to 3");
}

/// Catches the lint's word scanner losing its place: a nested block comment hides a DROP
/// until its outer end, a doubled quote does not end a string early (the DROP after it is
/// still seen), a comment at the end of the file with no newline and an unterminated string
/// end the scan without a panic or a phantom word.
#[test]
fn the_scanner_handles_nested_comments_doubled_quotes_and_unterminated_text() {
    for sql in [
        "ALTER TABLE kv ADD COLUMN a text; /* outer /* DROP TABLE kv */ DELETE */ SELECT 1;",
        "COMMENT ON TABLE kv IS 'it''s a DROP';",
        "SELECT 1; -- DROP TABLE kv",
        "SELECT 'DROP TABLE kv",
    ] {
        assert!(expand(sql).is_ok(), "{sql}: {:?}", expand(sql));
    }
    assert!(expand("COMMENT ON TABLE kv IS 'it''s'; DROP TABLE kv;").is_err());
}

/// Catches an `ALTER ... TYPE` check that trips on a statement that also says CREATE, and an
/// UPDATE after a `(`, `ELSE` or `LOOP` slipping through as not a row rewrite.
#[test]
fn alter_type_beside_create_is_not_a_type_change_and_nested_updates_are_refused() {
    assert!(
        expand("ALTER TABLE kv ADD COLUMN type int DEFAULT 0, ADD COLUMN create int DEFAULT 0;")
            .is_ok()
    );
    for sql in [
        "DO $$ BEGIN IF a THEN UPDATE kv SET v = 1; END IF; END $$;",
        "DO $$ BEGIN IF a THEN NULL; ELSE UPDATE kv SET v = 1; END IF; END $$;",
        "DO $$ BEGIN LOOP UPDATE kv SET v = 1; END LOOP; END $$;",
    ] {
        assert!(expand(sql).is_err(), "{sql}");
    }
}

/// Catches a unique index on a table whose name the lint could not read (a quoted name
/// leaves nothing) being allowed: the lint fails closed.
#[test]
fn a_unique_index_on_a_table_with_a_quoted_name_is_refused() {
    assert!(expand("CREATE TABLE \"q\" (a int);\nCREATE UNIQUE INDEX i ON \"q\" (a);").is_err());
}

/// Catches a bounded call that can wait for ever on a server that accepts the connection
/// and never answers: the bounded call carries a connect timeout of exactly 10 s, and an
/// unbounded call (`apply`) carries none.
///
/// Catches: the `connect_timeout` call removed; `CONNECT_LIMIT` made 3000 s; the
/// bound applied to the unbounded call.
#[test]
fn a_bounded_call_connects_with_the_limit_and_an_unbounded_one_does_not() {
    let config = PostgresConfig::parse("postgres:///kbf?host=/run/postgresql").expect("parses");
    assert_eq!(CONNECT_LIMIT, Duration::from_secs(10));
    let bounded = connect_settings(&config.pg, Some(Duration::from_secs(1)));
    assert_eq!(bounded.get_connect_timeout(), Some(&CONNECT_LIMIT));
    assert_eq!(
        connect_settings(&config.pg, None).get_connect_timeout(),
        None
    );
}
