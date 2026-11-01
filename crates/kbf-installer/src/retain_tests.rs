use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use super::*;
use crate::layout::Platform;
use crate::state::tests::{A, B, C, c};
use crate::testkit::host_root_on;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn scratch_file(name: &str) -> PathBuf {
    kbf_host::testkit::scratch(&format!("retain-{name}")).join(FILE)
}

fn set(commits: &[&str]) -> BTreeSet<CommitId> {
    commits.iter().map(|x| c(x)).collect()
}

fn names(commits: Vec<CommitId>) -> Vec<String> {
    commits.iter().map(ToString::to_string).collect()
}

/// Catches a floor that is not applied: any count below two is raised to two, and a larger
/// one is left as it is.
///
/// Catches: `at_least` returning `keep` (0 and 1 stay below the floor); `min` for `max`.
#[test]
fn a_count_is_raised_to_the_floor_and_no_further() {
    assert_eq!(MIN_KEEP, 2);
    assert_eq!(DEFAULT_KEEP, 4);
    assert_eq!(
        [0, 1, 2, 3, 40].map(at_least),
        [MIN_KEEP, MIN_KEEP, MIN_KEEP, 3, 40]
    );
}

/// Catches a flag that takes a count below the floor, or a word: two and more parse, one,
/// zero, a negative, nothing and words are refused with a reason that names the floor or the
/// word.
///
/// Catches: the bound `n < MIN_KEEP` loosened to `n < 1` (a 1 is taken); the parse error
/// passed over.
#[test]
fn a_flag_count_below_the_floor_or_not_a_number_is_refused() {
    assert_eq!(parse_keep("2"), Ok(2));
    assert_eq!(parse_keep("32"), Ok(32));
    for low in ["1", "0"] {
        let err = parse_keep(low).expect_err(low);
        assert_eq!(
            err,
            format!("a host keeps at least 2 releases besides the ones in use, not {low}")
        );
    }
    for word in ["-1", "", "many", "2.5"] {
        let err = parse_keep(word).expect_err(word);
        assert_eq!(err, format!("{word:?} is not a count of releases"));
    }
}

/// Catches a file that is not there being an error, and the reverse: a host that never wrote
/// `retain.json` has the default (nothing pinned, no last good, no versions).
///
/// Catches: `NotFound` treated as an error.
#[test]
fn a_missing_file_is_the_default() {
    let path = scratch_file("missing");
    let read = Retained::read(&path).expect("missing is default");
    assert_eq!(read, Retained::default());
    assert!(read.protected().is_empty());
    assert!(!path.exists(), "reading writes nothing");
}

/// Catches a reader that refuses a file from an older or newer writer: keys it does not know
/// are ignored and the ones it knows are read, and every key is optional.
///
/// Catches: `deny_unknown_fields` on `Retained`; a required `pinned`.
#[test]
fn an_unknown_key_is_ignored_and_every_key_is_optional() {
    let path = scratch_file("unknown");
    fs::write(
        &path,
        format!(
            r#"{{"pinned": ["{A}"], "added_by_a_newer_release": {{"x": [1]}},
                "versions": {{"{B}": "v7"}}}}"#
        ),
    )
    .expect("seed");
    let read = Retained::read(&path).expect("read");
    assert_eq!(read.pinned, set(&[A]));
    assert_eq!(read.last_good, None);
    assert_eq!(read.versions.get(&c(B)).map(String::as_str), Some("v7"));
    fs::write(&path, "{}").expect("seed");
    assert_eq!(Retained::read(&path).expect("empty"), Retained::default());
}

