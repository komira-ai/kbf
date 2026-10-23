//! `cargo xtask release notes` and `release previous`: the notes show the Beta record the
//! tag carries and never judge it; what is not reversible comes from the two builds.

use std::path::PathBuf;

use kbf_gate::{CommitId, GateResult, Limit, ResultDoc, Sample, Status, Waiver};

use crate::fixture::*;

/// A release tag carrying a record `v0.10.0` and an older `v0.9.0` on the first commit.
fn with_notes(name: &str, change: impl FnOnce(&mut ResultDoc)) -> Fx {
    let fx = fx(name);
    fx.git(&["tag", "v0.9.0", &fx.first]);
    let mut d = good();
    change(&mut d);
    fx.annotate(TAG, RELEASE, &msg(&d));
    fx
}

/// `kbf --version --json` of the build of `commit`, with the fields given.
fn identity(commit: &str, meta_log: Option<u64>, protocol: Option<u64>) -> String {
    let mut v = serde_json::json!({"commit": &commit[..12], "profile": "release"});
    if let Some(m) = meta_log {
        v["meta_log"] = m.into();
    }
    if let Some(p) = protocol {
        v["protocol"] = p.into();
    }
    v.to_string()
}

/// `release notes` with this release's identity `cur` and the previous release's `prev`
/// (as the workflow writes them, one file each); the result of the run and the notes file.
fn notes_run(fx: &Fx, cur: &str, prev: Option<&str>) -> (Ran, PathBuf) {
    let out = fx.root.join("notes.md");
    let cur_file = fx.root.join("version.json");
    std::fs::write(&cur_file, cur).unwrap();
    let mut extra = vec![
        "--version-json",
        cur_file.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ];
    let prev_file = fx.root.join("previous-version.json");
    if let Some(p) = prev {
        std::fs::write(&prev_file, p).unwrap();
        extra.extend(["--previous-version-json", prev_file.to_str().unwrap()]);
    }
    (run(fx, "notes", TAG, RELEASE, &extra), out)
}

/// The notes of a release whose builds both say `meta_log` 3 and `protocol` 1 (a first
/// release has no previous build to say it).
fn notes_of(fx: &Fx) -> String {
    let cur = identity(RELEASE, Some(3), Some(1));
    let has_previous = !fx.git(&["tag", "--list", "v0.9.0"]).is_empty();
    let prev = has_previous.then(|| identity(&fx.first, Some(3), Some(1)));
    let ((code, o, err), out) = notes_run(fx, &cur, prev.as_deref());
    assert_eq!(code, Some(0), "{o}{err}");
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn the_notes_list_the_commits_since_the_previous_release_by_version_order() {
    // Catches: a previous release taken by name order (v0.10.0 sorts before v0.9.0, so
    // the notes would call this the first release) or a range that starts at the root.
    let notes = notes_of(&with_notes("notes-commits", |_| {}));
    assert!(notes.starts_with("# v0.10.0\n"), "{notes}");
    assert!(notes.contains("## Commits since v0.9.0\n"), "{notes}");
    assert!(notes.contains(" release commit\n"), "{notes}");
    assert!(!notes.contains(" first\n"), "{notes}");
}

#[test]
fn the_first_release_lists_all_its_history() {
    let fx = fx("notes-first");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let notes = notes_of(&fx);
    assert!(
        notes.contains("## Commits (the first release)\n"),
        "{notes}"
    );
    assert!(
        notes.contains(" first\n") && notes.contains(" release commit\n"),
        "{notes}"
    );
}

/// The part of `notes` between the heading `from` and the next `## ` heading.
fn part<'a>(notes: &'a str, from: &str) -> &'a str {
    let start = notes
        .find(from)
        .unwrap_or_else(|| panic!("no {from} in {notes}"));
    let rest = &notes[start + from.len()..];
    &rest[..rest.find("\n## ").unwrap_or(rest.len())]
}

