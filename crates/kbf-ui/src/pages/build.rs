//! The Build page: one build's summary, the actions that failed, and a page of its
//! actions.

use super::failures::{self, Failure, FailuresPage};
use super::{
    action_label, attempt_end, build_page, failed, invocation_url, outcome, pill, problem,
    query_value, summary,
};
use crate::api::{ActionsPage, Invocation, Row};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, Source};

/// Build: the summary, `/failures` and `/actions`.
pub(crate) const BUILD: Page = Page {
    name: "Build",
    reads: &[
        "/v1/invocations/{id}",
        "/v1/invocations/{id}/failures",
        "/v1/invocations/{id}/actions",
    ],
};

/// The Build page of build `id`; `?page_token=` is passed to `/actions` and
/// `?failures_token=` to `/failures`.
pub(crate) async fn build<S: Source>(source: &S, id: &str, query: &str) -> Answer {
    let here = build_page(id);
    let token = query_value(query, "page_token");
    let failures_token = query_value(query, "failures_token");
    let mut r = Reader::new(source, &BUILD);
    let inv: Invocation = match r.json(&invocation_url(id)).await {
        Ok(inv) => inv,
        Err(e) => return failed(&BUILD, &here, r.asked(), &e),
    };
    // The failures are the first thing a person comes for, but a page of rows is still
    // worth showing when this read fails: the failure is shown in its place.
    let failures: Result<FailuresPage, ReadError> =
        r.json(&failures::url(id, failures_token.as_deref())).await;
    let page = match load_actions(&mut r, id, token.as_deref()).await {
        Ok(page) => page,
        Err(e) => return failed(&BUILD, &here, r.asked(), &e),
    };
    let body = layout::page(BUILD.name, &here, r.asked(), |h| {
        crumbs(h);
        h.el("h1", &[], |h| {
            h.text("Build ").el("code", &[], |h| {
                h.text(&inv.id);
            });
        });
        summary(h, &inv);
        h.el("h2", &[], |h| {
            h.text("Failed actions");
        });
        match &failures {
            Ok(f) => failed_actions(h, &inv, f, &Tokens::of(&here, &failures_token, &token)),
            Err(e) => problem(h, e),
        }
        h.el("h2", &[], |h| {
            h.text("Actions");
        });
        actions(h, &inv.id, &page.actions);
        if let Some(next) = &page.next_page_token {
            let link = Tokens::of(&here, &failures_token, &token).link(&[("page_token", next)]);
            h.el("p", &[attr("class", "pager")], |h| {
                h.link(&link, "Next page");
            });
        }
    });
    Answer::html(200, body)
}

/// The tokens a Build page URL carries, so a link to one pager keeps the other's place.
struct Tokens<'a> {
    here: &'a str,
    failures: Option<&'a str>,
    actions: Option<&'a str>,
}

impl<'a> Tokens<'a> {
    fn of(here: &'a str, failures: &'a Option<String>, actions: &'a Option<String>) -> Self {
        Self {
            here,
            failures: failures.as_deref(),
            actions: actions.as_deref(),
        }
    }

    /// This page with `set` replacing the tokens it names.
    fn link(&self, set: &[(&str, &str)]) -> Href {
        let mut query = form_urlencoded::Serializer::new(String::new());
        for (name, kept) in [
            ("failures_token", self.failures),
            ("page_token", self.actions),
        ] {
            let value = set
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
                .or(kept);
            if let Some(value) = value {
                query.append_pair(name, value);
            }
        }
        let query = query.finish();
        let url = if query.is_empty() {
            self.here.to_owned()
        } else {
            format!("{}?{query}", self.here)
        };
        Href::root_relative(&url).expect("a build page's path is root-relative")
    }
}

async fn load_actions<S: Source>(
    r: &mut Reader<'_, S>,
    id: &str,
    token: Option<&str>,
) -> Result<ActionsPage, ReadError> {
    let mut url = format!("{}/actions", invocation_url(id));
    if let Some(token) = token {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("page_token", token);
        url = format!("{url}?{}", query.finish());
    }
    r.json(&url).await
}

fn crumbs(h: &mut Html) {
    let builds = Href::root_relative(super::BUILDS_PATH).expect("the Builds path is root-relative");
    h.el("p", &[attr("class", "crumbs")], |h| {
        h.link(&builds, "Builds");
    });
}

/// One page of `/failures`: each failed action, linked to its Action page.
fn failed_actions(h: &mut Html, inv: &Invocation, page: &FailuresPage, tokens: &Tokens<'_>) {
    if page.failures.is_empty() {
        let words = if tokens.failures.is_some() {
            "No failed actions on this page."
        } else if inv.counts.failed > 0 {
            "The API lists no failed action, though the build counts failures: its record \
             may have been cut."
        } else {
            "No action failed."
        };
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(words);
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Action",
                    "Outcome",
                    "Failure class",
                    "Attempts",
                    "Instance",
                    "stderr",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for f in &page.failures {
                failure_row(h, &inv.id, f, tokens.failures);
            }
        });
    });
    if let Some(next) = &page.next_page_token {
        let link = tokens.link(&[("failures_token", next)]);
        h.el("p", &[attr("class", "pager")], |h| {
            h.link(&link, "More failed actions");
        });
    }
}

fn failure_row(h: &mut Html, id: &str, f: &Failure, token: Option<&str>) {
    let page = failures::action_page(id, &f.action, token);
    let href = Href::root_relative(&page).expect("an action page's path is root-relative");
    let label = action_label(f.target.as_ref(), f.mnemonic.as_ref(), &f.action);
    let (words, class) = outcome(f.outcome.as_ref());
    let class_cell = f.failure.as_ref().map_or_else(
        || "none".to_owned(),
        |c| format!("{}: {}", c.class, c.reason),
    );
    let stderr = f
        .stderr
        .as_ref()
        .map_or_else(|| "none".to_owned(), failures::stream_words);
    h.el("tr", &[], |h| {
        h.el("td", &[], |h| {
            h.link(&href, &label);
        });
        h.el("td", &[], |h| pill(h, &words, class));
        for cell in [
            &class_cell,
            &f.attempts.len().to_string(),
            &f.instance,
            &stderr,
        ] {
            h.el("td", &[], |h| {
                h.text(cell);
            });
        }
    });
}

/// The rows of one page of `/actions`, each linking to its Action page.
fn actions(h: &mut Html, id: &str, rows: &[Row]) {
    if rows.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No actions on this page.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Action",
                    "Outcome",
                    "How",
                    "Attempts",
                    "Last attempt",
                    "Instance",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for row in rows {
                let page = failures::action_page(id, &row.action, None);
                let href =
                    Href::root_relative(&page).expect("an action page's path is root-relative");
                let label = action_label(row.target.as_ref(), row.mnemonic.as_ref(), &row.action);
                let (words, class) = outcome(row.outcome.as_ref());
                let last = row.attempts.last().map_or_else(String::new, |a| {
                    format!("{} on {}", attempt_end(a), a.worker)
                });
                h.el("tr", &[], |h| {
                    h.el("td", &[], |h| {
                        h.link(&href, &label);
                    });
                    h.el("td", &[], |h| pill(h, &words, class));
                    for cell in [
                        &row.how,
                        &row.attempts.len().to_string(),
                        &last,
                        &row.instance,
                    ] {
                        h.el("td", &[], |h| {
                            h.text(cell);
                        });
                    }
                });
            }
        });
    });
}
