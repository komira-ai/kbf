//! The `update/*` rows of the desired release (`docs/design/pull-updates.md` section 6.2):
//! their bytes, frozen, and what reading and writing them does on the in-memory fake and on a
//! SQLite file. The host and place rows older builds wrote are read by
//! `kbf_server::stored_update` (`tests/stored_update.rs`).
//!
//! The goldens below were worked out by hand from the protobuf wire format (a key is
//! `tag << 3 | wire type`, a varint is seven bits at a time with the high bit saying more
//! follow, a zero is not written), not copied from the code's output: a row that changes its
//! bytes changes what a store written by the last release means to this one.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{Desired, Generation, Policy, Release, RowError, Rows, Source};
use kbf_state::{Cond, Txn};
use kbf_types::FarmTime;
use std::time::Duration;
use support::harness::{Backing, Cell};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// A policy row with every agreed field set (the first eight, in the design's order of the
/// time, 1 to 8), then two of the node step's limits (the write limit is 13, the rebuild
/// limit 16), and not the pause, which came later and is 18.
const POLICY_EVERY_FIELD: &str = "0801100118b0ea012002280430c0cf24380140016890bf058001a0f736";

/// The pause as it is written: a varint key of two bytes (18 << 3 = 144 = 0x90 0x01) and the
/// value 1.
const PAUSED: &str = "900101";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn raw(cell: &Cell, key: &str) -> String {
    let row = cell
        .store()
        .get(key)
        .expect("a read")
        .expect("the row exists");
    hex(&row.value)
}

fn put_raw(cell: &Cell, key: &str, value: &[u8]) {
    let txn = Txn::new().put(key, value.to_vec(), Cond::Any);
    cell.store()
        .txn(txn)
        .expect("a write")
        .expect("no condition");
}

/// Offers `commit` from `source` at `at`, expecting no particular generation.
fn offer_from(rows: &Rows, commit: &str, source: Source, at: FarmTime) -> Desired {
    let release = Release::new(commit);
    rows.offer_expecting(&release, source, at, None).unwrap()
}

// The bytes.

/// Catches: a row whose bytes move: a field renumbered, a default written where it was
/// left out, an enum numbered by its Rust order. Each row, with a value that sets every
/// field, against the bytes the wire format gives it. The policy is written only by a pause,
/// which rewrites the row it read, so each policy below starts from the bytes an older build
/// left and ends as the same bytes with the pause set or cleared.
#[test]
fn the_bytes_of_every_row_are_frozen() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "frozen");
        let rows = Rows::new(cell.store());

        // update/desired {generation, commit, set_at}: generation 1, "v2", set at 1000 ms.
        // (The pause is in update/policy, below.)
        let now = FarmTime::from_millis(1000);
        rows.offer(&Release::new("v2"), now).expect("an offer");
        assert_eq!(raw(&cell, "update/desired"), "08011202763218e807");

        // update/policy on a store with none: the defaults with the pause set, of which only
        // `auto_nodes` (true by default, a plain bool in the row) is written besides it.
        rows.set_paused(true).expect("a pause");
        assert_eq!(raw(&cell, "update/policy"), format!("0801{PAUSED}"));

        // The defaults with a floor of one, three places and a Mac force: only the fields
        // that are not the default are written. `auto_nodes` is true in the defaults and a
        // plain bool in the row (absent is false, as every build has read it), so it is the
        // first field.
        put_raw(&cell, "update/policy", &unhex("08012001280330c0ee6d"));
        rows.set_paused(false).expect("a resume");
        assert_eq!(raw(&cell, "update/policy"), "08012001280330c0ee6d");

        // Every agreed field set, then two limits: the pause is appended last, and cleared
        // again it leaves the same bytes.
        put_raw(&cell, "update/policy", &unhex(POLICY_EVERY_FIELD));
        rows.set_paused(true).expect("a pause");
        assert_eq!(
            raw(&cell, "update/policy"),
            format!("{POLICY_EVERY_FIELD}{PAUSED}")
        );
        rows.set_paused(false).expect("a resume");
        assert_eq!(raw(&cell, "update/policy"), POLICY_EVERY_FIELD);
        let every = Policy {
            auto_nodes: true,
            auto_frontend: true,
            paused: false,
            idle_wait: Duration::from_secs(30),
            min_serving: 2,
            max_hosts_out: 4,
            mac_force_after: Some(Duration::from_secs(600)),
            new_work_only_to_updated: true,
            accept_failing_builds: true,
            write_limit: Duration::from_secs(90),
            rebuild_limit: Some(Duration::from_secs(900)),
            ..Policy::default()
        };
        assert_eq!(rows.policy().expect("a read"), every);
    }
}