#[test]
fn a_record_supplied_not_reversible_list_is_not_used_and_the_all_clear_opens_the_notes() {
    // Catches (ruling Q3: verify is the only source of the irreversible list): a notes
    // writer that unions in, or falls back to, the verdict's `not_reversible` lines. Proves
    // a verdict naming a line leaves the section at the all-clear when the builds agree, and
    // that the line appears nowhere in the notes. Catches: the union re-added in `render`.
    let notes = notes_of(&with_notes("notes-irreversible", |d| {
        d.not_reversible = vec!["drops the legacy index".to_owned()];
    }));
    assert!(!notes.contains("drops the legacy index"), "{notes}");
    assert!(
        part(&notes, "## Before you click")
            .contains("### Not reversible\n\nNothing in this release is flagged as not reversible"),
        "{notes}"
    );
    let notes = notes_of(&with_notes("notes-reversible", |_| {}));
    assert!(
        part(&notes, "## Before you click")
            .contains("### Not reversible\n\nNothing in this release is flagged as not reversible"),
        "{notes}"
    );
}

#[test]
fn before_you_click_comes_first_with_waivers_and_the_record_in_a_line() {
    // Catches: the section anywhere but first (the reviewer reads the top), waivers left
    // to the detail section, a verdict summary that is not there.
    let notes = notes_of(&with_notes("notes-order", |d| {
        d.waivers = vec![Waiver::new("min-load", "op", "holiday week").unwrap()];
    }));
    let at = |h: &str| notes.find(h).unwrap_or_else(|| panic!("no {h} in {notes}"));
    assert!(
        notes.starts_with("# v0.10.0\n\n## Before you click\n"),
        "{notes}"
    );
    assert!(
        at("## Before you click") < at("## Commits since"),
        "{notes}"
    );
    assert!(at("## Commits since") < at("## Beta record"), "{notes}");
    let top = part(&notes, "## Before you click");
    assert!(
        top.contains("### Waivers\n\n- min-load waived by op: holiday week\n"),
        "{notes}"
    );
    assert!(
        top.contains(
            "### Record\n\n- status Green; 1 gate(s), 0 warning(s); detail under Beta record (audit only, nothing gates on it)"
        ),
        "{notes}"
    );
    let none = notes_of(&with_notes("notes-no-waiver", |_| {}));
    assert!(
        none.contains("### Waivers\n\nNo gate or condition is waived.\n"),
        "{none}"
    );
}

/// The notes of a release whose previous release (`v0.9.0`) was built with `prev`
/// (`meta_log`, `protocol`) and which is built with `cur`.
fn notes_between(
    name: &str,
    cur: (Option<u64>, Option<u64>),
    prev: (Option<u64>, Option<u64>),
) -> String {
    let fx = with_notes(name, |_| {});
    let ((code, o, err), out) = notes_run(
        &fx,
        &identity(RELEASE, cur.0, cur.1),
        Some(&identity(&fx.first, prev.0, prev.1)),
    );
    assert_eq!(code, Some(0), "{o}{err}");
    std::fs::read_to_string(out).unwrap()
}

const RAISED: &str = "meta_log raised from 3 (v0.9.0) to 4: v0.9.0 cannot open the metadata log this release writes, so there is no going back to it";

#[test]
fn a_raised_meta_log_is_not_reversible_though_the_record_says_nothing() {
    // Catches: notes that read the verdict's lines only (nobody types the log version
    // into a tag), or a comparison the wrong way round or off by one (equal is reversible,
    // and so is a lower number).
    let notes = notes_between("meta-raised", (Some(4), Some(1)), (Some(3), Some(1)));
    assert!(
        part(&notes, "## Before you click").contains(&format!("- {RAISED}\n")),
        "{notes}"
    );
    for (name, cur) in [("meta-equal", 3), ("meta-lower", 2)] {
        let notes = notes_between(name, (Some(cur), Some(1)), (Some(3), Some(1)));
        assert!(!notes.contains("meta_log"), "{name}: {notes}");
        assert!(
            notes.contains("Nothing in this release is flagged"),
            "{name}: {notes}"
        );
    }
}

#[test]
fn an_unknown_meta_log_counts_as_not_reversible() {
    // Catches: fail-open, an unstated number read as reversible, on either side.
    let unknown =
        "- meta_log is not stated by this release or by v0.9.0: unknown counts as not reversible\n";
    for (name, cur, prev) in [
        ("meta-unknown-cur", None, Some(3)),
        ("meta-unknown-prev", Some(3), None),
        ("meta-unknown-both", None, None),
    ] {
        let notes = notes_between(name, (cur, Some(1)), (prev, Some(1)));
        assert!(
            part(&notes, "## Before you click").contains(unknown),
            "{name}: {notes}"
        );
    }
}

