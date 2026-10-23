//! What a crafted `/ui` path can make the pages read, against the real server (no proxy in
//! front): `/ui` is open, so the path and query a stranger sends must never become a read
//! of a route the page does not declare, nor a method other than `GET`
//! (`docs/design/kbf-ui-v1.md`, "every page reads only declared routes").
//!
//! The pages build each `/v1` URL from the path they were asked for, so the audit is of
//! that construction: a path segment is decoded once, written into the API URL as one
//! percent-encoded segment (so `%2F`, `..%2F`, a backslash, a question mark or a doubled
//! encoding stay inside the `{id}` or `{digest}` they fill), and a segment that is a dot
//! segment names no page.

mod pages;

use std::sync::Mutex;

use kbf_ui::{Fetched, Source, SourceError};
use pages::{OPEN, Server};

/// The operator API as the pages' source, remembering each URL and its status.
struct Api<'a> {
    server: &'a Server,
    answered: Mutex<Vec<(String, u16)>>,
}

impl Source for Api<'_> {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        let answer = self.server.get(url).await;
        self.answered
            .lock()
            .expect("not poisoned")
            .push((url.to_owned(), answer.code));
        Ok(Fetched {
            status: answer.code,
            body: answer.body,
            headers: Vec::new(),
        })
    }
}

/// Path segments of `/ui` that try to walk to, or smuggle, another route.
const HOSTILE_SEGMENTS: [&str; 12] = [
    "..%2F..%2Fv1%2Fnodes",
    "%2e%2e%2f%2e%2e%2fv1%2Fnodes",
    "%252e%252e",
    "%252e%252e%252fv1",
    "a%5C..%5Cnodes",
    "a%3Fb%23c",
    "x%2Fy",
    "%2F",
    "..%2F",
    "a%20b%0D%0AHost%3A%20evil",
    "%E2%80%AEnodes",
    "v1%2Fnodes%3Fx%3D1",
];

/// Whether `url` is one of the three routes of the build pages with its parameters each
/// one non-empty path segment: no `/`, backslash, `?`, `#`, whitespace or control
/// character in it, and not a dot segment in any spelling. The only query allowed is a
/// `page_token`.
fn is_declared_shape(url: &str) -> bool {
    let (path, query) = url
        .split_once('?')
        .map_or((url, None), |(p, q)| (p, Some(q)));
    if query.is_some_and(|q| !q.starts_with("page_token=")) {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    let params_ok = |params: &[&str]| {
        params.iter().all(|s| {
            let dots = s.to_ascii_lowercase().replace("%2e", ".");
            !s.is_empty()
                && dots != "."
                && dots != ".."
                && !s.chars().any(|c| {
                    matches!(c, '\\' | '?' | '#' | '/') || c.is_whitespace() || c.is_control()
                })
        })
    };
    match segments[..] {
        ["", "v1", "invocations", id] => params_ok(&[id]),
        ["", "v1", "invocations", id, "actions"] => params_ok(&[id]),
        ["", "v1", "invocations", id, "actions", digest] => params_ok(&[id, digest]),
        _ => false,
    }
}

/// Every `/v1/...` link the markup carries as an `href` (a page links each URL it read to
/// its JSON), up to the closing quote. A hostile id echoed as text (the lookup field) is
/// escaped and is not one.
fn v1_mentions(html: &str) -> Vec<&str> {
    html.match_indices("href=\"/v1/")
        .map(|(start, key)| {
            let rest = &html[start + key.len() - "/v1/".len()..];
            let end = rest.find('"').unwrap_or(rest.len());
            &rest[..end]
        })
        .collect()
}

/// Catches: a page that writes the id or the digest it was asked for into a `/v1` URL
/// without encoding it (a `segment` that encodes nothing reads
/// `/v1/invocations/../../v1/nodes`, which a URL client resolves to `/v1/nodes`), or one
/// that does not refuse a dot segment before it is written into one: whatever the path or
/// the query, every URL the page asks the real routes for is one of its declared routes
/// with one segment per parameter, and a build that does not exist answers no page with
/// another route's document.
#[tokio::test]
async fn a_crafted_path_or_query_reads_only_the_declared_routes() {
    let server = Server::with(OPEN);
    let api = Api {
        server: &server,
        answered: Mutex::new(Vec::new()),
    };
    let mut cases: Vec<(String, String)> = Vec::new();
    for seg in HOSTILE_SEGMENTS {
        cases.push((format!("/ui/builds/{seg}"), String::new()));
        cases.push((format!("/ui/builds/b1/actions/{seg}"), String::new()));
        cases.push((format!("/ui/builds/{seg}/actions/{seg}"), String::new()));
        cases.push(("/ui/builds".to_owned(), format!("id={seg}")));
        cases.push(("/ui/builds/b1".to_owned(), format!("page_token={seg}")));
    }
    for query in [
        "id=..",
        "id=%2e%2e",
        "id=.",
        "id=%2E",
        "id=..%2F..%2Fv1%2Fnodes",
    ] {
        cases.push(("/ui/builds".to_owned(), query.to_owned()));
    }
    let mut reads = 0;
    for (path, query) in &cases {
        let answer = kbf_ui::respond(&api, path, query).await;
        assert!(
            matches!(answer.status(), 400 | 404),
            "{path}?{query}: {}",
            answer.status()
        );
        let html = String::from_utf8_lossy(answer.body()).into_owned();
        for url in v1_mentions(&html) {
            assert!(
                is_declared_shape(url),
                "{path}?{query}: the page names {url}"
            );
        }
        let answered = std::mem::take(&mut *api.answered.lock().expect("not poisoned"));
        for (url, code) in answered {
            reads += 1;
            assert!(is_declared_shape(&url), "{path}?{query}: read {url}");
            assert_ne!(code, 200, "{path}?{query}: {url} answered a document");
        }
    }
    assert!(reads > 50, "the cases reached the routes: {reads} reads");
    server.stop().await;
}

/// Catches: the mounted `/ui` answering a crafted raw path from a route it does not
/// declare, or any method but `GET` and `HEAD` (defects: `post` for `get` on the page
/// route; a page that reads the decoded path whole). The paths are sent raw over TCP to
/// the real server, as a client that does not normalize them would.
#[tokio::test]
async fn the_mounted_ui_answers_a_crafted_raw_path_with_its_own_404_and_refuses_writes() {
    let server = Server::with(OPEN);
    let nodes = server.get("/v1/nodes").await;
    assert_eq!(nodes.code, 200, "{}", nodes.head);
    let nodes_text = nodes.text();
    for seg in HOSTILE_SEGMENTS {
        for path in [
            format!("/ui/builds/{seg}"),
            format!("/ui/builds/b1/actions/{seg}"),
            format!("/ui/{seg}"),
            format!("/ui/builds/{seg}/actions/{seg}"),
        ] {
            let answer = server.get(&path).await;
            assert!(
                matches!(answer.code, 400 | 404),
                "{path}: {} {}",
                answer.code,
                answer.head
            );
            let html = answer.text();
            assert!(
                !html.contains(&nodes_text),
                "{path}: another route's document"
            );
            for url in v1_mentions(&html) {
                assert!(is_declared_shape(url), "{path}: the page names {url}");
            }
        }
    }
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        for path in [
            "/ui",
            "/ui/builds",
            "/ui/builds/b1",
            "/ui/builds/b1/actions/d",
        ] {
            let answer = server.request(method, path, "").await;
            assert_eq!(answer.code, 405, "{method} {path}: {}", answer.head);
        }
    }
    server.stop().await;
}
