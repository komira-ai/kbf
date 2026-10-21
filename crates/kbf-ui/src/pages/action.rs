//! The Action page: one action's answer, with its stdout and stderr as text when the
//! blob route links them and they are small.

use super::failures::{self, Failure, FailuresPage, LogTail};
use super::{
    STREAMS, action_label, attempt_end, build_page, bytes, definitions, failed, invocation_url,
    log_page, micros, outcome, pill, problem, query_value, segment,
};
use crate::api::{Action, Attempt, Blob, Outcome, Output};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, Source};

/// Action: the action's answer, the log blobs it links, and, for a failed action, the
/// failures list that holds its command and log tails.
pub(crate) const ACTION: Page = Page {
    name: "Action",
    reads: &[
        "/v1/invocations/{id}/actions/{digest}",
        "/v1/invocations/{id}/failures",
        "/v1/blobs/{digest}",
    ],
};

/// The most pages of `/failures` the page reads to find a failed action when it is not
/// told which page lists it.
pub(crate) const FAILURES_PAGES_MAX: usize = 5;

/// Where a failed action's command and log tails were looked for.
enum Found {
    /// The action did not fail, so nothing was looked for.
    NotAsked,
    /// Its entry in `/failures`.
    Entry(Box<Failure>),
    /// Not on the pages read; `more` says the list goes on.
    Missing { pages: usize, more: bool },
    /// The list could not be read.
    Unread(ReadError),
}

impl Found {
    fn stream(&self, of: impl Fn(&Failure) -> Option<&LogTail>) -> Option<&LogTail> {
        match self {
            Self::Entry(f) => of(f),
            _ => None,
        }
    }
}

/// `digest` of build `id` in `/failures`: on the page `hint` names when given, else on
/// the first [`FAILURES_PAGES_MAX`] pages.
async fn find<S: Source>(
    r: &mut Reader<'_, S>,
    id: &str,
    digest: &str,
    instance: &str,
    hint: Option<String>,
) -> Found {
    let limit = if hint.is_some() {
        1
    } else {
        FAILURES_PAGES_MAX
    };
    let mut token = hint;
    let mut read = 0;
    loop {
        read += 1;
        let page: FailuresPage = match r.json(&failures::url(id, token.as_deref())).await {
            Ok(page) => page,
            Err(e) => return Found::Unread(e),
        };
        let next = page.next_page_token;
        if let Some(f) = page
            .failures
            .into_iter()
            .find(|f| f.action == digest && f.instance == instance)
        {
            return Found::Entry(Box::new(f));
        }
        match next {
            Some(next) if read < limit => token = Some(next),
            more => {
                return Found::Missing {
                    pages: read,
                    more: more.is_some(),
                };
            }
        }
    }
}

/// The largest log shown on the page; a larger one is a link. The read API's tails
/// (`docs/design/invocations-api.md` 4.7) will replace this read when they land.
pub(crate) const LOG_SHOWN_MAX_BYTES: u64 = 64 * 1024;

/// A log as the page shows it.
enum Log {
    /// The answer has none.
    None,
    /// The blob route is off: no link.
    NoRoute,
    /// The store no longer holds it.
    Gone,
    /// Too large to show, or of a size the digest does not say: a link, when the
    /// API's `href` is one.
    Large(Option<Href>, Option<u64>),
    /// Its text, or why it could not be read.
    Read(Result<String, ReadError>),
    /// The end of the log, as the failures list carries it, and the link that
    /// downloads the whole log.
    Tail(String, String, Option<Href>),
}