#[test]
fn a_changed_or_unstated_protocol_is_listed_and_an_equal_one_is_not() {
    // Catches: the protocol ignored, or listed when it did not change.
    let notes = notes_between("proto-changed", (Some(3), Some(2)), (Some(3), Some(1)));
    assert!(
        part(&notes, "## Before you click").contains("- wire protocol 1 (v0.9.0) to 2: "),
        "{notes}"
    );
    let notes = notes_between("proto-unknown", (Some(3), None), (Some(3), Some(1)));
    assert!(
        notes.contains(
            "- wire protocol is not stated by this release or by v0.9.0: unknown counts as changed"
        ),
        "{notes}"
    );
    let notes = notes_between("proto-equal", (Some(3), Some(1)), (Some(3), Some(1)));
    assert!(!notes.contains("- wire protocol"), "{notes}");
}

#[test]
fn what_the_builds_say_is_listed_once_and_the_records_own_lines_are_dropped() {
    // Catches: the verdict's list added next to the computed one (even when it repeats a
    // computed line, which must still appear exactly once, from the builds).
    let fx = with_notes("notes-union", |d| {
        d.not_reversible = vec![RAISED.to_owned(), "drops the legacy index".to_owned()];
    });
    let ((code, o, err), out) = notes_run(
        &fx,
        &identity(RELEASE, Some(4), Some(1)),
        Some(&identity(&fx.first, Some(3), Some(1))),
    );
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert_eq!(notes.matches(RAISED).count(), 1, "{notes}");
    assert!(!notes.contains("drops the legacy index"), "{notes}");
}

#[test]
fn the_first_release_has_nothing_to_go_back_to() {
    // Catches: a first release called reversible (`reversible(_, None)` is false).
    let fx = fx("notes-first-irreversible");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let notes = notes_of(&fx);
    assert!(
        part(&notes, "## Before you click")
            .contains("- first release: there is no earlier release to go back to. Hosts running an untagged earlier build: upgrade kbf-installer before staging (older installers may refuse this build's version output).\n"),
        "{notes}"
    );
}

/// The notes of a release between identities written as `cur_extra` and `prev_extra`
/// (extra top-level keys over the commit stamp).
fn notes_with_keys(name: &str, cur_extra: &[&str], prev_extra: &[&str]) -> String {
    let id = |commit: &str, extra: &[&str]| {
        let mut v: serde_json::Value =
            serde_json::from_str(&identity(commit, Some(3), Some(1))).unwrap();
        for k in extra {
            v[*k] = 1.into();
        }
        v.to_string()
    };
    let fx = with_notes(name, |_| {});
    let ((code, o, err), out) = notes_run(
        &fx,
        &id(RELEASE, cur_extra),
        Some(&id(&fx.first, prev_extra)),
    );
    assert_eq!(code, Some(0), "{o}{err}");
    std::fs::read_to_string(out).unwrap()
}

#[test]
fn a_key_the_previous_version_output_lacks_tells_operators_to_upgrade_the_installer() {
    // Catches: the line dropped, the keys unsorted or wrong, a subset check inverted
    // (a removed key then warns, an added one does not).
    let line = format!(
        "- Upgrade kbf-installer on every host before staging this release: its version \
         output adds seq, zeta, which installers built before {} may refuse\n",
        &RELEASE[..12]
    );
    let notes = notes_with_keys("keys-added", &["zeta", "seq"], &["other"]);
    assert!(
        part(&notes, "## Before you click").contains(&line),
        "{notes}"
    );
    for (name, cur, prev) in [
        ("keys-same", &["seq"][..], &["seq"][..]),
        ("keys-removed", &[][..], &["seq"][..]),
    ] {
        let notes = notes_with_keys(name, cur, prev);
        assert!(!notes.contains("Upgrade kbf-installer"), "{name}: {notes}");
    }
}

