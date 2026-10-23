//! The key families of a state store, the digest of each, and the comparison of two stores
//! by them.

use kbf_state_copy::digest::{Counter, Entry, Summary, family_of, summarize};
use sha2::{Digest, Sha256};

fn entry(key: &str, value: &[u8], version: i64, expires_us: Option<i64>) -> Entry {
    Entry {
        key: key.to_owned(),
        value: value.to_vec(),
        version,
        expires_us,
    }
}

fn counter(name: &str, value: i64) -> Counter {
    Counter {
        name: name.to_owned(),
        value,
    }
}

/// The bytes the module docs say one key/value row contributes, written out again here.
fn encoded(row: &Entry) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(row.key.len() as u64).to_be_bytes());
    bytes.extend_from_slice(row.key.as_bytes());
    bytes.extend_from_slice(&(row.value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&row.value);
    bytes.extend_from_slice(&row.version.to_be_bytes());
    bytes.push(u8::from(row.expires_us.is_some()));
    bytes.extend_from_slice(&row.expires_us.unwrap_or(0).to_be_bytes());
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The digest of the one family a summary of `rows` has.
fn digest_of(rows: &[Entry]) -> String {
    let summary = summarize(rows, &[]);
    assert_eq!(summary.keys.len(), 1, "one family");
    summary.keys[0].digest.clone()
}

/// Asserts that a key's family is its first path segment with the slash, and that a key
/// with no slash is a family of its own.
#[test]
fn a_keys_family_is_its_first_segment_with_the_slash() {
    assert_eq!(family_of("op/0001"), "op/");
    assert_eq!(family_of("meta/epoch"), "meta/");
    assert_eq!(family_of("a/b/c"), "a/");
    assert_eq!(family_of("/rooted"), "/");
    assert_eq!(family_of("plain"), "plain");
    assert_eq!(family_of(""), "");
}

/// Asserts that rows are grouped by family, the families come out in name order, and each
/// has the count of its rows, whatever order the rows are given in.
#[test]
fn rows_are_grouped_by_family_in_name_order() {
    let rows = [
        entry("queue/2", b"", 5, None),
        entry("op/1", b"a", 1, None),
        entry("plain", b"p", 2, None),
        entry("op/2", b"b", 3, None),
        entry("lease/x", b"l", 4, None),
    ];
    let summary = summarize(&rows, &[]);
    let found: Vec<(&str, u64)> = summary
        .keys
        .iter()
        .map(|family| (family.name.as_str(), family.row_count))
        .collect();
    assert_eq!(
        found,
        [("lease/", 1), ("op/", 2), ("plain", 1), ("queue/", 1)]
    );
    assert_eq!(summary.key_rows(), 5);
    assert_eq!(summary.counter_rows(), 0);
    assert!(summary.counters.is_empty());
}

/// Asserts that a family's digest is the lower-case hex SHA-256 of its rows in key order in
/// the encoding the module docs give: an 8-byte length before the key and the value, 8
/// bytes for the version, and a presence byte before the 8 bytes of the expiry.
#[test]
fn a_digest_is_the_sha256_of_the_documented_encoding_in_key_order() {
    let first = entry("op/1", b"queued", 7, None);
    let second = entry("op/2", b"", 9, Some(600_000_000));
    let summary = summarize(&[second.clone(), first.clone()], &[]);
    let mut bytes = encoded(&first);
    bytes.extend(encoded(&second));
    let digest = &summary.keys[0].digest;
    assert_eq!(digest, &sha256_hex(&bytes));
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(digest, &digest.to_lowercase());
}

/// Asserts that the digest of a counter family is the SHA-256 of each counter's name behind
/// an 8-byte length and its value in 8 bytes, in name order.
#[test]
fn a_counter_digest_is_the_sha256_of_name_and_value() {
    let summary = summarize(&[], &[counter("seq/b", 2), counter("seq/a", 1)]);
    let mut bytes = Vec::new();
    for (name, value) in [("seq/a", 1_i64), ("seq/b", 2)] {
        bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    assert!(summary.keys.is_empty());
    assert_eq!(summary.counters.len(), 1);
    assert_eq!(summary.counters[0].name, "seq/");
    assert_eq!(summary.counters[0].row_count, 2);
    assert_eq!(summary.counters[0].digest, sha256_hex(&bytes));
    assert_eq!(summary.counter_rows(), 2);
}

/// Asserts that the order the rows are given in does not change any digest.
#[test]
fn the_order_of_the_input_does_not_change_a_digest() {
    let rows = [
        entry("op/1", b"a", 1, None),
        entry("op/2", b"b", 2, Some(5)),
        entry("op/3", b"c", 3, None),
    ];
    let reversed = [rows[2].clone(), rows[1].clone(), rows[0].clone()];
    assert_eq!(summarize(&rows, &[]), summarize(&reversed, &[]));
}

/// Asserts that a change to any one of a row's key, value, version or expiry changes the
/// digest of its family, and that no expiry, an expiry of zero and an expiry of one differ.
#[test]
fn each_field_of_a_row_changes_the_digest() {
    let base = digest_of(&[entry("op/1", b"v", 1, None)]);
    let variants = [
        entry("op/2", b"v", 1, None),
        entry("op/1", b"w", 1, None),
        entry("op/1", b"v", 2, None),
        entry("op/1", b"v", 1, Some(0)),
        entry("op/1", b"v", 1, Some(1)),
    ];
    let mut seen = vec![base];
    for variant in &variants {
        let digest = digest_of(std::slice::from_ref(variant));
        assert!(!seen.contains(&digest), "{variant:?} repeats a digest");
        seen.push(digest);
    }
}

/// Asserts that two rows whose keys and values run together the same way still have
/// different digests, so the lengths are part of what is hashed.
#[test]
fn lengths_keep_the_key_and_the_value_apart() {
    let joined_one = digest_of(&[entry("op/1", b"23", 1, None)]);
    let joined_two = digest_of(&[entry("op/12", b"3", 1, None)]);
    assert_ne!(joined_one, joined_two);
}

/// Asserts that a counter and a key/value row with the same name are in separate sets of
/// families, and that a counter's value is part of its digest.
#[test]
fn counters_and_key_value_rows_are_separate_sets() {
    let rows = [entry("seq/a", b"", 1, None)];
    let one = summarize(&rows, &[counter("seq/a", 1)]);
    let two = summarize(&rows, &[counter("seq/a", 2)]);
    assert_eq!(one.keys, two.keys);
    assert_ne!(one.counters, two.counters);
    assert_eq!(one.key_rows(), 1);
    assert_eq!(one.counter_rows(), 1);
}

/// Asserts that two equal summaries differ nowhere.
#[test]
fn equal_summaries_have_no_differences() {
    let rows = [entry("op/1", b"a", 1, None)];
    let counters = [counter("seq/a", 1)];
    let summary = summarize(&rows, &counters);
    let same = summarize(&rows, &counters);
    let none: Vec<String> = Vec::new();
    assert_eq!(summary.differences(&same), none);
    let empty = Summary::default();
    assert_eq!(empty.differences(&Summary::default()), none);
}

/// Asserts that a family whose row changed is reported by name, with the count and digest
/// each side has, and that a family that matches is not reported.
#[test]
fn a_changed_value_is_reported_against_its_family() {
    let state_rows = [
        entry("op/1", b"a", 1, None),
        entry("lease/1", b"x", 2, None),
    ];
    let copy_rows = [
        entry("op/1", b"b", 1, None),
        entry("lease/1", b"x", 2, None),
    ];
    let state = summarize(&state_rows, &[]);
    let copy = summarize(&copy_rows, &[]);
    let found = state.differences(&copy);
    assert_eq!(found.len(), 1, "{found:?}");
    let (old, new) = (&state.keys[1], &copy.keys[1]);
    assert_eq!(old.name, "op/");
    let want = format!(
        "key family op/: state file 1 rows, digest {}, copy 1 rows, digest {}",
        old.digest, new.digest
    );
    assert_eq!(found[0], want);
}

/// Asserts that a family missing from the copy, and one the copy has and the file does not,
/// are each reported as absent on the side that lacks it, in both sets.
#[test]
fn a_missing_and_an_extra_family_are_reported_as_absent() {
    let state_rows = [entry("op/1", b"a", 1, None)];
    let copy_rows = [entry("extra/1", b"a", 1, None)];
    let state = summarize(&state_rows, &[counter("seq/a", 1)]);
    let copy = summarize(&copy_rows, &[counter("other/a", 1)]);
    let found = state.differences(&copy);
    assert_eq!(found.len(), 4, "{found:?}");
    assert!(found[0].starts_with("key family extra/: state file absent, copy 1 rows"));
    assert!(found[1].starts_with("key family op/: state file 1 rows"));
    assert!(found[1].ends_with(", copy absent"));
    assert!(found[2].starts_with("counter family other/: state file absent"));
    assert!(found[3].starts_with("counter family seq/: state file 1 rows"));
}

/// Asserts that the table lists each set of families with its name padded to the longest,
/// the count right-aligned and the digest, and that an empty store has the two headings.
#[test]
fn the_table_lists_each_family_with_its_count_and_digest() {
    let rows = [
        entry("a/x", b"1", 1, None),
        entry("longer/y", b"2", 2, None),
    ];
    let summary = summarize(&rows, &[counter("c/n", 3)]);
    let [a, longer] = &summary.keys[..] else {
        panic!("two families expected");
    };
    let c = &summary.counters[0];
    let want = format!(
        "key families (rows, digest):\n  {:<7}  {:>6}  {}\n  {:<7}  {:>6}  {}\n\
         counter families (rows, digest):\n  {:<2}  {:>6}  {}\n",
        a.name,
        a.row_count,
        a.digest,
        longer.name,
        longer.row_count,
        longer.digest,
        c.name,
        c.row_count,
        c.digest
    );
    assert_eq!(summary.table(), want);
    let empty = "key families (rows, digest):\ncounter families (rows, digest):\n";
    assert_eq!(Summary::default().table(), empty);
}
