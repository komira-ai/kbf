//! The Builds page: the recent builds the farm's history lists, filtered by state and
//! instance, and a lookup of one build by id.

use serde::Deserialize;

use super::{BUILDS_PATH, build_page, history_off, invocation_url, pill, problem, summary};
use crate::api::Invocation;
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, Source};

/// Builds: the history list, and one build by id.
pub(crate) const BUILDS: Page = Page {
    name: "Builds",
    reads: &["/v1/invocations", "/v1/invocations/{id}"],
};

/// The longest instance filter the page forwards: an instance name is a REAPI name.
const INSTANCE_MAX_BYTES: usize = 256;

/// A page of `GET /v1/invocations`.
#[derive(Debug, Deserialize)]
struct List {
    invocations: Vec<Listed>,
    /// The link to the next page, with every filter kept.
    next_page: Option<String>,
    /// Some instance's list could not be read in full.
    #[serde(default)]
    partial: bool,
}

/// One build of the list.
#[derive(Debug, Deserialize)]
struct Listed {
    id: String,
    first_seen: String,
    last_seen: String,
    state: String,
    complete: bool,
    counts: Listing,
    instances: Vec<String>,
}

/// The counters the list carries.
#[derive(Debug, Deserialize)]
struct Listing {
    ran: u64,
    hits: u64,
    failed: u64,
}

/// What the page was asked for.
#[derive(Default)]
struct Asked {
    id: Option<String>,
    state: Option<String>,
    instance: Option<String>,
    token: Option<String>,
}

/// The query as [`Asked`], or why it is refused: a parameter the page does not take, one
/// given twice, a state that is not `open` or `closed`, an instance too long.
fn parse(query: &str) -> Result<Asked, String> {
    let mut asked = Asked::default();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let slot = match &*key {
            "id" => &mut asked.id,
            "state" => &mut asked.state,
            "instance" => &mut asked.instance,
            "page_token" => &mut asked.token,
            other => return Err(format!("{other:?} is not a parameter of this page")),
        };
        if slot.is_some() {
            return Err(format!("{key:?} is given twice"));
        }
        *slot = Some(value.into_owned());
    }
    // An empty value is the form's "any".
    for slot in [
        &mut asked.id,
        &mut asked.state,
        &mut asked.instance,
        &mut asked.token,
    ] {
        if slot.as_deref() == Some("") {
            *slot = None;
        }
    }
    match asked.state.as_deref() {
        None | Some("open" | "closed") => {}
        Some(other) => return Err(format!("state={other:?}: open or closed")),
    }
    if asked
        .instance
        .as_ref()
        .is_some_and(|i| i.len() > INSTANCE_MAX_BYTES)
    {
        return Err(format!("instance is over {INSTANCE_MAX_BYTES} bytes"));
    }
    Ok(asked)
}

/// `path` with the filters of `a`, at the page `token` names.
fn with_filters(path: &str, a: &Asked, token: Option<&str>) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (key, value) in [
        ("state", a.state.as_deref()),
        ("instance", a.instance.as_deref()),
        ("page_token", token),
    ] {
        if let Some(value) = value {
            query.append_pair(key, value);
        }
    }
    let query = query.finish();
    if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    }
}

/// The token of a next-page link (`/v1/invocations?...&page_token=<t>`).
fn token_of(next_page: &str) -> Option<String> {
    let (_, query) = next_page.split_once('?')?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "page_token")
        .map(|(_, v)| v.into_owned())
}

/// The Builds page: with `?id=`, that build's summary; else the list.
pub(crate) async fn builds<S: Source>(source: &S, query: &str) -> Answer {
    let asked = match parse(query) {
        Ok(asked) => asked,
        Err(why) => return refused(&why),
    };
    // An id of `.` or `..` names no build: written into `/v1/invocations/{id}` it is a
    // dot segment, which a URL client resolves to another route.
    let dot = matches!(asked.id.as_deref(), Some("." | ".."));
    let mut r = Reader::new(source, &BUILDS);
    let (found, listed) = match &asked.id {
        Some(id) if !dot => (Some(r.json::<Invocation>(&invocation_url(id)).await), None),
        Some(_) => (None, None),
        None => {
            let url = with_filters("/v1/invocations", &asked, asked.token.as_deref());
            (None, Some(r.json::<List>(&url).await))
        }
    };
    let failed = match (&found, &listed) {
        (Some(Err(e)), _) | (_, Some(Err(e))) => Some(e),
        _ => None,
    };
    let off = failed.is_some_and(ReadError::is_history_off);
    let status = if off {
        200
    } else if let Some(e) = failed {
        e.status()
    } else if dot {
        404
    } else {
        200
    };
    let passed_on = failed.filter(|_| !off).map_or(&[][..], ReadError::headers);
    let body = layout::page(BUILDS.name, BUILDS_PATH, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Builds");
        });
        lookup(h, asked.id.as_deref().unwrap_or_default());
        if off {
            history_off(h);
        } else if let Some(e) = failed {
            problem(h, e);
        } else if let Some(Ok(inv)) = &found {
            h.el("section", &[attr("class", "card")], |h| {
                h.el("h2", &[], |h| {
                    let href = Href::root_relative(&build_page(&inv.id))
                        .expect("a build page's path is root-relative");
                    h.link(&href, &inv.id);
                });
                summary(h, inv);
            });
        } else if let Some(Ok(list)) = &listed {
            show_list(h, &asked, list);
        } else {
            h.el("p", &[attr("class", "problem")], |h| {
                h.text("A build id of . or .. names no build.");
            });
        }
    });
    Answer::html(status, body).passing_on(passed_on)
}

