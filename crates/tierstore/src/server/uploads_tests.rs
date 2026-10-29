//! ListMultipartUploads through `Server::handle`, over a proxy whose tiers are in memory.

use hyper::StatusCode;

use crate::{Backend, Op};

use super::ops_tests::{completion, start, upload};
use super::testing::{BUCKET, NOW, Reply, Rig, texts};

/// Lists the open uploads of the bucket (`query` is what follows `uploads&`).
async fn list(rig: &Rig, query: &str) -> Reply {
    let uri = format!("/{BUCKET}?uploads&{query}");
    rig.send("GET", &uri, &[], b"").await
}

/// The key and the upload id of each upload of a listing, in the order it gives them.
fn uploads(reply: &Reply) -> Vec<(String, String)> {
    let keys = texts(&reply.body, "Key");
    let ids = texts(&reply.body, "UploadId");
    keys.into_iter().zip(ids).collect()
}

/// Asserts that uploads started under two prefixes are listed by prefix: a prefix gives the
/// uploads under it and no others, the empty prefix gives all, each upload carries its key, its
/// id and the time the cold tier started it, and a prefix with no upload gives an empty,
/// complete listing. It catches a listing that ignores `prefix`, drops the id, or reads the
/// wrong time.
#[tokio::test(start_paused = true)]
async fn uploads_started_under_two_prefixes_are_listed_by_prefix() {
    let rig = Rig::new(1 << 20).await;
    let one = start(&rig, "a/one").await;
    let two = start(&rig, "a/two").await;
    let other = start(&rig, "b/one").await;

    let all = list(&rig, "").await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(all.header("content-type"), "application/xml");
    let want = [("a/one", &one), ("a/two", &two), ("b/one", &other)];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(key, id)| ((*key).to_owned(), (*id).clone()))
        .collect();
    assert_eq!(uploads(&all), want);
    assert_eq!(texts(&all.body, "Bucket"), [BUCKET]);
    assert_eq!(texts(&all.body, "MaxUploads"), ["1000"]);
    assert_eq!(texts(&all.body, "IsTruncated"), ["false"]);
    assert!(texts(&all.body, "NextKeyMarker").is_empty());
    // The clock of the rig stands at `NOW`, 2027-01-15 08:00:00 UTC.
    assert_eq!(NOW, 1_800_000_000_000);
    let started = vec!["2027-01-15T08:00:00.000Z"; 3];
    assert_eq!(texts(&all.body, "Initiated"), started);

    let under_a = list(&rig, "prefix=a%2F").await;
    assert_eq!(uploads(&under_a), want[..2]);
    assert_eq!(texts(&under_a.body, "Prefix"), ["a/"]);
    let under_b = list(&rig, "prefix=b%2F").await;
    assert_eq!(uploads(&under_b), want[2..]);
    let under_two = list(&rig, "prefix=a%2Ft").await;
    assert_eq!(uploads(&under_two), want[1..2]);
    let none = list(&rig, "prefix=c%2F").await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(uploads(&none).is_empty());
    assert_eq!(texts(&none.body, "IsTruncated"), ["false"]);
    assert_eq!(rig.hot.calls(Op::ListUploads), 0);
    assert_eq!(rig.cold.calls(Op::ListUploads), 5);
}

