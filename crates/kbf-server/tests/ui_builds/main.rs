//! The Builds, Build and Action pages of `/ui` over this server's real routes, through the
//! mount: the pages read the in-process `/v1` routes, so a key a page reads that a route
//! does not answer (or answers in another form) is a `502` here. Each test names the defect
//! it catches.

#[path = "../pages/mod.rs"]
mod pages;
#[path = "../history_read/world.rs"]
mod world;

use kbf_invocation::{Event, InvocationId, Outcome, Owner};
use kbf_proto::reapi::{Action, Command, Digest as WireDigest, command};
use kbf_types::Digest;
use pages::{
    OPEN, PAGES_ONLY, Server, at, digest, finished, granted, listing, queued, started, text,
};
use prost::Message;
use world::{World, ran};

fn wire(d: &Digest) -> WireDigest {
    WireDigest {
        hash: d.hash_hex(),
        size_bytes: i64::try_from(d.size_bytes).expect("small"),
    }
}

/// The page at `path`: status and HTML.
async fn page(server: &Server, path: &str) -> (u16, String) {
    let answer = server.get(path).await;
    (answer.code, answer.text())
}

/// Catches: the Builds list reading a key the history route does not answer (a `502`),
/// the list not reaching the route with its state filter (an open build listed under
/// `closed`, or the reverse), a failed build not marked failed in words, a build not
/// linked to its page, and a hostile build id written as markup. Two builds on a server
/// with history: one clean, one with a failing action; both still open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builds_list_is_the_history_route_with_its_filters() {
    let world = World::new();
    let server = world.start(PAGES_ONLY);
    let mut events = ran("clean", "main", 1, 1, 0);
    events.extend(ran("bad", "main", 2, 2, 1));
    events.extend(ran("<i>x</i>", "main", 3, 3, 0));
    server.feed(events).await;
    for id in ["clean", "bad", "<i>x</i>"] {
        world.flushed(id, 1).await;
    }

    let (status, html) = page(&server, "/ui/builds").await;
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<a href=\"/ui/builds/clean\">clean</a>"),
        "{html}"
    );
    assert!(
        html.contains("<a href=\"/ui/builds/bad\">bad</a>"),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"pill fail\">1 failed</span>"),
        "{html}"
    );
    assert!(
        html.contains("&lt;i&gt;x&lt;/i&gt;") && !html.contains("<i>"),
        "{html}"
    );
    assert!(
        html.contains(
            "<link rel=\"alternate\" type=\"application/json\" href=\"/v1/invocations\">"
        )
    );

    let (status, html) = page(&server, "/ui/builds?state=open").await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains(">bad</a>"), "{html}");
    let (status, html) = page(&server, "/ui/builds?state=closed").await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("No builds match."), "{html}");
    let (status, html) = page(&server, "/ui/builds?state=bogus").await;
    assert_eq!(status, 400, "{html}");
    server.stop().await;
}

/// Catches: the Builds list on a server with no history: it must say why it is empty
/// (the route's own `404`), not show an empty list as a quiet farm.
#[tokio::test]
async fn without_history_the_builds_list_says_the_route_is_off() {
    let server = Server::with(OPEN);
    let (status, html) = page(&server, "/ui/builds").await;
    assert_eq!(status, 404, "{html}");
    assert!(html.contains("problem"), "{html}");
    assert!(!html.contains("No builds match."), "{html}");
    server.stop().await;
}

fn queued_as(target: &str, when: u64, action: Digest, op: u64) -> Event {
    let Event::Call(mut c) = queued("b", "main", at(when), action, op) else {
        unreachable!("queued is a call")
    };
    c.metadata.target = Some(kbf_invocation::Text::sent(target));
    Event::Call(c)
}

/// Catches: the failed action missing from the Build page or not linked to its Action
/// page; the Action page without its command (arguments, environment names), its failure
/// and exit code, or its stderr tail; the arguments, a name or the log written as markup
/// (all are hostile here); an environment VALUE on the page (the Command blob holds one,
/// and the API sends names only); the download link not downloading (the route's answer
/// to it must be an attachment: the page's link drops `inline=text`). One build, one
/// failed action, the Command and Action blobs and the log in the cache.
#[tokio::test]
async fn a_failed_action_shows_its_command_and_tail_and_links_the_download() {
    let server = Server::with(OPEN);
    let argument = "-DX=\"<script>alert(1)</script>\" & more";
    let command = Command {
        arguments: ["cc", argument, "-o", "out/a.o"].map(str::to_owned).into(),
        environment_variables: vec![command::EnvironmentVariable {
            name: "<ENV>".to_owned(),
            value: "SECRET-VALUE-9e1".to_owned(),
        }],
        working_directory: "src".to_owned(),
        output_paths: vec!["out/a.o".to_owned()],
        ..Command::default()
    };
    let command = server.store(&command.encode_to_vec()).await;
    let action = Action {
        command_digest: Some(wire(&command)),
        ..Action::default()
    };
    let action = server.store(&action.encode_to_vec()).await;
    let stderr = server.store(b"line 1\n<script>boom()</script>\n").await;
    let result = digest(900);
    server
        .feed([
            queued_as("//app:<b>lib</b>", 0, action, 1),
            Event::Command {
                id: InvocationId::new("b"),
                owner: Owner::default(),
                action,
                command,
                shape: None,
            },
            granted(1, 1, "node-a", at(10)),
            started(1, 1, at(11)),
            finished(
                1,
                1,
                Outcome::Ok { exit_code: 3 },
                Some(listing(result, 3, &[], (None, Some(stderr)))),
                at(40),
            ),
        ])
        .await;

    let (status, build) = page(&server, "/ui/builds/b").await;
    assert_eq!(status, 200, "{build}");
    let link = format!("/ui/builds/b/actions/{}", text(&action));
    assert!(
        build.contains(&format!(
            "<a href=\"{link}\">//app:&lt;b&gt;lib&lt;/b&gt;</a>"
        )),
        "{build}"
    );
    assert!(
        build.contains("<span class=\"pill fail\">exit 3</span>"),
        "{build}"
    );

    let (status, html) = page(&server, &link).await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("<dt>Exit code</dt><dd>3</dd>"), "{html}");
    assert!(html.contains("<li><code>cc</code></li>"), "{html}");
    let escaped = argument
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    assert!(
        html.contains(&format!("<li><code>{escaped}</code></li>")),
        "{html}"
    );
    assert!(html.contains("<li><code>&lt;ENV&gt;</code></li>"), "{html}");
    assert!(
        html.contains("<dt>Working directory</dt><dd>src</dd>"),
        "{html}"
    );
    assert!(
        html.contains("<pre>line 1\n&lt;script&gt;boom()&lt;/script&gt;\n</pre>"),
        "{html}"
    );
    assert!(
        !html.contains("SECRET-VALUE-9e1"),
        "an environment value: {html}"
    );
    for page in [&build, &html] {
        assert!(!page.contains("<script"), "{page}");
        assert!(!page.contains("<b>"), "{page}");
    }

    // The link the page gives for the whole log downloads it.
    let href = format!("/v1/blobs/{}?instance=main&name=stderr", text(&stderr));
    assert!(
        html.contains(&format!(
            "<a href=\"{}\">Download the whole log</a>",
            href.replace('&', "&amp;")
        )),
        "{html}"
    );
    let download = server.get(&href).await;
    assert_eq!(download.code, 200);
    assert!(
        download
            .header("content-disposition")
            .is_some_and(|d| d.starts_with("attachment")),
        "{}",
        download.head
    );
    server.stop().await;
}