/// Catches: a staged source written under the wrong tag, number or place. The frozen bytes
/// of the desired row above, followed by the source: `28 01` (tag 5, the next after the
/// retired 4), which reads back as staged. The fetch row above is the same bytes without it.
#[test]
fn a_staged_row_ends_with_its_source_and_reads_back_staged() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "staged-bytes");
        let rows = Rows::new(cell.store());

        let now = FarmTime::from_millis(1000);
        let offered = offer_from(&rows, "v2", Source::Staged, now);
        assert_eq!(raw(&cell, "update/desired"), "08011202763218e8072801");
        assert_eq!(rows.desired().unwrap().value.unwrap(), offered);
    }
}

/// Catches: a row written before the source existed, or by a writer that does not know it,
/// read as anything but a fetch; and a zero source made `Corrupt` (the rule for every other
/// enum of these rows, which `source` deliberately does not follow). A desired row with no
/// tag 5 reads as fetch, and so does one that says 0 outright.
#[test]
fn a_row_with_no_source_reads_as_a_fetch() {
    let cell = Cell::new(Backing::Fake, "legacy");
    let rows = Rows::new(cell.store());
    // The bytes of the first frozen desired row above, which has no tag 5.
    put_raw(
        &cell,
        "update/desired",
        &[0x08, 0x01, 0x12, 0x02, 0x76, 0x32, 0x18, 0xe8, 0x07],
    );
    let legacy = rows.desired().unwrap().value.unwrap();
    assert_eq!(legacy.source, Source::Fetch);
    assert_eq!(legacy.generation, Generation(1));
    // The same with the source said outright as 0.
    put_raw(
        &cell,
        "update/desired",
        &[0x08, 0x01, 0x12, 0x02, 0x76, 0x32, 0x28, 0x00],
    );
    let zero = rows.desired().unwrap().value.unwrap();
    assert_eq!(zero.source, Source::Fetch);
}

/// Catches: a source serialized as anything but its lowercase word, or whose default is not
/// a fetch (what a row that names none reads as). Both words are written and read, so each
/// variant of the derived impls runs.
#[test]
fn a_source_is_its_lowercase_word_and_defaults_to_fetch() {
    let staged = serde_json::to_string(&Source::Staged).unwrap();
    assert_eq!(staged, "\"staged\"");
    let fetch: Source = serde_json::from_str("\"fetch\"").unwrap();
    assert_eq!(fetch, Source::Fetch);
    let bogus = serde_json::from_str::<Source>("\"bogus\"");
    assert!(bogus.is_err());
    let fetch_word = serde_json::to_string(&Source::Fetch).unwrap();
    assert_eq!(fetch_word, "\"fetch\"");
    let staged_back: Source = serde_json::from_str("\"staged\"").unwrap();
    assert_eq!(staged_back, Source::Staged);
    assert_eq!(Source::default(), Source::Fetch);
}

/// Catches: a policy row that only knows the eight agreed fields, as the operator API
/// wrote it, read with the node step's limits at zero or `max_hosts_out` at the invalid 0.
/// Absent is the default; an explicit zero is a zero.
#[test]
fn a_policy_row_with_only_the_agreed_fields_leaves_the_limits_at_their_defaults() {
    let cell = Cell::new(Backing::Fake, "agreed");
    let rows = Rows::new(cell.store());
    // auto_nodes on, a floor of two: nothing else written.
    put_raw(&cell, "update/policy", &[0x08, 0x01, 0x20, 0x02]);
    let read = rows.policy().expect("a read");
    assert_eq!(
        read,
        Policy {
            auto_nodes: true,
            min_serving: 2,
            ..Policy::default()
        }
    );
    assert_eq!(read.max_hosts_out, 1, "absent is the default of one");
    // An explicit zero is read as a zero.
    put_raw(&cell, "update/policy", &[0x28, 0x00]);
    let zero = rows.policy().expect("a read");
    assert_eq!(zero.max_hosts_out, 0);
}

