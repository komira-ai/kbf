//! `cargo xtask release previous` and the `--previous` of `release notes`: the release before
//! is the nearest earlier stable tag on the commit's history that has a published release;
//! a tag without one is passed over and named, and with none at all it is the first release.

use serde_json::json;

use crate::fixture::*;
use crate::notes::{identity, with_notes};

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

/// The line `xtask` prints on stderr for a tag passed over on the way to `named`.
fn passed_over(tag: &str, named: &str) -> String {
    format!(
        "xtask: {tag} is tagged but has no published release (its release run did not \
         publish): passed over; the release before {TAG} is {named}\n"
    )
}

/// `release notes` of `fx` given `--previous since`, with the identities of the two builds
/// (none for the previous one when `since` is empty); the result and the notes file.
fn notes_since(fx: &Fx, since: &str) -> (Ran, std::path::PathBuf) {
    let out = fx.root.join("notes.md");
    let cur = fx.root.join("version.json");
    std::fs::write(&cur, identity(RELEASE, Some(3), Some(1))).unwrap();
    let prev = fx.root.join("previous-version.json");
    std::fs::write(&prev, identity(&fx.first, Some(3), Some(1))).unwrap();
    let prev = if since.is_empty() {
        ""
    } else {
        prev.to_str().unwrap()
    };
    let ran = run(
        fx,
        "notes",
        TAG,
        RELEASE,
        &[
            "--previous",
            since,
            "--version-json",
            cur.to_str().unwrap(),
            "--previous-version-json",
            prev,
            "--out",
            out.to_str().unwrap(),
        ],
    );
    (ran, out)
}