/// Asserts that a listing of uploads is paged by `max-uploads`, `key-marker` and
/// `upload-id-marker`: a page that is cut says so and names the markers of the next, following
/// them visits every upload once and in the order of the one listing, even where two uploads
/// have one key, a key marker alone skips all the uploads of that key, and a page that is not
/// cut says it is the last. It catches a page that repeats or drops the upload on the border of
/// two pages, markers that skip a second upload of the same key, and an `IsTruncated` that is
/// wrong at either end.
#[tokio::test(start_paused = true)]
async fn a_listing_of_uploads_is_paged_by_the_markers() {
    let rig = Rig::new(1 << 20).await;
    for key in ["p/a", "p/b", "p/b", "p/c", "p/d"] {
        start(&rig, key).await;
    }
    let whole = list(&rig, "").await;
    let all = uploads(&whole);
    assert_eq!(all.len(), 5);
    assert_eq!(all[1].0, all[2].0);

    let mut seen = Vec::new();
    let mut pages = Vec::new();
    let mut markers = String::new();
    loop {
        let page = list(&rig, &format!("max-uploads=2{markers}")).await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(texts(&page.body, "MaxUploads"), ["2"]);
        pages.push(uploads(&page).len());
        seen.extend(uploads(&page));
        if texts(&page.body, "IsTruncated") == ["false"] {
            break;
        }
        assert_eq!(texts(&page.body, "IsTruncated"), ["true"]);
        let (key, id) = seen.last().unwrap().clone();
        assert_eq!(texts(&page.body, "NextKeyMarker"), [key.as_str()]);
        assert_eq!(texts(&page.body, "NextUploadIdMarker"), [id.as_str()]);
        markers = format!(
            "&key-marker={}&upload-id-marker={id}",
            key.replace('/', "%2F")
        );
    }
    assert_eq!(pages, [2, 2, 1]);
    assert_eq!(seen, all);

    let (key, id) = &all[1];
    let marked = format!(
        "key-marker={}&upload-id-marker={id}",
        key.replace('/', "%2F")
    );
    let after_first_b = list(&rig, &marked).await;
    assert_eq!(texts(&after_first_b.body, "KeyMarker"), [key.as_str()]);
    assert_eq!(texts(&after_first_b.body, "UploadIdMarker"), [id.as_str()]);
    assert_eq!(uploads(&after_first_b), all[2..]);
    let key_only = list(&rig, "key-marker=p%2Fb").await;
    assert_eq!(uploads(&key_only), all[3..]);
    let id_only = list(&rig, &format!("upload-id-marker={id}")).await;
    assert_eq!(uploads(&id_only), all);
    let whole_page = list(&rig, "max-uploads=5").await;
    assert_eq!(texts(&whole_page.body, "IsTruncated"), ["false"]);
    assert_eq!(uploads(&whole_page), all);
    let capped = list(&rig, "max-uploads=5000").await;
    assert_eq!(texts(&capped.body, "MaxUploads"), ["1000"]);
}

/// Asserts that an upload that was completed or aborted is not listed, through the front's own
/// calls, while the upload left open is, and that the object a completion made is not an
/// upload. It catches a listing that keeps an upload after it ends.
#[tokio::test(start_paused = true)]
async fn a_completed_or_aborted_upload_is_not_listed() {
    let rig = Rig::new(1 << 20).await;
    let done = start(&rig, "k/done").await;
    let aborted = start(&rig, "k/aborted").await;
    let open = start(&rig, "k/open").await;
    assert_eq!(list(&rig, "").await.body.matches("<Upload>").count(), 3);

    let part = upload(&rig, "k/done", &done, 1, b"bytes").await;
    let body = completion(&[(1, part.header("etag"))]);
    let uri = format!("k/done?uploadId={done}");
    let completed = rig.call("POST", &uri, body.as_bytes()).await;
    assert_eq!(completed.status, StatusCode::OK);
    let uri = format!("k/aborted?uploadId={aborted}");
    let gone = rig.call("DELETE", &uri, b"").await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);

    let left = list(&rig, "").await;
    assert_eq!(uploads(&left), [("k/open".to_owned(), open)]);
    assert_eq!(rig.cold_mem.get("k/done", None).await.unwrap(), "bytes");
}