/// The Action page of action `digest` of build `id`; `?failures_token=` names the page
/// of `/failures` that lists it.
pub(crate) async fn action<S: Source>(source: &S, id: &str, digest: &str, query: &str) -> Answer {
    let here = format!("{}/actions/{}", build_page(id), segment(digest));
    let mut r = Reader::new(source, &ACTION);
    let url = format!("{}/actions/{}", invocation_url(id), segment(digest));
    let a: Action = match r.json(&url).await {
        Ok(a) => a,
        Err(e) => return failed(&ACTION, &here, r.asked(), &e),
    };
    let found = if failures::is_failure(a.row.outcome.as_ref()) {
        let hint = query_value(query, "failures_token");
        find(&mut r, id, &a.row.action, &a.row.instance, hint).await
    } else {
        Found::NotAsked
    };
    let stdout = log(
        &mut r,
        a.stdout.as_ref(),
        found.stream(|f| f.stdout.as_ref()),
    )
    .await;
    let stderr = log(
        &mut r,
        a.stderr.as_ref(),
        found.stream(|f| f.stderr.as_ref()),
    )
    .await;
    let label = action_label(
        a.row.target.as_ref(),
        a.row.mnemonic.as_ref(),
        &a.row.action,
    );
    let body = layout::page(ACTION.name, &here, r.asked(), |h| {
        let builds =
            Href::root_relative(super::BUILDS_PATH).expect("the Builds path is root-relative");
        let build = Href::root_relative(&build_page(id)).expect("a build page is root-relative");
        h.el("p", &[attr("class", "crumbs")], |h| {
            h.link(&builds, "Builds").text(" / ").link(&build, id);
        });
        h.el("h1", &[], |h| {
            h.text(&label);
        });
        let (words, class) = outcome(a.row.outcome.as_ref());
        h.el("p", &[], |h| pill(h, &words, class));
        let mut fields = vec![
            ("Action", a.row.action.clone()),
            ("Instance", a.row.instance.clone()),
            ("How", a.row.how.clone()),
            (
                "Command",
                a.command.clone().unwrap_or_else(|| "none".to_owned()),
            ),
        ];
        if let Some(Outcome::Ok { exit_code }) = &a.row.outcome {
            fields.push(("Exit code", exit_code.to_string()));
        }
        if let Some(u) = &a.usage {
            fields.extend([
                ("Wall", micros(u.wall_micros)),
                ("CPU user", micros(u.cpu_user_micros)),
                ("CPU system", micros(u.cpu_system_micros)),
                ("Peak memory", bytes(u.peak_memory_bytes)),
            ]);
        }
        definitions(h, &fields);
        attempts(h, &a.row.attempts);
        run_logs(h, &a.row.attempts);
        why(h, &found);
        outputs(h, &a.outputs, a.outputs_truncated);
        show_log(h, "stdout", &stdout);
        show_log(h, "stderr", &stderr);
    });
    Answer::html(200, body)
}

/// What the failures list says of a failed action: its failure class and command, or
/// why they are not shown.
fn why(h: &mut Html, found: &Found) {
    match found {
        Found::NotAsked => {}
        Found::Entry(f) => failures::detail(h, f),
        Found::Missing { pages, more } => {
            h.el("h2", &[], |h| {
                h.text("Failure");
            });
            h.el("p", &[attr("class", "hint")], |h| {
                let tail = if *more {
                    "; the list goes on, so open the failed actions from the Build page"
                } else {
                    ""
                };
                h.text(&format!(
                    "The failure class and command are read from the build's list of failed \
                     actions, which lists no such action in the {pages} page(s) read{tail}."
                ));
            });
        }
        Found::Unread(e) => {
            h.el("h2", &[], |h| {
                h.text("Failure");
            });
            problem(h, e);
        }
    }
}

/// `tail`'s text when the failures list carried one (with the link that downloads the
/// whole log); else `blob`'s text, read through its `href` when it is held, linked and
/// small.
async fn log<S: Source>(r: &mut Reader<'_, S>, blob: Option<&Blob>, tail: Option<&LogTail>) -> Log {
    if let Some(t) = tail
        && let Some(text) = &t.tail
    {
        let link = t.href.as_deref().and_then(failures::download);
        return Log::Tail(failures::stream_words(t), text.clone(), link);
    }
    let Some(blob) = blob else {
        return Log::None;
    };
    if blob.available == Some(false) {
        return Log::Gone;
    }
    let Some(href) = &blob.href else {
        return Log::NoRoute;
    };
    match blob.size_bytes() {
        Some(size) if size <= LOG_SHOWN_MAX_BYTES => Log::Read(r.text(href).await),
        size => Log::Large(Href::root_relative(href), size),
    }
}

