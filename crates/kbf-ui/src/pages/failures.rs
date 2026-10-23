//! `GET /v1/invocations/{id}/failures`: the document the Build and Action pages read, and
//! how the Action page shows one failed action's command and log tails.
//!
//! Nothing is redacted here: arguments and log text are shown as the farm's API answers
//! them (ruling for the open UI; `docs/api.md`, "The failures view"). Environment
//! variables are names only: the API never sends a value.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{bytes, definitions, micros, segment};
use crate::api::{Attempt, Outcome, Text, known_or_other};
use crate::html::{Href, Html, attr};

/// A page of `/failures`.
#[derive(Debug, Deserialize)]
pub(crate) struct FailuresPage {
    pub(crate) failures: Vec<Failure>,
    pub(crate) next_page_token: Option<String>,
}

/// One action that did not end OK with exit code 0.
#[derive(Debug, Deserialize)]
pub(crate) struct Failure {
    pub(crate) action: String,
    pub(crate) instance: String,
    pub(crate) target: Option<Text>,
    pub(crate) mnemonic: Option<Text>,
    #[serde(default, deserialize_with = "known_or_other")]
    pub(crate) outcome: Option<Outcome>,
    /// The last attempt's failure class; `null` when it did not end in a failure.
    pub(crate) failure: Option<Class>,
    pub(crate) attempts: Vec<Attempt>,
    /// `null` when the Action or Command blob is not held, or the blob route is off.
    pub(crate) command: Option<Command>,
    pub(crate) stdout: Option<LogTail>,
    pub(crate) stderr: Option<LogTail>,
}

/// A failure class and the daemon's reason text.
#[derive(Debug, Deserialize)]
pub(crate) struct Class {
    pub(crate) class: String,
    pub(crate) reason: String,
}

/// The command as the API decoded it from the Action and Command blobs.
#[derive(Debug, Deserialize)]
pub(crate) struct Command {
    pub(crate) arguments: Vec<String>,
    pub(crate) environment_names: Vec<String>,
    pub(crate) working_directory: String,
    pub(crate) platform: BTreeMap<String, String>,
    pub(crate) timeout_micros: Option<u64>,
    pub(crate) output_paths: Vec<String>,
    pub(crate) truncated: bool,
}

/// A log of a failed action, with the tail the API read.
#[derive(Debug, Deserialize)]
pub(crate) struct LogTail {
    pub(crate) href: Option<String>,
    pub(crate) bytes: u64,
    /// The end of the log as text; `null` when the API could not read it.
    pub(crate) tail: Option<String>,
    pub(crate) tail_bytes: Option<u64>,
    pub(crate) truncated: Option<bool>,
}

/// The failures route of build `id`, at the page `token` names (the first when none).
pub(crate) fn url(id: &str, token: Option<&str>) -> String {
    let base = format!("{}/failures", super::invocation_url(id));
    match token {
        None => base,
        Some(token) => {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("page_token", token);
            format!("{base}?{}", query.finish())
        }
    }
}

/// The path of an Action page, with the failures page that holds it when that is not
/// the first, so the page asks for one page and not a search.
pub(crate) fn action_page(id: &str, digest: &str, token: Option<&str>) -> String {
    let page = format!("{}/actions/{}", super::build_page(id), segment(digest));
    match token {
        None => page,
        Some(token) => {
            let mut query = form_urlencoded::Serializer::new(String::new());
            query.append_pair("failures_token", token);
            format!("{page}?{}", query.finish())
        }
    }
}

/// Whether an action with this outcome is one `/failures` lists: any that is not OK with
/// exit code 0 (one still running has no outcome and is not).
pub(crate) const fn is_failure(outcome: Option<&Outcome>) -> bool {
    !matches!(outcome, None | Some(Outcome::Ok { exit_code: 0 }))
}

/// `href` (a blob route link) as the route's download: the same link without the
/// `inline=text` that makes the route answer the bytes as text. The route answers the
/// rest as `Content-Disposition: attachment`.
pub(crate) fn download(href: &str) -> Option<Href> {
    let (path, query) = href.split_once('?').unwrap_or((href, ""));
    let mut kept = form_urlencoded::Serializer::new(String::new());
    for (k, v) in form_urlencoded::parse(query.as_bytes()).filter(|(k, _)| k != "inline") {
        kept.append_pair(&k, &v);
    }
    let kept = kept.finish();
    if kept.is_empty() {
        Href::root_relative(path)
    } else {
        Href::root_relative(&format!("{path}?{kept}"))
    }
}

/// The failure class and reason, and the command, as definition lists and lists.
pub(crate) fn detail(h: &mut Html, f: &Failure) {
    h.el("h2", &[], |h| {
        h.text("Failure");
    });
    let reason = f.failure.as_ref().map_or_else(
        || "none: the last attempt did not end in a failure class".to_owned(),
        |c| format!("{}: {}", c.class, c.reason),
    );
    definitions(h, &[("Failure class", reason)]);
    h.el("h2", &[], |h| {
        h.text("Command");
    });
    let Some(c) = &f.command else {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text(
                "Not available: the Action or Command blob is not held, is too large, or the \
                 blob route is off.",
            );
        });
        return;
    };
    command(h, c);
}

fn command(h: &mut Html, c: &Command) {
    let timeout = c
        .timeout_micros
        .map_or_else(|| "none set".to_owned(), micros);
    let platform = if c.platform.is_empty() {
        "none".to_owned()
    } else {
        c.platform
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    definitions(
        h,
        &[
            ("Working directory", c.working_directory.clone()),
            ("Platform", platform),
            ("Timeout", timeout),
        ],
    );
    list(h, "Arguments", "ol", &c.arguments);
    list(
        h,
        "Environment names (values are never shown)",
        "ul",
        &c.environment_names,
    );
    list(h, "Output paths", "ul", &c.output_paths);
    if c.truncated {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("Cut: the API drops strings from the end to keep the command within 64 KiB.");
        });
    }
}

/// `items` under a `h3` of `title`, each in a `code`.
fn list(h: &mut Html, title: &str, tag: &'static str, items: &[String]) {
    h.el("h3", &[], |h| {
        h.text(title);
    });
    if items.is_empty() {
        h.el("p", &[attr("class", "hint")], |h| {
            h.text("None.");
        });
        return;
    }
    h.el(tag, &[attr("class", "strings")], |h| {
        for item in items {
            h.el("li", &[], |h| {
                h.el("code", &[], |h| {
                    h.text(item);
                });
            });
        }
    });
}

/// How much of a log its tail is: the last of it, the whole of it, or none read.
pub(crate) fn stream_words(t: &LogTail) -> String {
    match (&t.tail, t.truncated) {
        (None, _) => format!("not read ({})", bytes(t.bytes)),
        (Some(_), Some(true)) => format!(
            "last {} of {}",
            bytes(t.tail_bytes.unwrap_or(0)),
            bytes(t.bytes)
        ),
        (Some(_), _) => format!("whole log, {}", bytes(t.bytes)),
    }
}