/// Catches a file that cannot be understood being read as empty: that would let `gc` remove a
/// pinned release. A file that is not JSON, one of the wrong shape, one with a commit that is
/// not one, and one that is a directory are each an error that names the path.
///
/// Catches: a parse error mapped to the default; the read error of a directory passed over.
#[test]
fn a_file_that_cannot_be_read_or_parsed_is_an_error_naming_the_path() {
    let path = scratch_file("garbled");
    for bad in [
        "{",
        "null",
        r#"{"pinned": ["not-a-commit"]}"#,
        r#"{"pinned": 3}"#,
    ] {
        fs::write(&path, bad).expect("seed");
        let err = Retained::read(&path).expect_err(bad);
        assert!(matches!(err, RetainError::Parse { .. }), "{bad}: {err:?}");
        let text = err.to_string();
        assert!(
            text.starts_with(&format!(
                "{} cannot be parsed, so nothing is removed",
                path.display()
            )),
            "{text}"
        );
    }

    // A directory opens and cannot be read.
    let dir = scratch_file("directory");
    fs::create_dir(&dir).expect("dir");
    let err = Retained::read(&dir).expect_err("a directory");
    assert!(matches!(err, RetainError::Read { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read {}: ", dir.display())),
        "{err}"
    );

    // A parent that is a file cannot be opened, and is not "missing".
    let file = scratch_file("parent-is-a-file");
    fs::create_dir(&file).expect("dir");
    let under = file.join("sub").join(FILE);
    fs::write(file.join("sub"), "x").expect("a file");
    let err = Retained::read(&under).expect_err("not a directory");
    assert!(matches!(err, RetainError::Read { .. }), "{err:?}");
}

/// Catches a write that is not whole: the file reads back as written, replaces what was
/// there entirely (no old key survives), is `0644`, leaves no file beside it, and an empty
/// value writes no keys at all.
///
/// Catches: `pinned` or `versions` always serialized; the new file not renamed over the old
/// one; the aside file left behind.
#[test]
fn a_write_round_trips_replaces_the_file_whole_and_leaves_nothing_beside_it() {
    let path = scratch_file("write");
    let first = Retained {
        pinned: set(&[A, B]),
        last_good: Some(c(C)),
        versions: [(c(A), "v1".to_owned()), (c(B), "v2".to_owned())].into(),
    };
    first.write(&path).expect("write");
    assert_eq!(Retained::read(&path).expect("read"), first);
    let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);

    // A stale aside file from a killed writer does not stop the next write.
    let aside = path.with_extension("json.new");
    fs::write(&aside, "stale").expect("stale");
    Retained::default()
        .write(&path)
        .expect("write the empty value");
    assert_eq!(fs::read_to_string(&path).expect("text"), "{}\n");
    assert!(!aside.exists(), "the aside file is renamed over the file");
    let listing: Vec<_> = fs::read_dir(path.parent().expect("dir"))
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(listing, [OsString::from(FILE)]);
}

/// Catches a write that goes into the file in place: when the new copy cannot be made beside
/// it (here the place is a directory) the write fails with the path named and the file that
/// was there is untouched.
///
/// Catches: the write truncating `retain.json` itself (it would succeed and replace the old
/// pin).
#[test]
fn a_write_that_cannot_be_made_aside_leaves_the_old_file_and_says_so() {
    let path = scratch_file("write-fails");
    let old = Retained {
        pinned: set(&[A]),
        ..Retained::default()
    };
    old.write(&path).expect("write");
    let aside = path.with_extension("json.new");
    fs::create_dir(&aside).expect("a directory where the copy goes");
    let err = Retained::default().write(&path).expect_err("cannot write");
    assert!(matches!(err, RetainError::Write { .. }), "{err:?}");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot write {}: ", path.display())),
        "{err}"
    );
    assert_eq!(Retained::read(&path).expect("untouched"), old);

    // And in a directory that is not there.
    let err = old
        .write(&path.parent().expect("dir").join("nope").join(FILE))
        .expect_err("no directory");
    assert!(matches!(err, RetainError::Write { .. }), "{err:?}");
}

/// Catches an update that writes when nothing changed (it would make the file on every
/// confirm), one that does not write a change, one that returns the old value, and one that
/// goes on after a file it cannot parse.
///
/// Catches: `now != was` always true (the first case makes a file); the write skipped; the
/// parse error passed over.
#[test]
fn an_update_writes_only_a_change_and_never_over_a_file_it_cannot_parse() {
    let path = scratch_file("update");
    let same = Retained::update(&path, |_| {}).expect("no change");
    assert_eq!(same, Retained::default());
    assert!(!path.exists(), "nothing changed, nothing written");

    let now = Retained::update(&path, |r| {
        r.pinned.insert(c(A));
    })
    .expect("a change");
    assert_eq!(now.pinned, set(&[A]));
    assert_eq!(Retained::read(&path).expect("read"), now);

    let before = fs::read_to_string(&path).expect("text");
    let mtime = fs::metadata(&path)
        .expect("stat")
        .modified()
        .expect("mtime");
    Retained::update(&path, |r| {
        r.pinned.insert(c(A));
    })
    .expect("the same change");
    assert_eq!(fs::read_to_string(&path).expect("text"), before);
    assert_eq!(
        fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime"),
        mtime,
        "an update that changes nothing does not rewrite"
    );

    // A change that cannot be written is an error, and the file is as it was.
    fs::create_dir(path.with_extension("json.new")).expect("a directory where the copy goes");
    let err = Retained::update(&path, |r| {
        r.pinned.insert(c(B));
    })
    .expect_err("cannot write");
    assert!(matches!(err, RetainError::Write { .. }), "{err:?}");
    assert_eq!(fs::read_to_string(&path).expect("text"), before);
    fs::remove_dir(path.with_extension("json.new")).expect("rm");

    fs::write(&path, "{").expect("garble");
    let mut ran = false;
    let err = Retained::update(&path, |_| ran = true).expect_err("garbled");
    assert!(matches!(err, RetainError::Parse { .. }), "{err:?}");
    assert!(
        !ran,
        "the change is not made over a file that is not understood"
    );
    assert_eq!(fs::read_to_string(&path).expect("text"), "{");
}

