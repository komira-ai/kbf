//! The seed's manifest, the floor and the two rules an order meets before the pipeline is
//! asked, and where the package keeps its files.

use std::fs;

use kbf_host::path::Root;
use serde_json::json;

use super::*;
use crate::layout::Platform;
use crate::seed_fixture::{SEQ, Seeded, sha};
use crate::testkit::{A, B, c};

/// The manifest as the package builder writes it, as bytes.
fn bytes(seeded: &Seeded) -> Vec<u8> {
    serde_json::to_vec(&seeded.manifest).expect("json")
}

/// The manifest with `change` applied to its keys, as bytes.
fn changed(seeded: &Seeded, change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut manifest = seeded.manifest.clone();
    change(&mut manifest);
    serde_json::to_vec(&manifest).expect("json")
}

/// A commit that shares the first 12 digits with `B` and differs after them: the name of the
/// seed directory is the same.
fn same_build_other_commit() -> CommitId {
    c(&format!("{}{}", &B[..12], "f".repeat(28)))
}

/// A manifest is the closed set of keys the builder writes, each of the right type, with a
/// platform a release is built for: it reads back as what it says (the commit, the `seq`, the
/// facts and the file names the seed's files have), and each of these is refused with the key
/// or the rule named: a key it does not know, a key missing, a digest that is not 64 lowercase
/// hex digits, a commit that is not 40, a platform no release is built for, text that is not
/// JSON, and a file over the cap.
///
/// Catches a manifest that accepts keys it does not understand (a later builder's meaning
/// ignored), a digest or commit taken in a shape the rest of the installer refuses, and a
/// platform that would name a file the seed does not have.
///
/// Mutants to plant: `deny_unknown_fields` removed; the platform check removed; the size cap
/// raised past the oversize file.
#[test]
fn a_manifest_is_a_closed_set_of_keys_and_reads_back_as_what_it_says() {
    let seeded = Seeded::scratch("seed-manifest");
    let manifest = Manifest::parse(&bytes(&seeded)).expect("parses");
    assert_eq!(manifest.commit, c(B));
    assert_eq!(manifest.seq, SEQ);
    assert_eq!(manifest.facts(), seeded.facts());
    assert_eq!(
        manifest.seed_facts(),
        SeedFacts {
            commit: c(B),
            seq: SEQ,
            facts: seeded.facts(),
        }
    );
    assert_eq!(
        manifest.tarball_name(),
        format!("kbf-{B}-linux-x86_64.tar.gz")
    );
    assert_eq!(
        manifest.bundle_name(),
        format!("kbf-{B}-linux-x86_64.tar.gz.sigstore.json")
    );
    assert_eq!(manifest.tarball_sha256.as_str(), sha(&seeded.tarball));
    assert_eq!(manifest.tarball_size, seeded.tarball.len() as u64);

    type Break = (&'static str, fn(&mut serde_json::Value), &'static str);
    let breaks: [Break; 8] = [
        ("an unknown key", |m| m["extra"] = json!(1), "`extra`"),
        (
            "a missing key",
            |m| {
                m.as_object_mut().expect("an object").remove("seq");
            },
            "missing field `seq`",
        ),
        (
            "an upper-case digest",
            |m| m["bundle_sha256"] = "AB".repeat(32).into(),
            "lowercase hex",
        ),
        (
            "a short commit",
            |m| m["commit"] = "0123456789ab".into(),
            "is not a commit id",
        ),
        (
            "a platform nobody builds for",
            |m| m["platform"] = "linux-riscv".into(),
            "is not one a release is built for",
        ),
        (
            "a number as text",
            |m| m["seq"] = "500".into(),
            "expected u64",
        ),
        (
            "a negative size",
            |m| m["tarball_size"] = json!(-1),
            "expected u64",
        ),
        (
            "a trust digest of the wrong length",
            |m| m["gh_sha256"] = "ab".into(),
            "lowercase hex",
        ),
    ];
    for (what, change, want) in breaks {
        let why = Manifest::parse(&changed(&seeded, change)).expect_err(what);
        assert!(why.contains(want), "{what}: {why}");
        assert!(why.starts_with("manifest.json"), "{what}: {why}");
    }

    let not_json = Manifest::parse(b"not json").expect_err("not json");
    assert!(not_json.starts_with("manifest.json"), "{not_json}");
    let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
    let why = Manifest::parse(&big).expect_err("over the cap");
    assert!(why.contains("at most 16384"), "{why}");
}