/// The `400` answer for a query the page does not take.
fn refused(why: &str) -> Answer {
    let body = layout::page(BUILDS.name, BUILDS_PATH, &[], |h| {
        h.el("h1", &[], |h| {
            h.text("Builds");
        });
        lookup(h, "");
        h.el("p", &[attr("class", "problem")], |h| {
            h.text(why);
        });
    });
    Answer::html(400, body)
}

/// The filter form, and the list under it.
fn show_list(h: &mut Html, asked: &Asked, list: &List) {
    filters(h, asked);
    if list.partial {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(
                "Partial: the list of some instance was not read in full, so builds may be \
                 missing.",
            );
        });
    }
    if list.invocations.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("No builds match.");
        });
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in [
                    "Build",
                    "Result",
                    "Instances",
                    "First seen",
                    "Last seen",
                    "Ran",
                    "Cache hits",
                    "Failed",
                ] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for b in &list.invocations {
                row(h, b);
            }
        });
    });
    if let Some(token) = list.next_page.as_deref().and_then(token_of) {
        let next = Href::root_relative(&with_filters(BUILDS_PATH, asked, Some(&token)))
            .expect("the Builds path is root-relative");
        h.el("p", &[attr("class", "pager")], |h| {
            h.link(&next, "Next page");
        });
    }
}

/// A build's result in words and the class that colours it: its words carry the
/// meaning, the colour only repeats them.
fn result(b: &Listed) -> (String, &'static str) {
    if b.counts.failed > 0 {
        (format!("{} failed", b.counts.failed), "fail")
    } else if b.state != "closed" {
        (b.state.clone(), "pending")
    } else if b.complete {
        ("ok".to_owned(), "ok")
    } else {
        ("closed, incomplete".to_owned(), "other")
    }
}

fn row(h: &mut Html, b: &Listed) {
    let href = Href::root_relative(&build_page(&b.id)).expect("a build page is root-relative");
    let (words, class) = result(b);
    let instances = b.instances.join(", ");
    h.el("tr", &[], |h| {
        h.el("td", &[], |h| {
            h.link(&href, &b.id);
        });
        h.el("td", &[], |h| pill(h, &words, class));
        for cell in [
            &instances,
            &b.first_seen,
            &b.last_seen,
            &b.counts.ran.to_string(),
            &b.counts.hits.to_string(),
            &b.counts.failed.to_string(),
        ] {
            h.el("td", &[], |h| {
                h.text(cell);
            });
        }
    });
}

/// The state and instance filters, a GET form to this page.
fn filters(h: &mut Html, a: &Asked) {
    let action = Href::root_relative(BUILDS_PATH).expect("the Builds path is root-relative");
    h.el(
        "form",
        &[
            attr("method", "get"),
            crate::html::url("action", &action),
            attr("class", "lookup"),
        ],
        |h| {
            h.el("label", &[attr("for", "state")], |h| {
                h.text("State");
            });
            h.el(
                "select",
                &[attr("id", "state"), attr("name", "state")],
                |h| {
                    for (value, label) in [("", "any"), ("open", "open"), ("closed", "closed")] {
                        let chosen = a.state.as_deref().unwrap_or_default() == value;
                        let mut attrs = vec![attr("value", value)];
                        if chosen {
                            attrs.push(attr("selected", "selected"));
                        }
                        h.el("option", &attrs, |h| {
                            h.text(label);
                        });
                    }
                },
            );
            h.el("label", &[attr("for", "instance")], |h| {
                h.text("Instance");
            });
            h.void(
                "input",
                &[
                    attr("id", "instance"),
                    attr("name", "instance"),
                    attr("type", "text"),
                    attr("value", a.instance.as_deref().unwrap_or_default()),
                    attr("autocomplete", "off"),
                    attr("spellcheck", "false"),
                ],
            );
            h.void("input", &[attr("type", "submit"), attr("value", "Filter")]);
        },
    );
}

/// The id form; `id` fills it in.
fn lookup(h: &mut Html, id: &str) {
    let action = Href::root_relative(BUILDS_PATH).expect("the Builds path is root-relative");
    h.el(
        "form",
        &[
            attr("method", "get"),
            crate::html::url("action", &action),
            attr("class", "lookup"),
        ],
        |h| {
            h.el("label", &[attr("for", "id")], |h| {
                h.text("Build id");
            });
            h.void(
                "input",
                &[
                    attr("id", "id"),
                    attr("name", "id"),
                    attr("type", "text"),
                    attr("value", id),
                    attr("required", "required"),
                    attr("autocomplete", "off"),
                    attr("spellcheck", "false"),
                ],
            );
            h.void("input", &[attr("type", "submit"), attr("value", "Look up")]);
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{Listed, Listing, result};

    /// Catches: a result worded from the wrong field: an incomplete closed build shown as
    /// ok (mutant: `complete` ignored), a build with failures that is still open shown as
    /// open only.
    #[test]
    fn a_listed_builds_result_is_its_failures_then_its_state() {
        let build = |state: &str, complete: bool, failed: u64| Listed {
            id: "b".to_owned(),
            first_seen: String::new(),
            last_seen: String::new(),
            state: state.to_owned(),
            complete,
            counts: Listing {
                ran: 0,
                hits: 0,
                failed,
            },
            instances: Vec::new(),
        };
        for (b, words, class) in [
            (build("closed", true, 0), "ok", "ok"),
            (build("closed", false, 0), "closed, incomplete", "other"),
            (build("open", false, 0), "open", "pending"),
            (build("open", false, 3), "3 failed", "fail"),
            (build("closed", true, 2), "2 failed", "fail"),
        ] {
            assert_eq!(result(&b), (words.to_owned(), class));
        }
    }
}