/// Asserts that an upload of a key under a reserved prefix is never listed, whatever the prefix
/// asked for, and that a page is still full when reserved uploads sort in front of the others:
/// the listing asks the cold tier again for what the filter took out. The uploads are made in the cold tier directly, since the front refuses to
/// start one. It catches a listing that leaks the proxy's own prefixes, and one whose page is
/// short, so that a client takes it for the last, when it has dropped a reserved upload.
#[tokio::test(start_paused = true)]
async fn uploads_under_reserved_prefixes_are_never_listed() {
    let rig = Rig::new(1 << 20).await;
    rig.cold_mem.create_upload("_journal/p1/0").await.unwrap();
    rig.cold_mem.create_upload("_journal/p1/1").await.unwrap();
    rig.cold_mem.create_upload("_tiering/lease").await.unwrap();
    let first = start(&rig, "a/1").await;
    let second = start(&rig, "a/2").await;
    let third = start(&rig, "a/3").await;

    let all = list(&rig, "").await;
    let want = [("a/1", &first), ("a/2", &second), ("a/3", &third)];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(key, id)| ((*key).to_owned(), (*id).clone()))
        .collect();
    assert_eq!(uploads(&all), want);
    for prefix in ["_journal%2F", "_tiering%2F", "_journal%2Fp1%2F0"] {
        let reserved = list(&rig, &format!("prefix={prefix}")).await;
        assert_eq!(reserved.status, StatusCode::OK, "{prefix}");
        assert!(uploads(&reserved).is_empty(), "{prefix}");
    }
    let before = rig.cold.calls(Op::ListUploads);
    let page = list(&rig, "max-uploads=2").await;
    assert_eq!(uploads(&page), want[..2]);
    assert_eq!(texts(&page.body, "IsTruncated"), ["true"]);
    assert_eq!(texts(&page.body, "NextKeyMarker"), ["a/2"]);
    assert!(rig.cold.calls(Op::ListUploads) > before + 1);
}

/// Asserts that a request the listing does not do is refused as it is for the other calls: a
/// `max-uploads` that is not a positive whole number is `InvalidArgument`, a `delimiter` and a
/// listing of the parts of one upload (`GET /key?uploadId=`) are `NotImplemented`, and a cold
/// tier that does not answer is `ServiceUnavailable`.
#[tokio::test(start_paused = true)]
async fn a_listing_of_uploads_refuses_what_it_does_not_do() {
    let rig = Rig::new(1 << 20).await;
    let id = start(&rig, "k").await;
    for bad in ["max-uploads=0", "max-uploads=-1", "max-uploads=many"] {
        let reply = list(&rig, bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(reply.code(), "InvalidArgument", "{bad}");
        assert!(reply.body.contains("max-uploads"), "{}", reply.body);
    }
    let delimited = list(&rig, "delimiter=%2F").await;
    assert_eq!(delimited.status, StatusCode::NOT_IMPLEMENTED);
    let parts = rig.call("GET", &format!("k?uploadId={id}"), b"").await;
    assert_eq!(parts.status, StatusCode::NOT_IMPLEMENTED);
    let on_key = rig.call("GET", "k?uploads", b"").await;
    assert_eq!(on_key.status, StatusCode::NOT_IMPLEMENTED);

    rig.cold.fail_next(Op::ListUploads, 1);
    let down = list(&rig, "").await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    let back = list(&rig, "").await;
    assert_eq!(back.status, StatusCode::OK);
    assert_eq!(uploads(&back).len(), 1);
}

/// Asserts that a key with `&` and `<` in it, and a prefix and a marker with them, come back
/// escaped in the XML (so that the document parses and gives the key as it was), and that the
/// markers the page names are the escaped text too. It catches a listing that writes a key into
/// the document as it is.
#[tokio::test(start_paused = true)]
async fn a_key_with_xml_characters_is_escaped_in_the_listing() {
    let rig = Rig::new(1 << 20).await;
    let first = rig.cold_mem.create_upload("t&c/<one>").await.unwrap();
    let second = rig.cold_mem.create_upload("t&c/two").await.unwrap();

    let page = list(&rig, "prefix=t%26c%2F&max-uploads=1").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body.contains("<Key>t&amp;c/&lt;one&gt;</Key>"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("<Prefix>t&amp;c/</Prefix>"),
        "{}",
        page.body
    );
    assert_eq!(uploads(&page), [("t&c/<one>".to_owned(), first.clone())]);
    assert_eq!(texts(&page.body, "NextKeyMarker"), ["t&c/<one>"]);

    let marker = "key-marker=t%26c%2F%3Cone%3E";
    let rest = list(&rig, &format!("{marker}&upload-id-marker={first}")).await;
    assert!(
        rest.body
            .contains("<KeyMarker>t&amp;c/&lt;one&gt;</KeyMarker>"),
        "{}",
        rest.body
    );
    assert_eq!(uploads(&rest), [("t&c/two".to_owned(), second)]);
}
