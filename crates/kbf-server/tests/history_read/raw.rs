//! The raw routes (`GET /v1/blobs/{digest}` and `GET /v1/runs/{id}/logs/{log}`) never
//! serve bytes a browser could run (`docs/design/kbf-ui-v1.md` 5.3, R2b): what a build
//! printed is untrusted, and these routes sit on the origin of the web UI.

use crate::flog::{copies, failed_run, run_id, with_tails};
use crate::pages::{Answer, OPEN};
use crate::world::World;

/// A log that is a web page, the worst a build can print.
const HTML: &[u8] = b"<!doctype html><title>x</title><script>alert(document.cookie)</script>";

/// Asserts the answer is raw and inert: octet-stream (or text/plain where text was asked
/// for), never a type a browser renders, `nosniff`, a sandbox policy, and (for the bytes
/// of a log or blob) an attachment.
fn inert(answer: &Answer, what: &str, content_type: &str, disposition: &str) {
    assert_eq!(
        answer.header("content-type"),
        Some(content_type),
        "{what}: {}",
        answer.head
    );
    assert!(
        !answer.head.to_ascii_lowercase().contains("text/html"),
        "{what}: {}",
        answer.head
    );
    let given = answer.header("content-disposition").unwrap_or_default();
    assert!(
        given == disposition || given.starts_with(&format!("{disposition};")),
        "{what}: {given}"
    );
    assert_eq!(
        answer.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(
        answer.header("content-security-policy"),
        Some("sandbox"),
        "{what}"
    );
}

/// Catches (defects: `attachment` dropped from the blob route or from the log route; the
/// content type of either set from the bytes; `nosniff` or the sandbox policy missing
/// from either, or only from its errors; `inline=text` served as `text/html` or as an
/// attachment of unknown type): an HTML file a build printed, read through the blob route,
/// through `inline=text`, through the failure-log route whole and by range, and the
/// errors of both, is never anything a browser opens as a page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_routes_serve_attachment_text_plain() {
    let world = World::new();
    let server = world.start(OPEN);
    let (stdout, stderr) = (
        server.store(HTML).await,
        server.store(b"error: boom\n").await,
    );
    let result = server.store(b"an ActionResult").await;
    server
        .feed(failed_run(
            "build",
            "main",
            1,
            (Some(stdout), Some(stderr)),
            result,
        ))
        .await;
    copies(&world, 1).await;
    world.flushed("build", 1).await;
    let id = run_id(&server, "build");
    let page = with_tails(&server, &id).await;

    // The blob route, through the link the run page gives.
    let href = page["logs"]["stdout"]["href"].as_str().expect("a link");
    assert!(href.starts_with("/v1/blobs/"), "{href}");
    let blob = server.get(href).await;
    assert_eq!(blob.code, 200, "{}", blob.head);
    inert(&blob, "blob", "application/octet-stream", "attachment");
    assert_eq!(blob.body, HTML, "the bytes are the build's, untouched");

    // Text on request is text, and still inert.
    let joiner = if href.contains('?') { '&' } else { '?' };
    let text = server.get(&format!("{href}{joiner}inline=text")).await;
    assert_eq!(text.code, 200, "{}", text.head);
    inert(&text, "inline=text", "text/plain; charset=utf-8", "inline");
    assert_eq!(text.body, HTML);

    // The failure-log route, whole and by range.
    let log = format!("/v1/runs/{id}/logs/stdout");
    let whole = server.get(&log).await;
    assert_eq!(whole.code, 200, "{}", whole.head);
    inert(&whole, "log", "application/octet-stream", "attachment");
    assert_eq!(whole.body, HTML);
    let part = server.get_with(&log, "Range: bytes=0-8\r\n").await;
    assert_eq!(part.code, 206, "{}", part.head);
    inert(&part, "log range", "application/octet-stream", "attachment");
    assert_eq!(part.body, &HTML[..9]);

    // Errors are never a page either.
    for (path, want) in [
        (format!("/v1/runs/{id}/logs/nothing"), 404),
        (
            "/v1/runs/000000000000000000000000/logs/stdout".to_owned(),
            404,
        ),
        ("/v1/blobs/nonsense".to_owned(), 400),
    ] {
        let answer = server.get(&path).await;
        assert_eq!(answer.code, want, "{path}: {}", answer.head);
        assert_eq!(
            answer.header("x-content-type-options"),
            Some("nosniff"),
            "{path}"
        );
        assert_eq!(
            answer.header("content-security-policy"),
            Some("sandbox"),
            "{path}"
        );
        assert!(
            !answer.head.to_ascii_lowercase().contains("text/html"),
            "{path}"
        );
    }
    server.stop().await;
}
