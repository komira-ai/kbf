//! The Log page: one run's whole log as escaped text, read in one ranged request.
//!
//! A log is untrusted: whatever a build printed. It is shown as text in a `<pre>`
//! (escaped by [`Html`], like every string), with the control characters that a
//! browser or a terminal acts on written as visible symbols, and with bytes that are
//! not UTF-8 replaced. Nothing is redacted (CEO ruling: the UI is open and shows what
//! the farm recorded; the page says so). At most [`LOG_SHOWN_BYTES`] are read, with a
//! `Range` header, and the page says when the log is longer and links the route that
//! sends all of it as a download.

use super::{bytes, definitions, failed, segment};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, Reader};
use crate::{Answer, PREFIX, Source};

/// Log: the copy of a failed run's log that history kept.
pub(crate) const LOG: Page = Page {
    name: "Log",
    reads: &["/v1/runs/{run_id}/logs/{stream}"],
};

/// The most of a log the page reads and shows.
pub(crate) const LOG_SHOWN_BYTES: u64 = 1024 * 1024;

/// The logs of a run, as the API names them.
pub(crate) const STREAMS: [&str; 3] = ["stdout", "stderr", "result"];

/// `/ui/runs/{run_id}/logs/{stream}`.
pub(crate) fn log_page(run_id: &str, stream: &str) -> String {
    format!("{PREFIX}/runs/{}/logs/{stream}", segment(run_id))
}

/// `/v1/runs/{run_id}/logs/{stream}`: the route that sends the whole log as a download.
fn raw_url(run_id: &str, stream: &str) -> String {
    format!("/v1/runs/{}/logs/{stream}", segment(run_id))
}

/// The Log page of `stream` of run `run_id`.
pub(crate) async fn run_log<S: Source>(source: &S, run_id: &str, stream: &str) -> Answer {
    let here = log_page(run_id, stream);
    let url = raw_url(run_id, stream);
    let mut r = Reader::new(source, &LOG);
    let slice = match r.range(&url, 0, LOG_SHOWN_BYTES - 1).await {
        Ok(slice) => slice,
        Err(e) => return failed(&LOG, &here, r.asked(), &e),
    };
    let mut got = slice.bytes;
    // A source that ignored the range sent more than was asked for: show what was.
    let cap = usize::try_from(LOG_SHOWN_BYTES).unwrap_or(usize::MAX);
    got.truncate(cap);
    let shown = got.len() as u64;
    let more = match slice.total {
        Some(total) => total > shown,
        None => shown >= LOG_SHOWN_BYTES,
    };
    let text = if got.is_empty() {
        None
    } else {
        let whole = if more { without_split_char(&got) } else { &got };
        Some(printable(&String::from_utf8_lossy(whole)))
    };
    let download = Href::root_relative(&url).expect("a run log's route is root-relative");
    let builds = Href::root_relative(super::BUILDS_PATH).expect("the Builds path is root-relative");
    let body = layout::page(LOG.name, &here, r.asked(), |h| {
        h.el("p", &[attr("class", "crumbs")], |h| {
            h.link(&builds, "Builds");
        });
        h.el("h1", &[], |h| {
            h.text(&format!("{stream} of run {run_id}"));
        });
        let size = slice.total.map_or_else(|| "unknown".to_owned(), bytes);
        definitions(
            h,
            &[
                ("Run", run_id.to_owned()),
                ("Log", stream.to_owned()),
                ("Size", size),
                ("Shown", bytes(shown)),
            ],
        );
        note(
            h,
            "Unredacted: the log is shown as the farm recorded it, secrets included.",
        );
        h.el("p", &[], |h| {
            if more {
                h.text(&format!(
                    "Cut: only the first {} of the log is shown. ",
                    bytes(LOG_SHOWN_BYTES)
                ));
            }
            h.link(&download, "Download the whole log");
            h.text(" (a file, not shown by the browser).");
        });
        match &text {
            None => note(h, "The log is empty."),
            Some(text) => {
                h.el("section", &[attr("class", "log")], |h| {
                    h.el("pre", &[], |h| {
                        h.text(text);
                    });
                });
            }
        }
    });
    Answer::html(200, body)
}

fn note(h: &mut Html, words: &str) {
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(words);
    });
}

/// `bytes` without a trailing, incomplete UTF-8 sequence: where a cut fell inside a code
/// point, the part of it that is there would be shown as a replacement character that
/// is not in the log.
fn without_split_char(bytes: &[u8]) -> &[u8] {
    for tail in 1..=3.min(bytes.len()) {
        let end = bytes.len() - tail;
        if let Err(e) = std::str::from_utf8(&bytes[end..])
            && e.valid_up_to() == 0
            && e.error_len().is_none()
        {
            return &bytes[..end];
        }
    }
    bytes
}

/// `text` with every character that something acts on written as one that does not:
/// the C0 controls but newline, tab and carriage return as their control pictures
/// (U+2400 to U+241F), DEL as U+2421, and the C1 controls and the bidirectional
/// overrides and marks (which reorder text on screen) as U+FFFD.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' | '\r' => c,
            '\u{0}'..='\u{1f}' => char::from_u32(0x2400 + u32::from(c)).unwrap_or('\u{fffd}'),
            '\u{7f}' => '\u{2421}',
            '\u{80}'..='\u{9f}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => '\u{fffd}',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests;