#[test]
fn notes_refuse_when_what_is_not_reversible_cannot_be_determined() {
    // Catches: fail-open, notes written with the identity of the previous release absent,
    // wrong (another commit's), not JSON, or a field that is not a number.
    let fx = with_notes("notes-closed", |_| {});
    let cur = identity(RELEASE, Some(3), Some(1));
    let bad: Vec<(&str, Option<String>, &str)> = vec![
        (
            "no previous identity",
            None,
            "--previous-version-json is required",
        ),
        (
            "another commit",
            Some(identity(RELEASE, Some(3), Some(1))),
            "names commit",
        ),
        ("not JSON", Some("meta_log=3".to_owned()), "not JSON"),
        (
            "over the size cap",
            Some(format!("{:width$}{{}}", "", width = 8192)),
            "over 8192 bytes",
        ),
        (
            "not a number",
            Some(format!(
                r#"{{"commit":"{}","meta_log":"3"}}"#,
                &fx.first[..12]
            )),
            "meta_log \"3\" is not a whole number",
        ),
        (
            "protocol not a number",
            Some(format!(
                r#"{{"commit":"{}","meta_log":3,"protocol":-1}}"#,
                &fx.first[..12]
            )),
            "protocol -1 is not a whole number",
        ),
    ];
    for (what, prev, why) in bad {
        let ((code, _, err), out) = notes_run(&fx, &cur, prev.as_deref());
        assert_eq!(code, Some(1), "{what}: {err}");
        assert!(err.contains(why), "{what}: {err}");
        assert!(!out.exists(), "{what}: notes were written");
    }
    // A file that is not there.
    let out = fx.root.join("notes.md");
    let gone = fx.root.join("gone.json");
    let (code, _, err) = run(
        &fx,
        "notes",
        TAG,
        RELEASE,
        &[
            "--version-json",
            fx.root.join("version.json").to_str().unwrap(),
            "--previous-version-json",
            gone.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("gone.json"), "{err}");
    // This release's own identity is as strict.
    let ((code, _, err), out) = notes_run(&fx, "{}", Some(&identity(&fx.first, Some(3), Some(1))));
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("names commit"), "{err}");
    assert!(!out.exists());
}

