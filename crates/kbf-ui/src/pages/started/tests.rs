//! The restart banner's start time on the Overview page:
//! what it says for each answer `GET /v1/health` can give, and that a page never fails
//! (or is read as healthy) because of it. Each test names the defect it catches.

use serde_json::json;

use super::{Started, note, rfc3339};
use crate::html::Html;
use crate::tests::{Fake, STARTED_MS, STARTED_SAYS, body, fleet_farm, health_doc, render};

/// Every page that carries the banner.
const PAGES: [&str; 1] = ["/ui"];

/// The read API that answers `path`, new.
fn farm_for(_path: &str) -> Fake {
    fleet_farm()
}

/// The words of the banner's start note on a page, as the page wrote them.
fn start_note(html: &str) -> Option<&str> {
    let from = html.find("<p class=\"hint\">This server started")?;
    let rest = &html[from..];
    let to = rest.find("</p>")?;
    Some(&rest[..to])
}

/// Catches: a page of the banner without the start time, or with another one than the
/// server's: the fixture's `started_unix_ms` is 2026-10-21T08:30:15Z (a time written from
/// the wrong unit, the seconds read as milliseconds or the reverse, a date off by a
/// day or an hour shows) and its `uptime_ms` 2 h 5 min. Catches: the note dropped from any
/// one page; `rfc3339` fed seconds; `uptime_ms` shown as the age of something else.
#[test]
fn every_page_of_the_banner_says_when_the_server_started() {
    for path in PAGES {
        let a = render(&farm_for(path), path);
        let html = body(&a);
        assert_eq!(a.status(), 200, "{path}: {html}");
        assert!(
            html.contains(&format!("<p class=\"hint\">{STARTED_SAYS}</p>")),
            "{path}: {html}"
        );
        assert!(!html.contains("is not known"), "{path}");
    }
}

/// Catches: the Overview, which has the health in hand, making up a time when its own
/// document has none (an older server), or failing; and a start time without an uptime
/// shown with an "ago" nobody sent. Catches: `Started::of` returning `At` for `None`;
/// the uptime defaulted to zero.
#[test]
fn the_overview_reads_the_start_from_its_own_health_document() {
    let mut old = health_doc();
    let keys = old.as_object_mut().expect("an object");
    keys.remove("started_unix_ms");
    keys.remove("uptime_ms");
    let a = render(&fleet_farm().json("/v1/health", &old), "/ui");
    let html = body(&a);
    assert_eq!(a.status(), 200, "{html}");
    assert!(
        html.contains("When this server started is not known: /v1/health does not say."),
        "{html}"
    );

    let mut no_uptime = health_doc();
    no_uptime
        .as_object_mut()
        .expect("an object")
        .remove("uptime_ms");
    let html = body(&render(&fleet_farm().json("/v1/health", &no_uptime), "/ui"));
    assert!(
        html.contains("<p class=\"hint\">This server started at 2026-10-21T08:30:15Z.</p>"),
        "{html}"
    );
    assert!(!html.contains(" ago "), "{html}");
}

/// Catches (hostile fixture): the largest numbers a server can send, which a page that
/// adds, subtracts or converts them to a signed type would panic on or wrap: the
/// start time and uptime of `u64::MAX`, and zero. Both render, the page stays `200`, and
/// the zero start is the Unix epoch. Catches: `rfc3339` in a narrower type; the uptime
/// subtracted from the start.
#[test]
fn extreme_start_times_render_without_panic() {
    for path in PAGES {
        let mut huge = health_doc();
        huge["started_unix_ms"] = json!(u64::MAX);
        huge["uptime_ms"] = json!(u64::MAX);
        let fake = farm_for(path).json("/v1/health", &huge);
        let a = render(&fake, path);
        let html = body(&a);
        assert_eq!(a.status(), 200, "{path}: {html}");
        let said = start_note(&html).expect("the note");
        assert!(said.contains(" d "), "{path}: {said}");
        assert!(said.ends_with(" ago by its own clock."), "{path}: {said}");
    }
    let mut epoch = health_doc();
    epoch["started_unix_ms"] = json!(0);
    epoch["uptime_ms"] = json!(0);
    let html = body(&render(&fleet_farm().json("/v1/health", &epoch), "/ui"));
    assert!(
        html.contains("This server started at 1970-01-01T00:00:00Z, 0 ms ago by its own clock."),
        "{html}"
    );
}

/// Catches: the banner's wording or formatting wrong, written straight (not through a
/// page): a day-long uptime, one under a second, one not sent, and the reason of a missing
/// time written escaped. Catches: the sentence without its full stop; the uptime unit
/// dropped; the reason written raw.
#[test]
fn the_note_is_one_sentence_each_way() {
    let said = |started: &Started| {
        let mut h = Html::default();
        note(&mut h, started);
        h.finish()
    };
    let at = |uptime_ms| Started::At {
        unix_ms: STARTED_MS,
        uptime_ms,
    };
    assert_eq!(
        said(&at(Some(86_400_000))),
        "<p class=\"hint\">This server started at 2026-10-21T08:30:15Z, 1 d 0 h ago by its own clock.</p>"
    );
    assert_eq!(
        said(&at(Some(999))),
        "<p class=\"hint\">This server started at 2026-10-21T08:30:15Z, 999 ms ago by its own clock.</p>"
    );
    assert_eq!(
        said(&at(None)),
        "<p class=\"hint\">This server started at 2026-10-21T08:30:15Z.</p>"
    );
    assert_eq!(
        said(&Started::Unsaid("<b>why</b>".to_owned())),
        "<p class=\"hint\">When this server started is not known: &lt;b&gt;why&lt;/b&gt;.</p>"
    );
}

/// Catches: a time written wrongly: the epoch, a leap day, a time of day.
#[test]
fn a_time_is_written_as_rfc_3339() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
    assert_eq!(rfc3339(1_709_210_096_999), "2024-02-29T12:34:56Z");
    assert_eq!(rfc3339(1_791_370_000_000), "2026-10-07T10:46:40Z");
    assert_eq!(rfc3339(946_684_799_000), "1999-12-31T23:59:59Z");
    assert_eq!(rfc3339(946_684_800_000), "2000-01-01T00:00:00Z");
}
