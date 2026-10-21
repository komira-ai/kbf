//! The Leases and Queue pages of `kbf-ui` over this server's real `/v1/leases` and
//! `/v1/queue`, through a listener, with a daemon that holds leases and work that waits:
//! the documents the routes answer are the ones the pages parse (a key a page reads that
//! a route does not send would make the page `502`), a node id and an id of an operation
//! reach the page as text, the node filter and the paging follow the route's own
//! contract, and the pages read only their declared routes.

mod support;

use std::sync::Mutex;
use std::time::Duration;

use kbf_server::api_client::OperatorClient;
use kbf_ui::{CONTENT_SECURITY_POLICY, Fetched, Source, SourceError};
use serde_json::Value;
use support::{Cell, Job};
use tokio::time::{Instant, sleep};

/// The operator API as the pages' source, remembering each URL and its status.
struct Api<'a> {
    cell: &'a Cell,
    answered: Mutex<Vec<(String, u16)>>,
}

impl Source for Api<'_> {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        let (api, path) = (self.cell.api.expect("an operator API"), url.to_owned());
        let reply = tokio::task::spawn_blocking(move || OperatorClient::new(api, None).get(&path))
            .await
            .expect("the call runs")
            .map_err(|e| SourceError(e.to_string()))?;
        self.answered
            .lock()
            .expect("not poisoned")
            .push((url.to_owned(), reply.status));
        Ok(Fetched {
            status: reply.status,
            body: reply.body.into_bytes(),
            headers: Vec::new(),
        })
    }
}

impl<'a> Api<'a> {
    const fn new(cell: &'a Cell) -> Self {
        Self {
            cell,
            answered: Mutex::new(Vec::new()),
        }
    }

    /// The page at `path_and_query`: its status and its HTML; every answer carries the
    /// CSP.
    async fn page(&self, path_and_query: &str) -> (u16, String) {
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        let answer = kbf_ui::respond(self, path, query).await;
        assert!(
            answer
                .headers()
                .contains(&("content-security-policy", CONTENT_SECURITY_POLICY)),
            "{path_and_query}"
        );
        let html = String::from_utf8(answer.body().to_vec()).expect("UTF-8");
        (answer.status(), html)
    }

    /// The route's own JSON, read through the same source.
    async fn json(&self, url: &str) -> Value {
        let fetched = self.get(url).await.expect("a reply");
        assert_eq!(fetched.status, 200, "{url}");
        serde_json::from_slice(&fetched.body).expect("JSON")
    }

    /// Whether every URL a page read answered `200`.
    fn all_ok(&self) -> bool {
        self.answered
            .lock()
            .expect("not poisoned")
            .iter()
            .all(|(_, code)| *code == 200)
    }

    fn urls(&self) -> Vec<String> {
        self.answered
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|(url, _)| url.clone())
            .collect()
    }
}

