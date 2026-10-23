//! The Log page: one run's log as escaped text, read in one ranged request.
//!
//! A log is untrusted: whatever a build printed. It is shown as text in a `<pre>`
//! (escaped by [`Html`], like every string), with the control characters that a
//! browser or a terminal acts on written as visible symbols, and with bytes that are
//! not UTF-8 replaced. Nothing is redacted (ruling: the UI is open and shows what
//! the farm recorded; the page says so). At most [`LOG_SHOWN_BYTES`] are read, with a
//! `Range` header: the LAST that many by default (`bytes=-N`, because what a failed
//! build says is at its end), the first with `?from=start`. The page says when the log
//! is longer, names the end it shows, links the other end, and links the route that
//! sends all of it as a download.

use super::{bytes, definitions, failed, query_value, segment};
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

/// Which end of the log the page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// The first [`LOG_SHOWN_BYTES`]: `?from=start`.
    Head,
    /// The last [`LOG_SHOWN_BYTES`]: the default.
    Tail,
}

impl End {
    /// `from=start` asks for the head; anything else, or nothing, is the tail.
    fn of(query: &str) -> Self {
        match query_value(query, "from").as_deref() {
            Some("start") => Self::Head,
            _ => Self::Tail,
        }
    }
}

/// The Log page of `stream` of run `run_id`; `query` may hold `from=start`.
pub(crate) async fn run_log<S: Source>(
    source: &S,
    run_id: &str,
    stream: &str,
    query: &str,
) -> Answer {
    let here = log_page(run_id, stream);
    let url = raw_url(run_id, stream);
    let end = End::of(query);
    let mut r = Reader::new(source, &LOG);
    let read = match end {
        End::Head => r.range(&url, 0, LOG_SHOWN_BYTES - 1).await,
        End::Tail => r.suffix(&url, LOG_SHOWN_BYTES).await,
    };
    let slice = match read {
        Ok(slice) => slice,
        Err(e) => return failed(&LOG, &here, r.asked(), &e),
    };
    let mut got = slice.bytes;
    // A source that ignored the range sent more than was asked for: show what was, from
    // the end of it that was asked for.
    let cap = usize::try_from(LOG_SHOWN_BYTES).unwrap_or(usize::MAX);
    match end {
        End::Head => got.truncate(cap),
        End::Tail => {
            got.drain(..got.len().saturating_sub(cap));
        }
    }
    let shown = got.len() as u64;
    let more = match slice.total {
        Some(total) => total > shown,
        None => shown >= LOG_SHOWN_BYTES,
    };
    let text = if got.is_empty() {
        None
    } else {
        let whole = match (more, end) {
            (false, _) => &got[..],
            (true, End::Head) => without_split_char(&got),
            (true, End::Tail) => without_split_start(&got),
        };
        Some(printable(&String::from_utf8_lossy(whole)))
    };
    let other = match end {
        End::Head => (here.clone(), "Show the end of the log"),
        End::Tail => (format!("{here}?from=start"), "Show the start of the log"),
    };
    let other_href = Href::root_relative(&other.0).expect("this page's path is root-relative");
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
                let which = match end {
                    End::Head => "first",
                    End::Tail => "last",
                };
                h.text(&format!(
                    "Cut: only the {which} {} of the log is shown. ",
                    bytes(LOG_SHOWN_BYTES)
                ));
                h.link(&other_href, other.1);
                h.text(". ");
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

/// `bytes` without a leading run of continuation bytes (at most three, the most that
/// belong to one code point): where a tail window began inside a code point, the part of
/// it that is there would be shown as replacement characters that are not in the log.
fn without_split_start(bytes: &[u8]) -> &[u8] {
    let split = bytes
        .iter()
        .take(3)
        .take_while(|b| (0x80..=0xbf).contains(*b))
        .count();
    &bytes[split..]
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