#[test]
fn the_previous_release_is_the_nearest_stable_ancestor() {
    // Catches: the neighbour in the tag list taken as the previous release (here a
    // release candidate, then a tag on a branch that is not main's history), either of
    // which makes the commit list and the reversibility check about the wrong release.
    let fx = with_tag_zoo("previous-zoo");
    fake_gh(&fx, &["v0.8.0", "v0.9.0-rc1", "v0.9.5"], &[]);
    let output = fx.root.join("out");
    let gh = fx.root.join("gh/gh");
    let (code, o, err) = xtask(&[
        "release",
        "previous",
        "--repo-dir",
        fx.repo().to_str().unwrap(),
        "--repo",
        GH_REPO,
        "--gh",
        gh.to_str().unwrap(),
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
    assert_eq!(err, "");
    assert_eq!(std::fs::read_to_string(output).unwrap(), expected);
    let ((code, o, err), out) = notes_since(&fx, "v0.8.0");
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert!(notes.contains("## Commits since v0.8.0\n"), "{notes}");
}

#[test]
fn a_first_release_has_no_previous_and_the_verb_says_so() {
    // Catches: empty lines missing (the workflow's `if: previous != ''` then reads a
    // stale output) and a tag of no place in `v*` taking another's.
    let fx = fx("previous-none");
    fx.git(&["tag", "v0.20.0", &fx.first]);
    fake_gh(&fx, &["v0.20.0"], &[]);
    for tag in [TAG, "rc1"] {
        let (code, o, err) = previous(&fx, tag, RELEASE);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(o, "previous=\nprevious-commit=\n", "{tag}");
        assert_eq!(err, "", "{tag}");
    }
    let (code, _, err) = previous(&fx, TAG, "abc");
    assert_eq!(code, Some(2), "{err}");
}

#[test]
fn a_newer_tag_without_a_published_release_is_passed_over_and_named() {
    // Catches: the newest tag taken as the previous release when its release run failed
    // before it published (`gh release download` then finds no release and every later
    // release fails), and a tag passed over without a word. v0.9.1's release is a draft;
    // v0.9.0 has none at all; v0.8.0 is published.
    let fx = with_notes("previous-unpublished", |_| {});
    fx.git(&["tag", "v0.8.0", &fx.first]);
    fx.git(&["tag", "v0.9.1", &fx.first]);
    fake_gh(&fx, &["v0.8.0"], &["v0.9.1"]);
    let (code, o, err) = previous(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        o,
        format!("previous=v0.8.0\nprevious-commit={}\n", fx.first)
    );
    let expected = passed_over("v0.9.1", "v0.8.0") + &passed_over("v0.9.0", "v0.8.0");
    assert_eq!(err, expected);
    // The notes of this release list the commits since v0.8.0: the commits of the tags
    // that never published are among them.
    let ((code, o, err), out) = notes_since(&fx, "v0.8.0");
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert!(notes.contains("## Commits since v0.8.0\n"), "{notes}");
}

#[test]
fn with_no_published_release_before_it_a_release_is_the_first() {
    // Catches: an unpublished tag taken as the previous release for want of a published
    // one, and a published release after this one (v0.11.0) taken as before it.
    let fx = with_notes("previous-none-published", |_| {});
    fx.git(&["tag", "v0.8.0", &fx.first]);
    fx.git(&["tag", "v0.11.0", RELEASE]);
    fake_gh(&fx, &["v0.11.0"], &["v0.8.0"]);
    let (code, o, err) = previous(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(o, "previous=\nprevious-commit=\n");
    let named = "none (the first release)";
    let expected = passed_over("v0.9.0", named) + &passed_over("v0.8.0", named);
    assert_eq!(err, expected);
    let ((code, o, err), out) = notes_since(&fx, "");
    assert_eq!(code, Some(0), "{o}{err}");
    let notes = std::fs::read_to_string(out).unwrap();
    assert!(
        notes.contains("## Commits (the first release)\n"),
        "{notes}"
    );
}

#[test]
fn a_published_release_on_a_later_page_is_found() {
    // Catches: a release list read to its first page only, so a release past the first
    // hundred would be no release.
    let fx = with_notes("previous-paged", |_| {});
    let drafts: Vec<_> = (0..100)
        .map(|n| json!({"tag_name": format!("v1.0.{n}"), "draft": true}))
        .collect();
    fake_gh_api(
        &fx,
        json!({
            RELEASES_PAGE_1: drafts,
            "repos/o/r/releases?per_page=100&page=2": [{"tag_name": "v0.9.0", "draft": false}],
        }),
    );
    let (code, o, err) = previous(&fx, TAG, RELEASE);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        o,
        format!("previous=v0.9.0\nprevious-commit={}\n", fx.first)
    );
}

#[test]
fn a_release_list_that_cannot_be_read_is_an_error_not_no_releases() {
    // Catches: a failed or malformed answer read as "nothing is published", which would
    // make a release with a previous one the first release, its notes and checks without it.
    let fx = with_notes("previous-unreadable", |_| {});
    let cases = [
        (json!({}), "gh api repos/o/r/releases: no list of entries"),
        (
            json!({ RELEASES_PAGE_1: {"message": "x"} }),
            "gh api repos/o/r/releases: no list of entries",
        ),
        (
            json!({ RELEASES_PAGE_1: [{"tag_name": "v0.9.0", "draft": "no"}] }),
            "gh api repos/o/r/releases: a release without a tag_name or a boolean draft: \
             {\"draft\":\"no\",\"tag_name\":\"v0.9.0\"}",
        ),
        (
            json!({ RELEASES_PAGE_1: [{"draft": false}] }),
            "gh api repos/o/r/releases: a release without a tag_name or a boolean draft: \
             {\"draft\":false}",
        ),
    ];
    for (api, said) in cases {
        fake_gh_api(&fx, api);
        let (code, o, err) = previous(&fx, TAG, RELEASE);
        assert_eq!(code, Some(1), "{said}: {o}{err}");
        assert!(err.contains(said), "{said}: {err}");
        assert_eq!(o, "", "{said}");
    }
}

#[test]
fn notes_refuse_a_previous_that_is_not_a_release_before_on_the_history() {
    // Catches: `--previous` taken on trust, so a tag on a side branch, a release candidate,
    // a later tag or no tag at all would set the commit range and the build compared.
    let fx = with_tag_zoo("previous-refused");
    fx.git(&["tag", "v0.11.0", RELEASE]);
    for since in ["v0.9.5", "v0.9.0-rc1", "v0.11.0", "v0.7.0", TAG] {
        let ((code, o, err), _) = notes_since(&fx, since);
        assert_eq!(code, Some(1), "{since}: {o}{err}");
        let said = format!(
            "--previous {since}: not a stable release tag before {TAG} that is an ancestor \
             of {RELEASE}"
        );
        assert!(err.contains(&said), "{since}: {err}");
    }
}

#[test]
fn notes_need_the_previous_release_named() {
    // Catches: a workflow that stops passing `--previous`, whose notes would quietly be
    // those of a first release.
    let fx = with_notes("previous-required", |_| {});
    let cur = fx.root.join("version.json");
    std::fs::write(&cur, identity(RELEASE, Some(3), Some(1))).unwrap();
    let out = fx.root.join("notes.md");
    let (dist, repo) = (fx.dist(), fx.repo());
    let (code, _, err) = xtask(&[
        "release",
        "notes",
        dist.to_str().unwrap(),
        "--repo-dir",
        repo.to_str().unwrap(),
        "--tag",
        TAG,
        "--commit",
        RELEASE,
        "--main-ref",
        "main",
        "--version-json",
        cur.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--previous <PREVIOUS>"), "{err}");
}