/// The route's answer once `ready` holds of it (the scheduler's views follow its ticks).
async fn until(api: &Api<'_>, url: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let started = Instant::now();
    loop {
        let body = api.json(url).await;
        if ready(&body) {
            return body;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{url} never became ready: {body}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}

/// Table rows of a page's body, the header row excluded.
fn rows(html: &str) -> usize {
    html.matches("<tr>").count().saturating_sub(1)
}

/// The UI path and query of the page's "Next page" link, as the browser would follow it.
fn next_link(html: &str) -> Option<String> {
    let (before, _) = html.split_once("\">Next page</a>")?;
    let (_, href) = before.rsplit_once("<a href=\"")?;
    Some(href.replace("&amp;", "&"))
}

/// Catches: a lease key the page reads that the route does not send (the page would be
/// `502`); a lease missing from the page or shown twice across pages; the node filter
/// not reaching the route; the page's next link not leading to the rest; a node id
/// written as markup (the daemon's id is `<b>`-and-quote text); a page that reads a
/// route it does not declare. Mutants: the node not forwarded; the token dropped from
/// the next link; the node written raw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_leases_page_lists_what_a_daemon_holds_and_pages() {
    let cell = Cell::start_with_api().await;
    let api = Api::new(&cell);
    let jobs: Vec<Job> = (0..3)
        .map(|i| Job::new(&format!("lease page {i}"), &[]))
        .collect();
    for job in &jobs {
        cell.upload(&job.blobs()).await;
    }
    let mut streams = Vec::new();
    for job in &jobs {
        streams.push(cell.execute(&job.action).await);
    }
    let node = "n<b>\"&";
    let mut daemon = cell.daemon(node, 8, 16).await;
    for _ in &jobs {
        daemon.start().await;
    }
    let held = until(&api, "/v1/leases", |b| {
        b["items"].as_array().unwrap().len() == 3
    })
    .await;
    let ids: Vec<&str> = held["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lease"].as_str().unwrap())
        .collect();

    let (status, html) = api.page("/ui/leases").await;
    assert_eq!(status, 200, "{html}");
    assert_eq!(rows(&html), 3, "{html}");
    for id in &ids {
        assert!(
            html.contains(&format!("<code>{id}</code>")),
            "{id} in {html}"
        );
    }
    assert!(html.contains("n&lt;b&gt;&quot;&amp;"), "{html}");
    assert!(!html.contains("n<b>"), "{html}");
    assert!(html.contains("<td>ci</td>"), "{html}");
    assert!(html.contains("<td>any</td>"), "{html}");
    assert!(
        html.contains("class=\"pill ok\">running<")
            || html.contains("class=\"pill pending\">granted<"),
        "{html}"
    );
    assert!(!html.contains("Next page"), "{html}");

    // The filter, typed as a browser's form submits it; an unknown node holds none.
    let mine = "/ui/leases?node=n%3Cb%3E%22%26";
    let (status, html) = api.page(mine).await;
    assert_eq!((status, rows(&html)), (200, 3), "{html}");
    let (status, html) = api.page("/ui/leases?node=node-z").await;
    assert_eq!(status, 200, "{html}");
    assert_eq!(rows(&html), 0, "{html}");
    assert!(
        html.contains("That node holds no lease the scheduler knows of."),
        "{html}"
    );

    // Page by two, then follow the page's own link to the last lease.
    let (status, first) = api.page("/ui/leases?page_size=2").await;
    assert_eq!((status, rows(&first)), (200, 2), "{first}");
    let next = next_link(&first).expect("a next page link");
    assert!(
        next.starts_with("/ui/leases?"),
        "on this page's path: {next}"
    );
    assert!(next.contains("page_token="), "{next}");
    let (status, second) = api.page(&next).await;
    assert_eq!((status, rows(&second)), (200, 1), "{second}");
    assert!(!second.contains("Next page"), "{second}");
    let seen: usize = ids
        .iter()
        .filter(|id| {
            let tag = format!("<code>{id}</code>");
            first.contains(&tag) != second.contains(&tag)
        })
        .count();
    assert_eq!(seen, 3, "every lease on exactly one page");

    // A token with another filter is the route's `400`, shown as the page's own.
    let token = next.split("page_token=").nth(1).expect("a token");
    let (status, html) = api
        .page(&format!("/ui/leases?node=other&page_token={token}"))
        .await;
    assert_eq!(status, 400, "{html}");
    assert!(
        html.contains("page_token is not one this route made"),
        "{html}"
    );

    assert!(
        api.urls()
            .iter()
            .all(|u| u == "/v1/health" || u == "/v1/leases" || u.starts_with("/v1/leases?")),
        "{:?}",
        api.urls()
    );
    drop(streams);
    drop(daemon);
}

/// Catches: a queue key the page reads that the route does not send (`502`); work no
/// node can run missing from the page, or without its reason and count; a quiet farm
/// shown as anything but empty; the page's own query reaching a route that takes none.
/// Mutants: the reason not written; the oldest age shown for an empty queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_page_shows_work_no_node_can_run() {
    let cell = Cell::start_with_api().await;
    let api = Api::new(&cell);
    let (status, html) = api.page("/ui/queue").await;
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<dt>Operations queued</dt><dd>0</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<dt>Oldest waiting</dt><dd>nothing waits</dd>"),
        "{html}"
    );
    assert!(html.contains("Nothing is queued."), "{html}");

    let job = Job::new("waits for a mac", &[("os", "macos")]);
    cell.upload(&job.blobs()).await;
    let _stream = cell.execute(&job.action).await;
    let _daemon = cell.daemon("node-a", 4, 8).await;
    until(&api, "/v1/queue", |b| {
        b["depth"] == 1 && !b["waiting"].as_array().unwrap().is_empty()
    })
    .await;

    let (status, html) = api.page("/ui/queue?node=ignored").await;
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<dt>Operations queued</dt><dd>1</dd>"),
        "{html}"
    );
    assert!(!html.contains("nothing waits"), "{html}");
    assert!(
        html.contains("<td>os=macos</td><td>ci</td><td>1</td>"),
        "{html}"
    );
    assert!(html.contains("os=macos"), "{html}");
    assert!(html.contains("lacks"), "{html}");
    assert!(html.contains("<code>op-"), "{html}");
    assert!(api.all_ok());
    // The last read is the banner's start time, from the real `/v1/health`, which the page
    // parsed (a key it names that the route does not send would say it is not known).
    assert_eq!(
        api.urls().iter().rev().map(String::as_str).nth(1),
        Some("/v1/queue")
    );
    assert_eq!(api.urls().last().map(String::as_str), Some("/v1/health"));
    assert!(html.contains("This server started at 20"), "{html}");
    assert!(!html.contains("is not known"), "{html}");
}
