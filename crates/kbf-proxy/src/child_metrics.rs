//! The active child's operator `/metrics`, fetched for the proxy's own `GET /metrics` to
//! append (design note decision C11).
//!
//! The fetch is one bound stream like any operator call: it takes a permit on the active
//! slot (so a retire waits for it), goes through the same pool, and carries the caller's
//! `Authorization` and the proxy's identity headers. It never waits: while a hold is in
//! force, with no active slot, or on any failure (a refused dial, a status other than 200, a
//! body over [`MAX_BYTES`], no complete answer within [`DEADLINE`]) it gives nothing, the
//! proxy's own series are served alone, and the failure is counted in
//! `kbf_proxy_child_metrics_failed_total`.
//!
//! The body is filtered before it is appended ([`filter`]): the proxy is the only author of
//! `kbf_proxy_*`, so a child's sample, `# HELP`, `# TYPE` or `# UNIT` line for such a name is
//! dropped (it would duplicate or contradict the proxy's own), and so is a `# EOF` line (it
//! would end the scrape before the proxy's series). A body that is not UTF-8 is not text to
//! append: nothing is appended and it counts as a failure.

use std::time::Duration;

use http::header::AUTHORIZATION;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Version};

use crate::core::Core;
use crate::identity::forward_headers;
use crate::metrics::Metrics;
use crate::route::Target;
use crate::slot::Bind;

/// The largest child body appended.
pub(crate) const MAX_BYTES: usize = 4 << 20;
/// The whole fetch, connect to last byte.
pub(crate) const DEADLINE: Duration = Duration::from_secs(2);

/// The prefix of every series the proxy owns.
const OWN_PREFIX: &str = "kbf_proxy_";

/// The child's `/metrics` body without the proxy's series and `# EOF`, or nothing (counted
/// as a failure).
pub(crate) async fn fetch(core: &Core, authorization: Option<HeaderValue>) -> Option<String> {
    let body = tokio::time::timeout(DEADLINE, ask(core, authorization))
        .await
        .ok()
        .flatten()
        .and_then(|bytes| filter(bytes).ok());
    if body.is_none() {
        Metrics::inc(&core.metrics.child_metrics_failed);
    }
    body
}

/// `body` as lines, each ending in a newline, minus the lines the child may not speak for;
/// `Err` when it is not UTF-8.
fn filter(body: Vec<u8>) -> Result<String, std::string::FromUtf8Error> {
    let body = String::from_utf8(body)?;
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        if line.trim() == "# EOF" || names_own_series(line) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Whether `line` is a sample, or a `# HELP`/`# TYPE`/`# UNIT` line, for a `kbf_proxy_` name.
fn names_own_series(line: &str) -> bool {
    let line = line.trim_start();
    let name = match line.strip_prefix('#') {
        Some(comment) => {
            let comment = comment.trim_start();
            ["HELP", "TYPE", "UNIT"]
                .iter()
                .find_map(|word| comment.strip_prefix(word))
                .filter(|rest| rest.starts_with(char::is_whitespace))
                .map_or("", str::trim_start)
        }
        None => line,
    };
    name.starts_with(OWN_PREFIX)
}

async fn ask(core: &Core, authorization: Option<HeaderValue>) -> Option<Vec<u8>> {
    let Bind::Bound(permit) = core.slots.bind() else {
        return None;
    };
    let pool = core
        .pool
        .target(permit.slot(), permit.generation(), Target::Operator);
    let (_reservation, mut send) = crate::pool::TargetPool::acquire(&pool).await.ok()?;
    let mut headers = HeaderMap::new();
    // The edge refuses a /metrics call without the operator token, so a header is always
    // present here; `extend` forwards it verbatim without a branch for an absent one.
    headers.extend(authorization.map(|value| (AUTHORIZATION, value)));
    forward_headers(&mut headers, None, &core.security.secret).ok()?;
    let mut request = Request::new(());
    *request.method_mut() = Method::GET;
    *request.uri_mut() = http::Uri::from_static("http://localhost/metrics");
    *request.headers_mut() = headers;
    *request.version_mut() = Version::HTTP_2;
    let (response, _) = send.send_request(request, true).ok()?;
    let response = response.await.ok()?;
    if response.status() != StatusCode::OK {
        return None;
    }
    let mut body = response.into_body();
    let mut out = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.ok()?;
        // Credit is returned before the cap is judged: the stream is dropped either way.
        let _ = body.flow_control().release_capacity(chunk.len());
        if out.len() + chunk.len() > MAX_BYTES {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    drop(permit);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{filter, names_own_series};

    #[test]
    fn own_series_are_samples_and_metadata_lines_with_the_prefix() {
        for line in [
            "kbf_proxy_x 1",
            "  kbf_proxy_x{a=\"b\"} 1",
            "# HELP kbf_proxy_x text",
            "# TYPE kbf_proxy_x counter",
            "#  UNIT kbf_proxy_x seconds",
        ] {
            assert!(names_own_series(line), "{line}");
        }
        for line in [
            "kbf_child_x 1",
            "kbf_proxyx 1",
            "# HELP kbf_child_x kbf_proxy_x",
            "# kbf_proxy_x a plain comment",
            "# HELPkbf_proxy_x 1",
            "#",
            "",
        ] {
            assert!(!names_own_series(line), "{line}");
        }
    }

    #[test]
    fn filter_ends_every_kept_line_with_a_newline_and_drops_eof() {
        let kept = filter(b"a 1\nkbf_proxy_b 2\r\n  # EOF  \nc 3".to_vec()).expect("utf-8");
        assert_eq!(kept, "a 1\nc 3\n");
        assert_eq!(filter(Vec::new()).expect("utf-8"), "");
        assert!(filter(vec![0xff]).is_err());
    }
}