/// A manifest serializes to exactly the closed set of keys it reads, each under the name the
/// reader requires, and the bytes read back as the same manifest: the package builder writes
/// a manifest with `Serialize` and the installer reads it with `Deserialize`, so the two
/// derives must agree on the key set.
///
/// Catches a key renamed or skipped on one side only (the reader requires every key, so a
/// manifest the builder wrote without it would be refused on every host), a field added to
/// the struct without the fixture and the builder following, and a value that does not read
/// back as it was written.
///
/// Mutants to plant: `#[serde(skip_serializing_if = ...)]` on a field; `#[serde(rename = ...)]`
/// on one field; a field added to `Manifest`; `#[serde(skip)]` on a field.
#[test]
fn manifest_serializes_to_exactly_its_closed_key_set_and_parses_back() {
    let seeded = Seeded::scratch("seed-manifest-serialize");
    let manifest = Manifest::parse(&bytes(&seeded)).expect("parses");
    let value = serde_json::to_value(&manifest).expect("json");
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let want = [
        "bundle_sha256",
        "commit",
        "gh_sha256",
        "installer_conf_sha256",
        "min_protocol",
        "platform",
        "profile",
        "protocol",
        "seq",
        "state_schema",
        "tarball_sha256",
        "tarball_size",
        "trusted_root_sha256",
    ];
    assert_eq!(keys, want);
    assert_eq!(value, seeded.manifest, "the values are the ones written");

    let written = serde_json::to_vec_pretty(&manifest).expect("json");
    assert_eq!(Manifest::parse(&written), Ok(manifest));
}

/// The floor is one decimal number in `seq_floor` beside `state.json`, or no file: a host with
/// no file has no floor (and `admit` then refuses), one with a file that is not a number, or
/// that cannot be read, is an error and not "no floor" (which would admit nothing, but would
/// say so wrongly), and the number may have a line end.
///
/// Catches a floor read as 0 where the file is garbled (every seed is then above it), and an
/// unreadable file taken for an absent one.
///
/// Mutants to plant: a parse error mapped to `Ok(None)`; the read error mapped to `Ok(None)`;
/// the text not trimmed (the line end after the number is refused).
#[test]
fn the_floor_is_one_number_in_a_file_or_none() {
    let seeded = Seeded::scratch("seed-floor");
    let host = &seeded.layout;
    assert_eq!(read_floor(host), Ok(None), "no file, no floor");
    let path = host.join(&host.seq_floor());
    for (text, want) in [("500\n", 500), ("7", 7), (" 12 \n", 12), ("0", 0)] {
        fs::write(&path, text).expect("floor");
        assert_eq!(read_floor(host), Ok(Some(want)), "{text:?}");
    }
    for bad in ["", "five", "-1", "5 6", "1.5", "99999999999999999999999"] {
        fs::write(&path, bad).expect("floor");
        let got = read_floor(host);
        assert!(
            matches!(&got, Err(FetchError::Unavailable(why)) if why.starts_with("seq_floor: ") && why.contains("is not one number")),
            "{bad:?}: {got:?}"
        );
    }
    fs::remove_file(&path).expect("remove");
    fs::create_dir(&path).expect("a directory where the file should be");
    let got = read_floor(host);
    assert!(
        matches!(&got, Err(FetchError::Unavailable(why)) if why.contains("cannot be read")),
        "{got:?}"
    );
}

/// The word and the reason of the refusal `got` is.
fn refusal_of(got: Result<(), FetchError>) -> (&'static str, String) {
    match got.expect_err("refused") {
        FetchError::Order { word, why } => (word, why),
        other => panic!("{other:?}"),
    }
}

