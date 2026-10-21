//! The Targets and Target pages of `kbf-ui` over this server's real history routes,
//! after a restart: the documents `/v1/tests`, `/v1/shapes/{key}` and `/v1/grid` answer
//! are the ones the pages parse, so a key a page reads that a route does not send (or
//! sends in another form) is a `502` page here. The source asks the operator API over
//! TCP as a browser's request reaches it through the mount.

use std::sync::Mutex;

use kbf_ui::{CONTENT_SECURITY_POLICY, Fetched, Source, SourceError};

use crate::kit::{indexed, run, shape};
use crate::pages::{PAGES_ONLY, Server};
use crate::world::World;

const KEY: u64 = 0x1111_1111_1111_1111;
const STEADY: u64 = 0x2222_2222_2222_2222;
const LABEL: &str = "komira//t:<script>alert(1)</script>";

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

/// The text of the page at `path` and `query`, which must answer `200` with the CSP.
async fn html(api: &Api<'_>, path: &str, query: &str) -> String {
    let answer = kbf_ui::respond(api, path, query).await;
    let html = String::from_utf8(answer.body().to_vec()).expect("UTF-8");
    assert_eq!(answer.status(), 200, "{path}: {html}");
    assert!(
        answer
            .headers()
            .contains(&("content-security-policy", CONTENT_SECURITY_POLICY)),
        "{path}"
    );
    html
}

/// The class of each cell of the first strip of `html`, in order.
fn classes(html: &str) -> Vec<&str> {
    html.split("class=\"cell ")
        .skip(1)
        .map(|c| c.split('"').next().unwrap_or_default())
        .collect()
}

/// Catches (P8, ui-data 6.2 with kbf-ui over it): a page that cannot read what the routes
/// answer; a strip not in build order; a flaky test (the same test passing and failing
/// across builds) not told by its cells; the label of a test written as markup; a build
/// the route names that the page links wrongly. Four builds run one test: pass, fail,
/// pass, fail, oldest first, with a restart before the reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_target_strip_over_the_real_routes_shows_a_flaky_test() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let outcomes = [("t1", 0), ("t2", 2), ("t3", 0), ("t4", 2)];
    for (n, (build, exit)) in (1u8..).zip(outcomes) {
        let mut ran = run(
            build,
            "main",
            shape(KEY, "cfgA", true, Some(LABEL)),
            n,
            exit,
        );
        // A second test, which only the oldest build ran: the list asks the grid for
        // both keys at once.
        if build == "t1" {
            ran.extend(run(build, "main", shape(STEADY, "cfgA", true, None), 5, 0));
        }
        a.feed(ran).await;
        world
            .flushed(build, if build == "t1" { 2 } else { 1 })
            .await;
    }
    indexed(&world, 2, 2).await;
    a.stop().await;

    let b = world.start(PAGES_ONLY);
    let api = Api {
        server: &b,
        answered: Mutex::new(Vec::new()),
    };
    let key = format!("{KEY:016x}");
    let list = html(&api, "/ui/targets", "").await;
    // The flaky test's row, then the other's: passed in the oldest build only.
    assert_eq!(
        classes(&list),
        [
            "ok", "fail", "ok", "fail", "ok", "pending", "pending", "pending"
        ],
        "{list}"
    );
    assert!(
        list.contains("komira//t:&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{list}"
    );
    assert!(
        list.contains(&format!(
            "<a href=\"/ui/targets/{key}?instance=main&amp;cfg=cfgA\">"
        )),
        "{list}"
    );

    let one = html(
        &api,
        &format!("/ui/targets/{key}"),
        "instance=main&cfg=cfgA",
    )
    .await;
    assert_eq!(classes(&one), ["ok", "fail", "ok", "fail"], "{one}");
    assert!(
        one.contains("2 failed of 4 builds that ran it (2 passed) in the last 4 builds."),
        "{one}"
    );
    // The table is newest first, and each build is a link to its page.
    let newest = one.find("<a href=\"/ui/builds/t4\">").expect("t4 listed");
    let oldest = one.find("<a href=\"/ui/builds/t1\">").expect("t1 listed");
    assert!(newest < oldest, "{one}");
    assert!(
        one.contains("<dt>Key</dt><dd>1111111111111111</dd>"),
        "{one}"
    );

    // The key alone lists its rows, each linked with its instance and configuration.
    let rows = html(&api, &format!("/ui/targets/{key}"), "").await;
    assert!(rows.contains("main, cfgA"), "{rows}");

    for html in [&list, &one, &rows] {
        assert!(!html.contains("<script"), "{html}");
    }
    let answered = api.answered.lock().expect("not poisoned").clone();
    assert_eq!(answered.len(), 5, "{answered:?}");
    assert!(
        answered[1]
            .0
            .starts_with("/v1/grid?shapes=1111111111111111%2C2222222222222222&"),
        "{answered:?}"
    );
    for (url, code) in &answered {
        assert_eq!(*code, 200, "{url}");
    }
    b.stop().await;
}