/// Catches a protected set that leaves out the pins or the last good release.
///
/// Catches: `protected` without the pins; without `last_good`.
#[test]
fn the_protected_set_is_the_pins_and_the_last_good() {
    let r = Retained {
        pinned: set(&[A, B]),
        last_good: Some(c(C)),
        versions: [(c(A), "v1".to_owned())].into(),
    };
    assert_eq!(r.protected(), set(&[A, B, C]));
    let only_pins = Retained {
        pinned: set(&[A]),
        ..Retained::default()
    };
    assert_eq!(only_pins.protected(), set(&[A]));
    let only_good = Retained {
        last_good: Some(c(B)),
        ..Retained::default()
    };
    assert_eq!(only_good.protected(), set(&[B]));
}

/// Catches the file living somewhere a release of the other platform would not look for it:
/// it is `retain.json` in the state directory, beside `state.json`, on each platform.
///
/// Catches: `path` joining the releases directory or the root.
#[test]
fn the_file_is_in_the_state_directory_on_both_platforms() {
    for platform in [Platform::Linux, Platform::MacOs] {
        let (layout, root) = host_root_on("retain-path", platform);
        let path = path(&layout);
        let state_dir = layout.join(&layout.state_dir());
        assert_eq!(path, state_dir.join("retain.json"), "{platform:?}");
        assert!(path.starts_with(&root));
    }
}

/// Catches a selection that keeps the wrong ones: of the releases not kept the `keep` newest
/// stay and the older ones are returned, oldest last; ties in time are ordered by commit,
/// the higher commit counting as newer; a kept release is never returned and never counts
/// among the newest; and a `keep` larger than what is there returns nothing.
///
/// Catches: the sort reversed (the newest returned); the kept ones not filtered; the tie
/// broken the other way; `skip(keep)` off by one.
#[test]
fn surplus_is_everything_but_the_newest_keep_others_newest_first() {
    let (x, y, z) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let found = vec![
        (c(&x), at(10)),
        (c(&y), at(20)),
        (c(&z), at(30)),
        (c(A), at(5)),
        (c(B), at(40)),
    ];
    let none = BTreeSet::new();
    assert_eq!(names(surplus(&found, &none, 2)), [&y, &x, A]);
    assert_eq!(names(surplus(&found, &none, 0)).len(), 5);
    assert_eq!(names(surplus(&found, &none, 4)), [A]);
    assert!(surplus(&found, &none, 5).is_empty());
    assert!(surplus(&found, &none, 50).is_empty());
    assert!(surplus(&[], &none, 0).is_empty());

    // Kept releases are neither returned nor counted among the newest: B and z are kept, so
    // the two newest others are y and x, and only A goes.
    assert_eq!(names(surplus(&found, &set(&[B, &z]), 2)), [A]);
    assert_eq!(names(surplus(&found, &set(&[B, &z]), 1)), [&x, A]);
    assert!(surplus(&found, &set(&[A, B, &x, &y, &z]), 0).is_empty());

    // The same time: the higher commit is the newer.
    let tied = vec![(c(A), at(7)), (c(B), at(7)), (c(C), at(7))];
    assert_eq!(names(surplus(&tied, &none, 1)), [A, B]);
    assert_eq!(names(surplus(&tied, &none, 2)), [B]);
}

