//! The opt-in auto-refresh of a page: `?refresh=N`, N one of [`INTERVALS`].
//!
//! The page reloads itself with `<meta http-equiv="refresh" content="N">` (only the
//! integer, so the browser comes back to the URL it is on, `refresh` included). Any other
//! query parameter, a repeated `refresh`, or a value outside [`INTERVALS`] is a `400`
//! page that says so; nothing is clamped or ignored. A page that failed to read does not
//! refresh: a tab left open on an outage does not ask a restarting server again and
//! again.

use crate::Answer;
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::Page;

/// The seconds a page may be asked to refresh at.
pub(crate) const INTERVALS: [u32; 4] = [5, 10, 30, 60];

/// The refresh a query asks for: `None` for none, or why the query is refused.
pub(crate) fn parse(query: &str) -> Result<Option<u32>, String> {
    let mut found = None;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if key != "refresh" {
            return Err(format!("{key} is not a parameter of this page"));
        }
        if found.is_some() {
            return Err("refresh is given more than once".to_owned());
        }
        // The value as the page writes it: no sign, no leading zero, no space.
        let seconds = INTERVALS
            .into_iter()
            .find(|s| s.to_string() == value)
            .ok_or_else(|| format!("refresh must be one of {}", list()))?;
        found = Some(seconds);
    }
    Ok(found)
}

fn list() -> String {
    let all: Vec<String> = INTERVALS.iter().map(ToString::to_string).collect();
    all.join(", ")
}

/// The `400` page for a query [`parse`] refused.
pub(crate) fn refused(page: &Page, here: &str, why: &str) -> Answer {
    let body = layout::page(page.name, here, &[], |h| {
        h.el("h1", &[], |h| {
            h.text(page.name);
        });
        h.el("p", &[attr("class", "problem")], |h| {
            h.text("This query is refused: ").text(why).text(".");
        });
    });
    Answer::html(400, body)
}

/// The line that sets the refresh: each interval a link to this page with it, the
/// current one plain text marked `aria-current`, so the choice is not by colour.
/// `here` is the page's own path.
pub(crate) fn controls(h: &mut Html, here: &str, current: Option<u32>) {
    h.el("p", &[attr("class", "refresh")], |h| {
        h.text("Auto-refresh: ");
        let choices = std::iter::once((None, "off".to_owned()))
            .chain(INTERVALS.iter().map(|s| (Some(*s), format!("{s} s"))));
        for (seconds, label) in choices {
            let url = seconds.map_or_else(|| here.to_owned(), |s| format!("{here}?refresh={s}"));
            if seconds == current {
                h.el(
                    "span",
                    &[attr("class", "current"), attr("aria-current", "true")],
                    |h| {
                        h.text(&label);
                    },
                );
            } else {
                let href = Href::root_relative(&url).expect("a page's path is root-relative");
                h.link(&href, &label);
            }
            h.text(" ");
        }
    });
}

#[cfg(test)]
mod tests;
