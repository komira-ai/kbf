//! `profile_name` and `HostNames`.

mod support;

use std::collections::BTreeSet;

use js_types::HostId;
use kbf_estimator::Basis;
use kbf_types::WorkerId;
use kbf_usage::{HostNames, profile_name};
use support::{base_key, key};

fn well_formed(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// Catches a name that is not stable between runs (a hash with a per-process seed, a
// pointer), which would orphan every stored profile at a restart, and a name that
// differs between two equal keys.
#[test]
fn equal_keys_have_equal_names() {
    let a = profile_name(&base_key());
    let b = profile_name(&key("main", "linux-x86_64", Basis::Cgroup, 7, Some(9)));
    assert_eq!(a, b);
    assert_eq!(a, profile_name(&base_key()));
}

// Catches a change to the hashed bytes (a renamed domain, a reordered field, another
// width): the stored profiles are named by these values, so a change forgets them all.
#[test]
fn names_are_pinned() {
    let cases = [
        (
            base_key(),
            "c2835ee737c3fca21b38ff3117535c4389cead7645ab49c2d0c1eaae2c6ed486",
        ),
        (
            key("main", "linux-x86_64", Basis::Cgroup, 7, None),
            "7a2ca4aea088acefb0d17e3bcc72253d543328c2653d65f2f695ec25da2a27b8",
        ),
        (
            key("", "", Basis::MacFootprint, 0, Some(0)),
            "27a7c7de7714915aa37bb3c49e91df07c1982d134fbad68db1f852efd376b2cf",
        ),
    ];
    for (key, expected) in cases {
        assert_eq!(profile_name(&key).as_str(), expected);
    }
}

// Catches a name that ignores one part of the key: each key below differs from the base
// in exactly one part, and all of them (and the base) must name different profiles.
#[test]
fn a_change_to_any_part_of_the_key_changes_the_name() {
    let variants = [
        base_key(),
        key("other", "linux-x86_64", Basis::Cgroup, 7, Some(9)),
        key("main", "linux-aarch64", Basis::Cgroup, 7, Some(9)),
        key("main", "linux-x86_64", Basis::MacFootprint, 7, Some(9)),
        key("main", "linux-x86_64", Basis::Cgroup, 8, Some(9)),
        key("main", "linux-x86_64", Basis::Cgroup, 7, Some(10)),
        key("main", "linux-x86_64", Basis::Cgroup, 7, None),
        key("main", "linux-x86_64", Basis::Cgroup, 7, Some(0)),
    ];
    let names: BTreeSet<String> = variants
        .iter()
        .map(|key| profile_name(key).as_str().to_owned())
        .collect();
    assert_eq!(names.len(), variants.len());
}

// Catches fields joined without a length: ("ab", "c") and ("a", "bc") would hash alike.
#[test]
fn text_parts_do_not_run_into_each_other() {
    let a = key("ab", "c", Basis::Cgroup, 1, None);
    let b = key("a", "bc", Basis::Cgroup, 1, None);
    assert_ne!(profile_name(&a), profile_name(&b));
}

// Catches a name outside the profile grammar, or one that starts with a character the
// scheduler's ids are not to start with: 64 lowercase hex digits, over many keys.
#[test]
fn every_name_is_lowercase_hex_and_starts_with_a_letter_or_digit() {
    for shape in 0..200 {
        let name = profile_name(&key("i", "p", Basis::Cgroup, shape, Some(shape * 3)));
        assert!(well_formed(name.as_str()), "{name}");
        let first = name.as_str().as_bytes()[0];
        assert!(first.is_ascii_digit() || first.is_ascii_lowercase());
    }
}

fn worker(text: &str) -> WorkerId {
    WorkerId::new(text)
}

// Catches a valid id that is renamed (two nodes then share nothing with their own ids,
// and the operator cannot match a series to a node), and a fallback counted for it.
#[test]
fn a_valid_worker_id_is_the_host_name() {
    let names = HostNames::new();
    for id in ["w1", "node-3.rack_2", "0abc", &"a".repeat(63)] {
        assert_eq!(
            names.host_name(&worker(id)),
            HostId::new(id).unwrap(),
            "{id}"
        );
    }
    assert_eq!(names.renamed(), 0);
}

// Catches an invalid id that is lowercased, has its characters replaced, is cut, or is
// passed through to a host id that refuses it: each gets "w-" and 32 hex digits, the same
// on every call, and no two of them share a name.
#[test]
fn an_invalid_worker_id_gets_a_deterministic_hashed_name() {
    let names = HostNames::new();
    let invalid = [
        "Worker/1".to_owned(),
        "WORKER".to_owned(),
        "worker".to_owned() + "/1",
        String::new(),
        "a".repeat(64),
        "a".repeat(129),
        "-leading-dash".to_owned(),
        ".leading-dot".to_owned(),
        "_underscore".to_owned(),
        "caf\u{e9}".to_owned(),
        "has space".to_owned(),
    ];
    let mut seen = BTreeSet::new();
    for id in &invalid {
        let host = names.host_name(&worker(id));
        let text = host.as_str();
        assert!(
            text.starts_with("w-") && text.len() == 34,
            "{id:?} -> {text}"
        );
        assert!(
            text[2..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(
            names.host_name(&worker(id)),
            host,
            "{id:?} is not deterministic"
        );
        assert!(seen.insert(text.to_owned()), "{id:?} collides");
    }
    assert_eq!(names.renamed(), 2 * invalid.len() as u64);
}

// Catches a change to the fallback's hashed bytes, which would rename every node whose id
// is invalid at an upgrade.
#[test]
fn the_fallback_name_is_pinned() {
    let names = HostNames::new();
    assert_eq!(
        names.host_name(&worker("Worker/1")).as_str(),
        "w-04142b4f94a8cbb978055b8f4bd224d8"
    );
    assert_eq!(
        names.host_name(&worker("")).as_str(),
        "w-80e5f8249c6438d1e1f98d1679c6e613"
    );
}

// Catches a counter that counts every call (valid ids too) or one that is shared between
// namers.
#[test]
fn the_counter_counts_only_the_calls_that_fell_back() {
    let names = HostNames::new();
    let other = HostNames::default();
    let _ = names.host_name(&worker("ok"));
    assert_eq!(names.renamed(), 0);
    let _ = names.host_name(&worker("NOT OK"));
    let _ = names.host_name(&worker("NOT OK"));
    let _ = names.host_name(&worker("fine"));
    assert_eq!(names.renamed(), 2);
    assert_eq!(other.renamed(), 0);
}
