//! The release list `release plan` reads to number past every release, drafts among
//! them: GitHub's GraphQL `releases`, page after page by cursor, and every way a page
//! can fail to say where the list ends.

use serde_json::{Value, json};

use super::*;

/// A full page of releases, `v9.0.<from>` and the 99 after it.
fn release_page(from: usize) -> Vec<Value> {
    (from..from + 100)
        .map(|n| json!({ "tagName": format!("v9.0.{n}"), "isDraft": false }))
        .collect()
}

/// The release-list reads the fake was asked for, in order.
fn release_reads(asked: &str) -> Vec<&str> {
    asked
        .lines()
        .filter(|l| l.starts_with("api graphql"))
        .collect()
}

#[test]
fn the_releases_are_read_with_graphql_page_after_page_until_none_follows() {
    // Catches: only the first 100 releases read (a draft's number on page 2 reused), a page
    // read after GitHub said none follows, and the release list read through REST, whose
    // pages carry every asset (16 MB a page, which GitHub answered with a 504).
    let fx = fx("release-pages", "9.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    let mut map = serde_json::Map::new();
    map.insert(refs_path(), json!([]));
    map.insert(releases_key(1), releases_answer(1, release_page(0), true));
    let last = vec![json!({ "tagName": "v9.0.100", "isDraft": true })];
    map.insert(releases_key(2), releases_answer(2, last, false));
    map.insert(
        releases_key(3),
        releases_answer(3, release_page(200), false),
    );
    fx.write_answers(map);
    let (code, out, err) = fx.plan(&sha, &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(value(&out, "version"), "9.0.101", "{out}");
    let asked = fx.gh_args();
    let query = format!("api graphql -f query={}", tags::RELEASES_QUERY);
    assert_eq!(
        release_reads(&asked),
        [
            format!("{query} -f owner=komira-ai -f name=kbf"),
            format!("{query} -f owner=komira-ai -f name=kbf -f after=c1"),
        ],
        "{asked}"
    );
    assert!(!asked.contains("/releases"), "{asked}");
}

/// A plan whose release list GitHub answers with `pages` (key, answer); its exit code and
/// stderr, with the release reads it made.
fn plan_over_release_pages(
    name: &str,
    pages: Vec<(String, Value)>,
) -> (Option<i32>, String, usize) {
    let fx = fx(name, "9.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    let mut map = serde_json::Map::new();
    map.insert(refs_path(), json!([]));
    map.extend(pages);
    fx.write_answers(map);
    let (code, out, err) = fx.plan(&sha, &[]);
    assert_eq!(out, "", "{err}");
    let asked = fx.gh_args();
    (code, err.trim_end().to_owned(), release_reads(&asked).len())
}

#[test]
fn a_release_list_that_gives_the_same_cursor_again_fails_the_plan() {
    // Catches: a list whose next page points back at a page already read, read without end
    // (the run that asked for page 2200 and spent the API's rate limit).
    let again = json!({ "data": { "repository": { "releases": {
        "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
        "nodes": release_page(100),
    } } } });
    let (code, err, reads) = plan_over_release_pages(
        "release-repeat",
        vec![
            (releases_key(1), releases_answer(1, release_page(0), true)),
            (releases_key(2), again),
        ],
    );
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: gh api graphql releases of {REPO}: page 2 gives the cursor \"c1\" again, \
             so the list has no end to read to"
        ),
    );
    assert_eq!(reads, 2);
}

#[test]
fn a_release_page_that_says_more_follow_but_gives_no_cursor_fails_the_plan() {
    // Catches: a missing cursor read as "no more pages" (the releases after it never
    // counted) or as page 1 again.
    let no_cursor = json!({ "data": { "repository": { "releases": {
        "pageInfo": { "hasNextPage": true, "endCursor": null },
        "nodes": release_page(0),
    } } } });
    let (code, err, reads) =
        plan_over_release_pages("release-no-cursor", vec![(releases_key(1), no_cursor)]);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: gh api graphql releases of {REPO}: page 1 says another page follows but \
             gives no cursor to it"
        ),
    );
    assert_eq!(reads, 1);
}

#[test]
fn a_release_page_without_its_list_or_its_page_info_fails_the_plan() {
    // Catches: an answer with no repository (GitHub's `data: null`), no nodes, or no
    // boolean hasNextPage read as "no releases" or "the last page", which hands out a
    // number a draft holds.
    let no_nodes = json!({ "data": { "repository": { "releases": {
        "pageInfo": { "hasNextPage": false, "endCursor": null },
    } } } });
    let no_more = json!({ "data": { "repository": { "releases": {
        "pageInfo": { "hasNextPage": "no", "endCursor": null },
        "nodes": [],
    } } } });
    for (name, answer) in [
        ("release-null", json!({ "data": null })),
        ("release-no-nodes", no_nodes),
        ("release-no-more", no_more),
    ] {
        let (code, err, reads) = plan_over_release_pages(name, vec![(releases_key(1), answer)]);
        assert_eq!(code, Some(1), "{name}");
        assert_eq!(
            err,
            format!("xtask: gh api graphql releases of {REPO}: page 1: no list of entries"),
            "{name}"
        );
        assert_eq!(reads, 1, "{name}");
    }
}

#[test]
fn a_release_page_that_is_not_json_fails_the_plan() {
    // Catches: a GraphQL answer that is no JSON (a proxy's HTML error page) read as "no
    // releases".
    let fx = fx("release-garbage", "9.0\n");
    let sha = fx.commit(&[("src/a.rs", Some("a"))]);
    fx.github(&[], &[], &[]);
    std::fs::write(fx.root.join("gh/gh-fault"), "graphql-garbage").expect("gh-fault");
    let (code, out, err) = fx.plan(&sha, &[]);
    assert_eq!((code, out.as_str()), (Some(1), ""), "{err}");
    assert_eq!(
        err.trim_end(),
        "xtask: gh api graphql: not JSON: expected value at line 1 column 1"
    );
}

#[test]
fn a_release_list_that_graphql_refuses_fails_the_plan() {
    // Catches: a failed GraphQL call (an `errors` answer, a 502) read as "no releases".
    let (code, err, reads) = plan_over_release_pages("release-refused", vec![]);
    assert_eq!(code, Some(1));
    assert_eq!(err, "xtask: gh api graphql: gh: Not Found (HTTP 404)");
    assert_eq!(reads, 1);
}

#[test]
fn a_release_list_that_still_goes_on_after_100_pages_fails_the_plan() {
    // Catches: a list whose every page is new and says another follows read without end.
    let pages = (1..=101)
        .map(|page| {
            let nodes = release_page(page as usize * 100);
            (releases_key(page), releases_answer(page, nodes, true))
        })
        .collect();
    let (code, err, reads) = plan_over_release_pages("release-cap", pages);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: gh api graphql releases of {REPO}: page 100 says another page follows, \
             the most pages read of one list: stopped rather than read on"
        ),
    );
    assert_eq!(reads, 100);
}
