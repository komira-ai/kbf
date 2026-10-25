//! The checksum headers of the completion of a multipart upload, which are the checksums of the
//! object it makes and not of its body.

use hyper::StatusCode;

use crate::Backend;

use super::ops_tests::{completion, start, upload};
use super::testing::{BUCKET, CHECKSUMS, Rig, checksum};

const MEBIBYTE: u64 = 1 << 20;

/// Asserts that the checksum headers of a completion, which are the checksums of the whole
/// object it makes, are not held to the XML body of the call: each of them, right for the
/// object, and one of an algorithm the server does not compute, leave the completion to
/// succeed; and that `Content-MD5`, which is the hash of the body, is held to it: a wrong one
/// is 400 `BadDigest` and leaves the upload open, and the right one completes it.
#[tokio::test(start_paused = true)]
async fn a_completion_is_not_held_to_the_checksums_of_the_object_it_makes() {
    let rig = Rig::new(MEBIBYTE).await;
    let whole = b"hello world";
    let mut names: Vec<&str> = CHECKSUMS.into_iter().skip(1).collect();
    names.push("x-amz-checksum-crc64nvme");
    for name in names {
        let id = start(&rig, name).await;
        let part = upload(&rig, name, &id, 1, whole).await;
        let body = completion(&[(1, part.header("etag"))]);
        let value = checksum(name, whole);
        let uri = format!("/{BUCKET}/{name}?uploadId={id}");
        let extra = [(name, value.as_str())];
        let done = rig.send("POST", &uri, &extra, body.as_bytes()).await;
        assert_eq!(done.status, StatusCode::OK, "{name}");
        let stored = rig.cold_mem.get(name, None).await.unwrap();
        assert_eq!(stored, &whole[..], "{name}");
    }
    let id = start(&rig, "md5").await;
    let part = upload(&rig, "md5", &id, 1, whole).await;
    let body = completion(&[(1, part.header("etag"))]);
    let uri = format!("/{BUCKET}/md5?uploadId={id}");
    let wrong = checksum("content-md5", b"another body");
    let extra = [("content-md5", wrong.as_str())];
    let refused = rig.send("POST", &uri, &extra, body.as_bytes()).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.code(), "BadDigest");
    let right = checksum("content-md5", body.as_bytes());
    let extra = [("content-md5", right.as_str())];
    let done = rig.send("POST", &uri, &extra, body.as_bytes()).await;
    assert_eq!(done.status, StatusCode::OK);
}