#[test]
fn an_identity_file_that_is_not_text_is_refused() {
    // Catches: an identity file that cannot be read as text taken as an empty or an absent
    // one, so the notes would be written without the previous release's log version.
    let fx = with_notes("notes-not-text", |_| {});
    let cur = identity(RELEASE, Some(3), Some(1));
    let prev = fx.root.join("previous.json");
    std::fs::write(&prev, [0xff, 0xfe, 0x7b, 0x7d]).unwrap();
    let out = fx.root.join("notes.md");
    let cur_file = fx.root.join("version.json");
    std::fs::write(&cur_file, &cur).unwrap();
    let (code, _, err) = run(
        &fx,
        "notes",
        TAG,
        RELEASE,
        &[
            "--version-json",
            cur_file.to_str().unwrap(),
            "--previous-version-json",
            prev.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("previous.json"), "{err}");
    // The read error is returned: swallowed, the file parses as empty text and fails as
    // "not JSON", which would also be a refusal.
    assert!(!err.contains("not JSON"), "{err}");
    assert!(err.contains("valid UTF-8"), "{err}");
    assert!(!out.exists(), "notes were written");
}

#[test]
fn the_current_identity_is_required() {
    // Catches: `--version-json` made optional (a release whose own log version is unread).
    let fx = with_notes("notes-no-cur", |_| {});
    let out = fx.root.join("notes.md");
    let (code, _, err) = run(
        &fx,
        "notes",
        TAG,
        RELEASE,
        &["--out", out.to_str().unwrap()],
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--version-json"), "{err}");
}

#[test]
fn an_empty_previous_identity_path_is_none_given() {
    // The workflow passes the path as an empty string when there is no previous release.
    // Catches: the empty path read as a file (a first release refused), or as an answer
    // when a previous release exists (it must still refuse: fail closed).
    let first = fx("notes-empty-first");
    first.annotate(TAG, RELEASE, &msg(&good()));
    let cur = first.root.join("version.json");
    std::fs::write(&cur, identity(RELEASE, Some(3), Some(1))).unwrap();
    let out = first.root.join("notes.md");
    let args = [
        "--version-json",
        cur.to_str().unwrap(),
        "--previous-version-json",
        "",
        "--out",
        out.to_str().unwrap(),
    ];
    let (code, o, err) = run(&first, "notes", TAG, RELEASE, &args);
    assert_eq!(code, Some(0), "{o}{err}");
    let second = with_notes("notes-empty-second", |_| {});
    let (code, _, err) = run(&second, "notes", TAG, RELEASE, &args);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("--previous-version-json is required"), "{err}");
}

#[test]
fn a_previous_identity_for_a_first_release_is_a_usage_error() {
    // Catches: an identity of some other build quietly ignored.
    let fx = fx("notes-first-prev");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let ((code, _, err), _) = notes_run(
        &fx,
        &identity(RELEASE, Some(3), Some(1)),
        Some(&identity(&fx.first, Some(3), Some(1))),
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("there is no previous release"), "{err}");
}

/// A repository with `v0.8.0` (stable, on main), `v0.9.0-rc1` (a release candidate on
/// main), `v0.9.5` (stable but on the side branch) and the release `v0.10.0`.
fn with_tag_zoo(name: &str) -> Fx {
    let fx = with_notes(name, |_| {});
    fx.git(&["tag", "-d", "v0.9.0"]);
    fx.git(&["tag", "v0.8.0", &fx.first]);
    fx.git(&["tag", "v0.9.0-rc1", &fx.first]);
    fx.git(&["tag", "v0.9.5", &fx.side]);
    fx
}

#[test]
fn the_previous_release_is_the_nearest_stable_ancestor() {
    // Catches: the neighbour in the tag list taken as the previous release (here a
    // release candidate, then a tag on a branch that is not main's history), either of
    // which makes the commit list and the reversibility check about the wrong release.
    let fx = with_tag_zoo("previous-zoo");
    let cur = identity(RELEASE, Some(3), Some(1));
    let prev = identity(&fx.first, Some(3), Some(1));
    let ((code, o, err), out) = notes_run(&fx, &cur, Some(&prev));
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert!(notes.contains("## Commits since v0.8.0\n"), "{notes}");
    let output = fx.root.join("out");
    let (code, o, err) = xtask(&[
        "release",
        "previous",
        "--repo-dir",
        fx.repo().to_str().unwrap(),
        "--tag",
        TAG,
        "--commit",
        RELEASE,
        "--output",
        output.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0), "{err}");
    let expected = format!("previous=v0.8.0\nprevious-commit={}\n", fx.first);
    assert_eq!(o, expected);
    assert_eq!(std::fs::read_to_string(output).unwrap(), expected);
}

#[test]
fn a_first_release_has_no_previous_and_the_verb_says_so() {
    // Catches: empty lines missing (the workflow's `if: previous != ''` then reads a
    // stale output) and a tag of no place in `v*` taking another's.
    let fx = fx("previous-none");
    fx.git(&["tag", "v0.20.0", &fx.first]);
    for tag in [TAG, "rc1"] {
        let (code, o, err) = xtask(&[
            "release",
            "previous",
            "--repo-dir",
            fx.repo().to_str().unwrap(),
            "--tag",
            tag,
            "--commit",
            RELEASE,
        ]);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(o, "previous=\nprevious-commit=\n", "{tag}");
    }
    let (code, _, err) = xtask(&[
        "release",
        "previous",
        "--repo-dir",
        fx.repo().to_str().unwrap(),
        "--tag",
        TAG,
        "--commit",
        "abc",
    ]);
    assert_eq!(code, Some(2), "{err}");
}

#[test]
fn the_beta_notes_carry_the_gates_the_escapes_and_the_withdrawn() {
    let notes = notes_of(&with_notes("notes-summary", |d| {
        d.escapes = vec![
            "handover with a build in flight".to_owned(),
            "two".to_owned(),
        ];
        d.withdrawn = vec!["v0.9.0".to_owned()];
        d.waivers = vec![Waiver::new("min-load", "op", "holiday week").unwrap()];
        d.gates[0] = GateResult::decide("lost-blobs", Sample::Value(0.0), Limit::AtMost(0.0), 1, 2);
    }));
    assert!(
        notes.contains("## Beta record\n\n- gate lost-blobs: 0\n"),
        "{notes}"
    );
    assert!(
        !notes.contains("bake:") && !notes.contains("load:"),
        "{notes}"
    );
    assert!(
        notes.contains("- waived min-load by op: holiday week\n"),
        "{notes}"
    );
    assert!(
        notes.contains("## Beta escapes\n\n2 scenario(s) only gamma found.\n"),
        "{notes}"
    );
    assert!(notes.contains("## Withdrawn\n\n- v0.9.0\n"), "{notes}");
}

#[test]
fn the_notes_of_the_record_verdict_write_writes_come_from_the_builds_not_its_list() {
    // Catches: the notes refusing the pipeline's own (Beta) verdict, or reading the verdict's
    // typed `not_reversible` list (a list in a tag is no source; the two builds are), which
    // would say "meta-log format" here though both builds agree.
    let fx = fx("notes-written");
    fx.annotate("v0.3.7", RELEASE, &std::fs::read(WRITTEN).unwrap());
    let (version, out) = (fx.root.join("version.json"), fx.root.join("notes.md"));
    std::fs::write(&version, identity(RELEASE, Some(3), Some(1))).unwrap();
    let (code, o, err) = run(
        &fx,
        "notes",
        "v0.3.7",
        RELEASE,
        &[
            "--version-json",
            version.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert!(
        notes.contains("### Not reversible\n\n- first release: there is no earlier release"),
        "{notes}"
    );
    assert!(!notes.contains("meta-log format"), "{notes}");
    assert!(
        notes.contains("## Beta record\n\n- gate ci: 1\n- gate beta: 1\n"),
        "{notes}"
    );
}

#[test]
fn notes_are_written_whatever_the_record_says() {
    // Catches (defect: `notes` refuses a record that failed): the notes judging the
    // record. A record that says it is red, a message that is not a record and a tag
    // with no message all get notes; the first states the status, the others say
    // there is no readable record. The commits and what is not reversible are the same.
    let red = notes_of(&with_notes("notes-red", |d| d.status = Status::Red));
    assert!(
        red.contains("### Record\n\n- status Red; 1 gate(s)"),
        "{red}"
    );
    assert!(red.contains("(audit only, nothing gates on it)"), "{red}");
    assert!(
        red.contains("## Beta record\n\n- gate lost-blobs: 0\n"),
        "{red}"
    );
    for (name, message) in [
        ("notes-prose", &b"release 0.10.0\n\nlooks fine\n"[..]),
        ("notes-empty-message", &b""[..]),
    ] {
        let fx = fx(name);
        fx.git(&["tag", "v0.9.0", &fx.first]);
        if message.is_empty() {
            let object =
                format!("object {RELEASE}\ntype commit\ntag {TAG}\ntagger T <t@e.x> 0 +0000\n\n");
            let sha = git(&fx.repo(), &["mktag"], Some(&object));
            fx.git(&["update-ref", &format!("refs/tags/{TAG}"), &sha]);
        } else {
            fx.annotate(TAG, RELEASE, message);
        }
        let notes = notes_of(&fx);
        assert!(
            notes.contains(
                "### Record\n\n- the tag carries no readable Beta record (nothing gates on it)\n"
            ),
            "{name}: {notes}"
        );
        assert!(
            notes.contains("## Commits since v0.9.0\n"),
            "{name}: {notes}"
        );
        assert!(!notes.contains("## Beta record"), "{name}: {notes}");
        assert!(!notes.contains("escapes"), "{name}: {notes}");
        assert!(
            notes.contains("Nothing in this release is flagged as not reversible"),
            "{name}: {notes}"
        );
    }
}

#[test]
fn notes_of_a_tag_with_a_prose_message_say_there_is_no_record() {
    // Catches: a tag with no readable record refused by notes, or its prose parsed.
    let fx = fx("notes-prose");
    fx.annotate(TAG, RELEASE, b"release 0.10.0\n\nlooks fine\n");
    let notes = notes_of(&fx);
    assert!(notes.contains("no readable Beta record"), "{notes}");
}

#[test]
fn a_record_with_no_gates_and_no_waivers_has_no_detail() {
    // Catches: the detail section missing when the record lists nothing.
    let notes = notes_of(&with_notes("notes-bare", |d| {
        d.gates.clear();
        d.status = Status::Running;
    }));
    assert!(
        notes.contains("## Beta record\n\nNo detail recorded.\n"),
        "{notes}"
    );
}

#[test]
fn a_warning_gate_is_reported_in_the_notes() {
    // Catches: a soft gate (latency between 1.5 and 3 times) read as red, or lost from the notes.
    let notes = notes_of(&with_notes("notes-warn", |d| {
        d.gates.push(GateResult::decide(
            "latency-p99",
            Sample::Value(1.8),
            Limit::Band {
                warn: 1.5,
                red: 3.0,
            },
            1,
            2,
        ));
    }));
    assert!(
        notes.contains("- gate latency-p99: 1.8 (warning)\n"),
        "{notes}"
    );
}

#[test]
fn a_gate_with_no_measurement_reads_absent_in_the_notes() {
    // An absent series is red, so the record cannot be green; the notes helper is
    // reached through a waiver-free green document whose gate measured nothing finite.
    let mut d = good();
    d.gates[0] = GateResult::decide(
        "lost-blobs",
        Sample::Value(f64::INFINITY),
        Limit::AtMost(f64::MAX),
        1,
        2,
    );
    d.gates[0].verdict = kbf_gate::Verdict::Green;
    d.gates[0].measured = None;
    let fx = fx("notes-absent");
    fx.annotate(TAG, RELEASE, &msg(&d));
    assert!(notes_of(&fx).contains("- gate lost-blobs: absent\n"));
}

#[test]
fn a_long_history_is_cut_and_counted() {
    // Catches: notes that list every commit of a first release.
    let fx = fx("notes-long");
    for i in 0..205 {
        fx.git(&["commit", "-q", "--allow-empty", "-m", &format!("c{i}")]);
    }
    let head = fx.git(&["rev-parse", "HEAD"]);
    let mut d = good();
    d.commit = CommitId::parse(&head).unwrap();
    fx.annotate(TAG, &head, &msg(&d));
    let cur_file = fx.root.join("version.json");
    std::fs::write(&cur_file, identity(&head, Some(3), Some(1))).unwrap();
    let out = fx.root.join("notes.md");
    let (code, o, err) = run(
        &fx,
        "notes",
        TAG,
        &head,
        &[
            "--version-json",
            cur_file.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    // 205 + first + release commit = 207 commits; 200 listed.
    assert!(notes.contains("- and 7 more\n"), "{notes}");
    assert!(!notes.contains(" c0\n"), "{notes}");
}

#[test]
fn a_tag_outside_the_release_names_is_a_first_release() {
    // `previous` finds no place for a tag that `v*` does not list.
    let fx = fx("notes-rc");
    let mut d = good();
    d.tag = Some("rc1".to_owned());
    fx.annotate("rc1", RELEASE, &msg(&d));
    let cur_file = fx.root.join("version.json");
    std::fs::write(&cur_file, identity(RELEASE, Some(3), Some(1))).unwrap();
    let out = fx.root.join("notes.md");
    let (code, o, err) = run(
        &fx,
        "notes",
        "rc1",
        RELEASE,
        &[
            "--version-json",
            cur_file.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(0), "{o}{err}");
    assert!(
        std::fs::read_to_string(out)
            .unwrap()
            .contains("(the first release)")
    );
}

#[test]
fn notes_that_cannot_be_written_fail() {
    // Catches: a notes file that was not written reported as written.
    let fx = fx("notes-unwritable");
    fx.annotate(TAG, RELEASE, &msg(&good()));
    let out = fx.root.join("missing-dir").join("notes.md");
    let cur = fx.root.join("version.json");
    std::fs::write(&cur, identity(RELEASE, Some(3), Some(1))).unwrap();
    let (code, _, err) = run(
        &fx,
        "notes",
        TAG,
        RELEASE,
        &[
            "--version-json",
            cur.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("write "), "{err}");
}