/// Catches: the default of `auto_nodes` moving from false to true changing what a policy row
/// written by a build before it means. Those builds wrote no `auto_nodes` field when it was
/// false (a plain bool, absent is false), so the stored bytes of a floor of one, three places
/// and a Mac force, `2001280330c0ee6d`, are the same row as before the change and must still
/// read as `auto_nodes` false with every other field as it was: an operator who wrote
/// manual is not switched to automatic by an upgrade.
#[test]
fn a_policy_row_written_before_auto_nodes_defaulted_true_still_reads_false() {
    let cell = Cell::new(Backing::Fake, "old-policy");
    let rows = Rows::new(cell.store());
    put_raw(
        &cell,
        "update/policy",
        &[0x20, 0x01, 0x28, 0x03, 0x30, 0xc0, 0xee, 0x6d],
    );
    assert_eq!(
        rows.policy().expect("a read"),
        Policy {
            auto_nodes: false,
            min_serving: 1,
            max_hosts_out: 3,
            mac_force_after: Some(Duration::from_secs(1800)),
            ..Policy::default()
        }
    );
    assert_eq!(
        rows.policy_if_set().expect("a read").map(|p| p.auto_nodes),
        Some(false)
    );
    // What an old frontend's `set_paused` stores on a policy never written: the pause alone
    // (`pull-updates.md`, mixed versions). Auto is off in it, and stays off.
    put_raw(&cell, "update/policy", &[0x90, 0x01, 0x01]);
    let paused = rows.policy().expect("a read");
    assert_eq!((paused.paused, paused.auto_nodes), (true, false));
}

/// Catches: a reader that guesses. A run with no commit, one with no generation (generation
/// 0 is what a host no run has touched carries, so a run of it would read every host as
/// having finished it), a source above staged (2), and bytes that are no message are each
/// refused with the row's name, never read as a fetch or as the nearest run.
#[test]
fn a_row_this_build_cannot_read_is_refused_not_guessed() {
    let cell = Cell::new(Backing::Fake, "corrupt");
    let rows = Rows::new(cell.store());
    let bad_runs: [(&str, &[u8]); 4] = [
        ("not a message", &[0xff]),
        ("no commit", &[0x08, 0x01]),
        ("no generation", &[0x12, 0x02, 0x76, 0x32]),
        (
            "a source from the future",
            &[0x08, 0x01, 0x12, 0x02, 0x76, 0x32, 0x28, 0x02],
        ),
    ];
    for (what, bytes) in bad_runs {
        put_raw(&cell, "update/desired", bytes);
        let Err(RowError::Corrupt { key, .. }) = rows.desired() else {
            panic!("{what}: the row was read");
        };
        assert_eq!(key, "update/desired", "{what}");
    }
    put_raw(&cell, "update/policy", &[0xff]);
    assert!(matches!(rows.policy(), Err(RowError::Corrupt { .. })));
    assert!(matches!(
        rows.policy_if_set(),
        Err(RowError::Corrupt { .. })
    ));
}

/// Catches: a reader that cannot take a row a later release wrote with one more field
/// (state-store rows are frozen across one step, section 3.2): the field is skipped and the
/// rest read as before.
#[test]
fn a_field_added_later_is_skipped() {
    let cell = Cell::new(Backing::Fake, "newer");
    let rows = Rows::new(cell.store());
    let now = FarmTime::from_millis(1000);
    let offered = offer_from(&rows, "v2", Source::Fetch, now);
    let mut bytes = cell.store().get("update/desired").unwrap().unwrap().value;
    // Field 15, a varint, and field 16, a string.
    bytes.extend_from_slice(&[0x78, 0x2a, 0x82, 0x01, 0x02, 0x68, 0x69]);
    put_raw(&cell, "update/desired", &bytes);
    assert_eq!(rows.desired().unwrap().value.unwrap(), offered);
}

// The rows.

/// Catches: an offer that reuses a generation or touches the pause; a rollback that is not an
/// offer; an offer with no release. A generation only grows (the first is 1), the clock is
/// kept as asked, and a pause (which is in `update/policy`, needs no run and is not an
/// offer's to clear) stands across offers; the same on both backends.
#[test]
fn offers_grow_the_generation_and_leave_the_pause_alone() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "offer");
        let rows = Rows::new(cell.store());
        assert_eq!(rows.desired().unwrap().value, None);
        rows.set_paused(true).unwrap();
        assert!(rows.policy().unwrap().paused, "a pause needs no run");
        assert_eq!(rows.desired().unwrap().value, None, "and starts none");
        let at = FarmTime::from_millis(42);
        let first = rows.offer(&Release::new("v2"), at).unwrap();
        assert_eq!(
            first,
            Desired {
                generation: Generation(1),
                commit: Release::new("v2"),
                set_at: at,
                source: Source::Fetch,
            }
        );
        assert!(
            rows.policy().unwrap().paused,
            "an offer does not resume a paused run"
        );
        let back = rows
            .offer(&Release::new("v1"), FarmTime::from_millis(50))
            .unwrap();
        assert_eq!(back.generation, Generation(2));
        assert!(rows.policy().unwrap().paused);
        rows.set_paused(false).unwrap();
        assert!(!rows.policy().unwrap().paused);
        assert_eq!(
            rows.offer(&Release::new(""), at),
            Err(RowError::NoRelease),
            "an offer names a release"
        );
        assert_eq!(
            rows.desired().unwrap().value.unwrap().generation,
            Generation(2)
        );
    }
}

