//! The opt-in refresh of the Overview, Nodes and Node pages. Each test names the
//! defect it catches.

use super::{INTERVALS, parse};
use crate::tests::{body, fleet_farm, render};

/// The pages that take `?refresh=`, with their paths.
const PAGES: [&str; 3] = ["/ui", "/ui/nodes", "/ui/nodes/mac-1"];

/// Catches: a query that is not exactly one `refresh` of an allowed interval accepted
/// (clamped, defaulted or ignored): a value outside the set, empty, negative, with a
/// sign or a space, a repeated `refresh`, an unknown parameter, and one parameter
/// besides a good one (catches: the set check dropped; the duplicate check dropped; the
/// key check dropped).
#[test]
fn only_one_refresh_of_an_allowed_interval_is_accepted() {
    assert_eq!(parse(""), Ok(None));
    for seconds in INTERVALS {
        assert_eq!(parse(&format!("refresh={seconds}")), Ok(Some(seconds)));
    }
    for bad in [
        "refresh=0",
        "refresh=1",
        "refresh=7",
        "refresh=61",
        "refresh=",
        "refresh=-5",
        "refresh=%2B5",
        "refresh=+5",
        "refresh=5.0",
        "refresh=05",
        "refresh=%205",
        "refresh=ten",
        "refresh=4294967301",
        "refresh=10&refresh=10",
        "refresh=10&refresh=30",
        "refresh",
        "other=1",
        "refresh=10&other=1",
        "Refresh=10",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

/// Catches: the refresh not written as `<meta http-equiv="refresh" content="N">` with
/// only the integer (a `url=` part would send a browser elsewhere); a page that
/// refreshes without being asked; a bad query answered with a page instead of a `400`
/// that names the rule; the choice of interval missing, or its current value marked by
/// colour only.
#[test]
fn a_page_refreshes_only_when_asked_and_only_to_itself() {
    let fake = fleet_farm();
    for path in PAGES {
        let plain = body(&render(&fake, path));
        assert!(!plain.contains("http-equiv"), "{path}: {plain}");
        assert!(
            plain.contains("<span class=\"current\" aria-current=\"true\">off</span>"),
            "{path}: {plain}"
        );
        for seconds in INTERVALS {
            let a = render(&fake, &format!("{path}?refresh={seconds}"));
            assert_eq!(a.status(), 200, "{path}");
            let html = body(&a);
            assert!(
                html.contains(&format!(
                    "<meta http-equiv=\"refresh\" content=\"{seconds}\">"
                )),
                "{path}: {html}"
            );
            assert_eq!(html.matches("http-equiv").count(), 1, "{path}");
            assert!(!html.contains("url="), "{path}: {html}");
            assert!(
                html.contains(&format!(
                    "<span class=\"current\" aria-current=\"true\">{seconds} s</span>"
                )),
                "{path}: {html}"
            );
            // Every other choice is a link back to this page.
            assert!(
                html.contains(&format!("<a href=\"{path}\">off</a>")),
                "{path}: {html}"
            );
        }
        for query in ["refresh=7", "refresh=5&refresh=5", "x=1"] {
            let a = render(&fake, &format!("{path}?{query}"));
            let html = body(&a);
            assert_eq!(a.status(), 400, "{path}?{query}");
            assert!(html.contains("This query is refused: "), "{html}");
            assert!(!html.contains("http-equiv"), "{html}");
        }
    }
    // The parameter's name is the caller's: it is text on the refusal page.
    let html = body(&render(&fake, "/ui/nodes?%3Cscript%3E=1"));
    assert!(html.contains("&lt;script&gt; is not a parameter"), "{html}");
    assert!(!html.contains("<script"), "{html}");
}

/// Catches: a refusal that reads the API first (a bad query must cost nothing).
#[test]
fn a_refused_query_reads_nothing() {
    let fake = fleet_farm();
    for path in PAGES {
        let _ = render(&fake, &format!("{path}?refresh=3"));
    }
    assert!(fake.seen().is_empty(), "{:?}", fake.seen());
}