fn show_log(h: &mut Html, name: &str, log: &Log) {
    h.el("section", &[attr("class", "log")], |h| {
        h.el("h2", &[], |h| {
            h.text(name);
        });
        match log {
            Log::None => note(h, "None recorded."),
            Log::NoRoute => note(h, "Not linked: the blob route is off on this server."),
            Log::Gone => note(h, "No longer held by the store."),
            Log::Large(href, size) => {
                h.el("p", &[], |h| {
                    let words = size.map_or_else(
                        || "Its size is not known, so it is not shown here. ".to_owned(),
                        |size| format!("{} is too large to show here. ", bytes(size)),
                    );
                    h.text(&words);
                    if let Some(href) = href {
                        h.link(href, "Open the whole log");
                    }
                });
            }
            Log::Read(Ok(text)) => {
                h.el("pre", &[], |h| {
                    h.text(text);
                });
            }
            Log::Read(Err(e)) => problem(h, e),
            Log::Tail(words, text, download) => {
                h.el("p", &[], |h| {
                    h.text(&format!("{words}. "));
                    if let Some(download) = download {
                        h.link(download, "Download the whole log");
                    }
                });
                h.el("pre", &[], |h| {
                    h.text(text);
                });
            }
        }
    });
}

fn note(h: &mut Html, words: &str) {
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(words);
    });
}

fn attempts(h: &mut Html, attempts: &[Attempt]) {
    h.el("h2", &[], |h| {
        h.text("Attempts");
    });
    if attempts.is_empty() {
        note(
            h,
            "None: the action did not run here (a cache hit, or joined another).",
        );
        return;
    }
    h.el("table", &[attr("class", "rows")], |h| {
        h.el("thead", &[], |h| {
            h.el("tr", &[], |h| {
                for name in ["Lease", "Worker", "Queued", "Started", "Ended", "End"] {
                    h.el("th", &[attr("scope", "col")], |h| {
                        h.text(name);
                    });
                }
            });
        });
        h.el("tbody", &[], |h| {
            for a in attempts {
                let cells = [
                    a.lease.clone(),
                    a.worker.clone(),
                    a.queued.clone(),
                    a.started.clone().unwrap_or_default(),
                    a.ended.clone().unwrap_or_default(),
                    attempt_end(a),
                ];
                h.el("tr", &[], |h| {
                    for cell in &cells {
                        h.el("td", &[], |h| {
                            h.text(cell);
                        });
                    }
                });
            }
        });
    });
}

/// The whole logs history kept, one line of links for each attempt that has a run id
/// (a farm without history sends none, and nothing is shown). A run keeps a copy only of
/// a failure's logs: a link to one that was not kept is the Log page's own `404`.
fn run_logs(h: &mut Html, attempts: &[Attempt]) {
    let runs: Vec<(&Attempt, &str)> = attempts
        .iter()
        .filter_map(|a| Some((a, a.run_id.as_deref()?)))
        .collect();
    if runs.is_empty() {
        return;
    }
    h.el("h2", &[], |h| {
        h.text("Run logs");
    });
    h.el("ul", &[attr("class", "outputs")], |h| {
        for (a, run_id) in runs {
            h.el("li", &[], |h| {
                h.text(&format!("Lease {}, run {run_id}: ", a.lease));
                for (i, stream) in STREAMS.into_iter().enumerate() {
                    if i > 0 {
                        h.text(", ");
                    }
                    let href = Href::root_relative(&log_page(run_id, stream))
                        .expect("a log page is root-relative");
                    h.link(&href, stream);
                }
            });
        }
    });
}

fn outputs(h: &mut Html, outputs: &[Output], truncated: bool) {
    h.el("h2", &[], |h| {
        h.text("Outputs");
    });
    if outputs.is_empty() {
        note(h, "None.");
        return;
    }
    h.el("ul", &[attr("class", "outputs")], |h| {
        for o in outputs {
            let size = o.blob.size_bytes().map_or_else(String::new, bytes);
            let kind = if o.directory { "directory" } else { "file" };
            let link = o
                .blob
                .href
                .as_deref()
                .filter(|_| o.blob.available != Some(false))
                .and_then(Href::root_relative);
            h.el("li", &[], |h| {
                h.el("code", &[], |h| match &link {
                    Some(href) => {
                        h.link(href, &o.path);
                    }
                    None => {
                        h.text(&o.path);
                    }
                });
                h.text(&format!(" {kind} {size}"));
            });
        }
    });
    if truncated {
        note(h, "More outputs than the record keeps: the list is cut.");
    }
}