/// The two rules an order meets before the pipeline: it names the seed's commit, all 40 digits,
/// and the seed's `seq` is strictly above the floor, a host with no floor admitting nothing.
/// Each refusal is `commit_or_ref` and says which rule (`seed_mismatch`, `seq_floor`); the
/// commit is judged first.
///
/// Catches the commit compared by the 12 digits that name the seed directory (an order for a
/// commit that shares them is admitted), a floor equal to the seed's `seq` admitted (the seed
/// that is running would be stageable again), a seed below the floor admitted, and a missing
/// floor taken for zero.
///
/// Mutants to plant: the commit compared by its first 12 digits; `<=` made `<` (equal admitted);
/// `None` made `Some(0)`; the floor judged before the commit.
#[test]
fn an_order_is_admitted_for_the_seeds_commit_with_a_seq_above_the_floor() {
    let seeded = Seeded::scratch("seed-admit");
    let seed = Manifest::parse(&bytes(&seeded))
        .expect("parses")
        .seed_facts();
    assert_eq!(admit(&seed, &c(B), Some(SEQ - 1)), Ok(()));
    assert_eq!(admit(&seed, &c(B), Some(0)), Ok(()));

    let refused = |commit: &CommitId, floor: Option<u64>| refusal_of(admit(&seed, commit, floor));
    for floor in [Some(SEQ), Some(SEQ + 1), Some(u64::MAX)] {
        let (word, why) = refused(&c(B), floor);
        assert_eq!(word, "commit_or_ref");
        let want = format!("seq_floor: seed seq {SEQ} is not above {}", floor.unwrap());
        assert_eq!(why, want);
    }
    let (word, why) = refused(&c(B), None);
    assert_eq!(word, "commit_or_ref");
    assert!(why.starts_with("seq_floor: no floor"), "{why}");

    for other in [same_build_other_commit(), c(A)] {
        let (word, why) = refused(&other, Some(0));
        assert_eq!(word, "commit_or_ref");
        assert_eq!(
            why,
            format!("seed_mismatch: this host carries {B}, not {other}")
        );
    }
    let (_, why) = refused(&c(A), Some(SEQ + 100));
    assert!(
        why.starts_with("seed_mismatch"),
        "the commit is judged first: {why}"
    );
}

/// The package's files are in the package's directory (beside the trust directory) and the
/// floor beside the state, per platform: `/usr/share/kbf-installer` on Linux and the
/// installer's own tree on a Mac.
///
/// Catches the seed under the state directory (where the service user's tree is), the Mac's
/// paths taken from the Linux ones, and a floor that is not beside `state.json`.
#[test]
fn the_seed_is_beside_the_trust_directory_and_the_floor_beside_the_state() {
    let linux = Layout::linux(Root::host());
    let mac = Layout::new(Root::host(), Platform::MacOs);
    assert_eq!(
        linux.seed_dir("0123456789ab").as_str(),
        "/usr/share/kbf-installer/seed/0123456789ab"
    );
    assert_eq!(
        linux.seed_file("0123456789ab", MANIFEST).as_str(),
        "/usr/share/kbf-installer/seed/0123456789ab/manifest.json"
    );
    assert_eq!(
        linux.seq_floor().as_str(),
        "/var/lib/kbf-installer/seq_floor"
    );
    assert_eq!(
        mac.seed_dir("0123456789ab").as_str(),
        "/usr/local/kbf-installer/seed/0123456789ab"
    );
    assert_eq!(
        mac.seed_file("0123456789ab", MANIFEST).as_str(),
        "/usr/local/kbf-installer/seed/0123456789ab/manifest.json"
    );
    assert_eq!(mac.seq_floor().as_str(), "/var/db/kbf-installer/seq_floor");
}

/// The rule on `seq` is one function, which the order and the run loop both ask: a seed is
/// above a floor only when its `seq` is strictly higher, and a host with no floor has none to
/// be above. The words are the order's, since the operator reads them in both places.
///
/// Catches the run loop announcing a seed the order would refuse (two copies of the rule that
/// drifted), an equal `seq` taken as above, and no floor taken as a floor of 0.
///
/// Mutants to plant: `>` made `>=`; `None` made `Some(0)`; the stub's `Ok(())` for every case.
#[test]
fn a_seq_is_above_a_floor_only_when_higher_and_there_is_one() {
    assert_eq!(above_floor(SEQ, Some(SEQ - 1)), Ok(()));
    assert_eq!(above_floor(1, Some(0)), Ok(()));
    for floor in [SEQ, SEQ + 1, u64::MAX] {
        let want = format!("seq_floor: seed seq {SEQ} is not above {floor}");
        assert_eq!(above_floor(SEQ, Some(floor)), Err(want));
    }
    let none = above_floor(SEQ, None).expect_err("no floor");
    assert_eq!(
        none,
        "seq_floor: no floor is recorded yet, so no seed is above it"
    );
    assert_eq!(above_floor(0, None), Err(none), "whatever the seq");
}

/// What `status.json` tells of a seed is its commit and its `seq`, as the keys the readers
/// know.
///
/// Catches a key renamed (every reader of `status.json` would stop seeing the seed) and the
/// facts leaking into it.
///
/// Mutant to plant: the facts added to `Available`.
#[test]
fn an_available_seed_is_told_by_its_commit_and_seq() {
    let seed = SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: Seeded::scratch("seed-available").facts(),
    };
    let told = serde_json::to_value(Available::of(&seed)).expect("json");
    assert_eq!(told, json!({"commit": B, "seq": SEQ}));
}
