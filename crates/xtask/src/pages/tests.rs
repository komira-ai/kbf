use super::*;

fn page(first: usize, len: usize) -> Vec<usize> {
    (first..first + len).collect()
}

fn failed(e: Error) -> String {
    match e {
        Error::Failed(text) => text,
        other => panic!("not Failed: {other:?}"),
    }
}

#[test]
fn a_short_page_is_the_last_and_an_empty_one_too() {
    let mut pages = Pages::new("repos/o/r/releases");
    assert_eq!(pages.number(), 1);
    assert!(pages.more(&page(0, PER_PAGE)).expect("full"));
    assert_eq!(pages.number(), 2);
    assert!(!pages.more(&page(PER_PAGE, PER_PAGE - 1)).expect("short"));

    let mut empty = Pages::<usize>::new("repos/o/r/releases");
    assert!(!empty.more(&[]).expect("empty"));
}

#[test]
fn a_full_page_equal_to_the_one_before_ends_the_read_with_an_error() {
    // Catches: an endpoint that ignores `page=` (git/matching-refs) read without end.
    let mut pages = Pages::new("repos/o/r/git/matching-refs/tags/v");
    assert!(pages.more(&page(0, PER_PAGE)).expect("page 1"));
    let err = failed(pages.more(&page(0, PER_PAGE)).expect_err("page 2 repeats"));
    assert_eq!(
        err,
        "gh api repos/o/r/git/matching-refs/tags/v: page 2 is page 1 again: the endpoint \
         does not page with `page=`, so the list has no end to read to"
    );
}

#[test]
fn a_short_page_equal_to_nothing_before_it_is_still_the_end() {
    // The first page compared with the empty start: a short or empty first page ends the
    // read, it is not a repeat.
    let mut pages = Pages::<usize>::new("p");
    assert!(!pages.more(&page(0, 3)).expect("short first page"));
}

#[test]
fn full_distinct_pages_end_with_an_error_at_the_cap_and_not_before() {
    // Catches: a list that changes on every page (or a cap off by one) read without end.
    let mut pages = Pages::new("repos/o/r/issues");
    for n in 1..MAX_PAGES {
        assert_eq!(pages.number(), n);
        assert!(
            pages
                .more(&page(n * PER_PAGE, PER_PAGE))
                .expect("under the cap")
        );
    }
    assert_eq!(pages.number(), MAX_PAGES);
    let err = failed(
        pages
            .more(&page(MAX_PAGES * PER_PAGE, PER_PAGE))
            .expect_err("the cap"),
    );
    assert_eq!(
        err,
        "gh api repos/o/r/issues: page 100 is still full (100 entries), the most pages \
         read of one list: stopped rather than read on"
    );
}

#[test]
fn a_short_page_at_the_cap_is_the_end_not_an_error() {
    let mut pages = Pages::new("p");
    for n in 1..MAX_PAGES {
        assert!(
            pages
                .more(&page(n * PER_PAGE, PER_PAGE))
                .expect("under the cap")
        );
    }
    assert!(!pages.more(&page(0, 1)).expect("short last page"));
}
