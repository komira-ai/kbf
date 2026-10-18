//! The migration lint and the embedded set. No database: the tests that need a server are
//! `tests/postgres_schema.rs`.

use super::{Kind, MIN_SUPPORTED, Migration, SchemaError, embedded, latest};

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
        "UPDATE counters SET value = value + 0;",
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