fn known<'a>(
    current: Option<&'a CommitId>,
    previous: Option<&'a CommitId>,
    on_trial: Option<&'a CommitId>,
    retained: Option<&'a Retained>,
) -> Known<'a> {
    Known {
        current,
        previous,
        on_trial,
        retained,
    }
}

/// Catches a listing that is out of order or says the wrong thing about a release: newest
/// first, a tie by commit with the higher first; each release carries the version recorded
/// for it (none for one with no record) and each reason it must stay, in the order
/// current, previous, on_trial, pinned, last_good; a release one reason covers has only it.
///
/// Catches: the sort ascending; the tie broken the other way; a reason word missing or
/// misattributed (current/previous swapped, pinned read from last_good); the version of
/// another release.
#[test]
fn the_listing_is_newest_first_with_each_version_and_every_reason_to_stay() {
    let (x, y) = ("1".repeat(40), "2".repeat(40));
    let found = vec![
        (c(&x), at(10)),
        (c(A), at(50)),
        (c(B), at(50)),
        (c(C), at(30)),
        (c(&y), at(20)),
    ];
    let retained = Retained {
        pinned: set(&[A, &x]),
        last_good: Some(c(&x)),
        versions: [(c(A), "v-a".to_owned()), (c(C), "v-c".to_owned())].into(),
    };
    let (cur, prev, trial) = (c(B), c(C), c(A));
    let listed = listing(
        &found,
        &known(Some(&cur), Some(&prev), Some(&trial), Some(&retained)),
    );
    let shape: Vec<_> = listed
        .iter()
        .map(|k| (k.commit.to_string(), k.version.clone(), k.why.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                A.to_owned(),
                Some("v-a".to_owned()),
                vec!["on_trial", "pinned"]
            ),
            (B.to_owned(), None, vec!["current"]),
            (C.to_owned(), Some("v-c".to_owned()), vec!["previous"]),
            (y.clone(), None, vec![]),
            (x.clone(), None, vec!["pinned", "last_good"]),
        ]
    );

    // A release that is every reason at once names them in the fixed order.
    let all = Retained {
        pinned: set(&[A]),
        last_good: Some(c(A)),
        ..Retained::default()
    };
    let a = c(A);
    let listed = listing(&found, &known(Some(&a), Some(&a), Some(&a), Some(&all)));
    assert_eq!(
        listed[0].why,
        ["current", "previous", "on_trial", "pinned", "last_good"]
    );
    assert_eq!(listed[1].why, Vec::<&str>::new());

    // Nothing known: every release listed with no reason and no version.
    let bare = listing(&found, &Known::default());
    assert_eq!(bare.len(), 5);
    assert!(bare.iter().all(|k| k.why.is_empty() && k.version.is_none()));
    assert_eq!(bare[0].commit, c(A));
    assert!(listing(&[], &Known::default()).is_empty());
}

/// Catches the lines `host status` prints: a count line first, then one line per release
/// with the commit, the version or the word `unknown`, and the reasons in brackets only
/// when there are some.
///
/// Catches: the count of something else; `unknown` missing; the brackets printed empty.
#[test]
fn the_lines_are_a_count_then_a_line_per_release() {
    assert_eq!(lines(&[]), "releases kept: 0\n");
    let kept = [
        Kept {
            commit: c(A),
            version: Some("v1".to_owned()),
            why: vec!["current", "pinned"],
        },
        Kept {
            commit: c(B),
            version: None,
            why: vec!["previous"],
        },
        Kept {
            commit: c(C),
            version: None,
            why: vec![],
        },
    ];
    assert_eq!(
        lines(&kept),
        format!(
            "releases kept: 3\n  {A} v1 (current, pinned)\n  {B} unknown (previous)\n  {C} unknown\n"
        )
    );
}

/// Catches a JSON listing a script cannot rely on: the reasons are left out when there are
/// none, the version is null when unknown.
///
/// Catches: `why` serialized when empty.
#[test]
fn a_kept_release_serializes_without_an_empty_reason_list() {
    let none = Kept {
        commit: c(A),
        version: None,
        why: vec![],
    };
    assert_eq!(
        serde_json::to_value(&none).expect("json"),
        serde_json::json!({"commit": A, "version": null})
    );
    let some = Kept {
        commit: c(B),
        version: Some("v".to_owned()),
        why: vec!["current"],
    };
    assert_eq!(
        serde_json::to_value(&some).expect("json"),
        serde_json::json!({"commit": B, "version": "v", "why": ["current"]})
    );
}