/// Catches: a repeated click that starts a new run (`pull-updates.md` section 5: setting the
/// commit that is already desired changes nothing and keeps the generation, and the time it
/// was set), and a generation that is not above the current run's when the counter it is
/// drawn from is behind the row (a row written without the counter, or a counter restored
/// from an older backup than the row). Setting the older commit again afterwards is a new
/// run: that is the rollback. The run is the pair of commit and source: the same commit from
/// another source is a new run, and the same pair again is not.
#[test]
fn the_commit_already_desired_keeps_its_generation_and_a_generation_is_always_above_the_row() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "same");
        let rows = Rows::new(cell.store());
        let first = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(10))
            .unwrap();
        let again = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(99))
            .unwrap();
        assert_eq!(again, first, "the same commit changes nothing");
        assert_eq!(rows.desired().unwrap().value.unwrap(), first);
        let back = rows
            .offer(&Release::new("v1"), FarmTime::from_millis(20))
            .unwrap();
        assert_eq!(
            back.generation,
            Generation(2),
            "the repeat did not spend the counter"
        );
        let forward = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(30))
            .unwrap();
        assert_eq!(forward.generation, Generation(3), "a rollback is a new run");

        // A row from a counter that is ahead of this store's: generation 90.
        let ahead = Desired {
            generation: Generation(90),
            commit: Release::new("x"),
            set_at: FarmTime::from_millis(1),
            source: Source::Fetch,
        };
        put_raw(
            &cell,
            "update/desired",
            &[0x08, 0x5a, 0x12, 0x01, 0x78, 0x18, 0x01],
        );
        assert_eq!(rows.desired().unwrap().value.unwrap(), ahead);
        let next = rows
            .offer(&Release::new("y"), FarmTime::from_millis(40))
            .unwrap();
        assert_eq!(
            next.generation,
            Generation(91),
            "above the row, whatever the counter says"
        );

        // The same commit from another source is another run, the same pair again is not.
        let staged = offer_from(&rows, "y", Source::Staged, FarmTime::from_millis(50));
        assert_eq!(staged.source, Source::Staged);
        assert_eq!(
            staged.generation,
            Generation(92),
            "the same commit from another source is a new run"
        );
        let repeat = offer_from(&rows, "y", Source::Staged, FarmTime::from_millis(60));
        assert_eq!(repeat, staged, "the same pair changes nothing");
        assert_eq!(rows.desired().unwrap().value.unwrap(), staged);
        let fetch = offer_from(&rows, "y", Source::Fetch, FarmTime::from_millis(70));
        assert_eq!(fetch.source, Source::Fetch);
        assert_eq!(
            fetch.generation,
            Generation(93),
            "and back to a fetch is another"
        );
    }
}

/// Catches: a missing policy row read as anything but the design's defaults, and a pause
/// that rewrites the policy it sits in (the other settings back at their defaults after a
/// pause), or a resume that does not clear it. On an empty store a pause is the defaults with
/// `paused` set; on a row an older build wrote, every other setting and limit stays as it was
/// through a pause and a resume.
#[test]
fn a_pause_changes_only_the_pause() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "pause");
        let rows = Rows::new(cell.store());
        assert_eq!(rows.policy().unwrap(), Policy::default());
        assert_eq!(rows.policy_if_set().unwrap(), None);
        rows.set_paused(true).unwrap();
        let paused_defaults = Policy {
            paused: true,
            ..Policy::default()
        };
        assert_eq!(rows.policy().unwrap(), paused_defaults);
        rows.set_paused(false).unwrap();
        assert_eq!(rows.policy().unwrap(), Policy::default());

        put_raw(&cell, "update/policy", &unhex(POLICY_EVERY_FIELD));
        let every = rows.policy().unwrap();
        rows.set_paused(true).unwrap();
        let paused = Policy {
            paused: true,
            ..every.clone()
        };
        assert_eq!(rows.policy().unwrap(), paused);
        rows.set_paused(false).unwrap();
        assert_eq!(rows.policy().unwrap(), every);
    }
}

/// Catches: rows that do not survive the store being closed and opened again, which is
/// what a frontend restart is: every row comes back as written.
#[test]
fn every_row_survives_the_store_being_reopened() {
    let mut cell = Cell::new(Backing::Sqlite, "reopen");
    {
        let rows = Rows::new(cell.store());
        rows.offer(&Release::new("v2"), FarmTime::from_millis(7))
            .unwrap();
        rows.set_paused(true).unwrap();
    }
    cell.reopen();
    let rows = Rows::new(cell.store());
    assert_eq!(
        rows.desired().unwrap().value.unwrap().commit,
        Release::new("v2")
    );
    assert!(rows.policy().unwrap().paused);
}
