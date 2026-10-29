//! What every [`Backend`] does, as checks over any backend: the tests of the in-memory
//! backend call each of them, and, with the `s3` feature, so do those of the S3 backend over
//! the loopback S3 service. A check starts from an empty backend.

use tierstore::{Backend, Error};

use crate::support::{body, broken, check_ranges, chunks, keys, page};

/// A second put of a key is refused with `AlreadyExists`, and the first object stays.
pub async fn a_second_put_is_refused(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let again = backend.put("k", body("two")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
}

/// Ranges of an object are byte-exact.
pub async fn ranges_are_byte_exact(backend: &dyn Backend) {
    backend.put("k", body("0123456789")).await.unwrap();
    check_ranges(backend, "k").await;
}

/// Head gives the size and a content tag that is equal for equal bytes and different for
/// different bytes of the same size.
pub async fn head_gives_the_size_and_a_tag_of_the_content(backend: &dyn Backend) {
    backend.put("a", body("same")).await.unwrap();
    backend.put("b", body("same")).await.unwrap();
    backend.put("c", body("diff")).await.unwrap();
    let a = backend.head("a").await.unwrap();
    let b = backend.head("b").await.unwrap();
    let c = backend.head("c").await.unwrap();
    assert_eq!(a.size, 4);
    assert!(a.same_content(&b));
    assert!(!a.same_content(&c));
}

/// An absent key is not found by a get or a head, and deleting it succeeds.
pub async fn an_absent_key_is_not_found_and_deleting_it_succeeds(backend: &dyn Backend) {
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(backend.get("k", None).await.unwrap_err(), missing);
    assert_eq!(backend.head("k").await.unwrap_err(), missing);
    assert_eq!(backend.delete("k").await, Ok(()));
    assert_eq!(keys(backend).await, Vec::<String>::new());
}

/// A delete removes the object, and the key can be put again.
pub async fn a_deleted_key_is_gone_and_can_be_put_again(backend: &dyn Backend) {
    backend.put("k", body("old")).await.unwrap();
    backend.delete("k").await.unwrap();
    assert_eq!(keys(backend).await, Vec::<String>::new());
    backend.put("k", body("new")).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("new"));
}

/// A listing holds the keys under the prefix, in key order, with their sizes.
pub async fn a_listing_holds_the_keys_under_a_prefix_in_order(backend: &dyn Backend) {
    backend.put("b/2", body("bb")).await.unwrap();
    backend.put("a/1", body("a")).await.unwrap();
    backend.put("b/1", body("b")).await.unwrap();
    assert_eq!(keys(backend).await, ["a/1", "b/1", "b/2"]);
    let listed = backend.list("b/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [1, 2]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["b/1", "b/2"]);
}

/// A page of a listing holds at most `max_keys` keys that sort after the start-after key, in
/// key order, and a page shorter than `max_keys` is the last; a page of none asks for none.
pub async fn a_page_starts_after_a_key_and_is_cut_at_max_keys(backend: &dyn Backend) {
    for key in ["a", "b", "c", "d", "x"] {
        backend.put(key, body("v")).await.unwrap();
    }
    assert_eq!(page(backend, None, 2).await, ["a", "b"]);
    assert_eq!(page(backend, Some("b"), 2).await, ["c", "d"]);
    assert_eq!(page(backend, Some("d"), 2).await, ["x"]);
    assert_eq!(page(backend, Some("x"), 2).await, Vec::<String>::new());
    assert_eq!(page(backend, None, 0).await, Vec::<String>::new());
}

/// A put with the content tag of the object replaces it and gives the new object's content
/// tag; a put with another tag is refused and changes nothing; a put over an absent key is
/// `NotFound`.
pub async fn a_put_if_match_replaces_only_the_object_with_that_tag(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;

    let wrong = backend.put_if_match("k", body("x"), "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));

    let swap = backend.put_if_match("k", body("two"), &tag);
    let new_tag = swap.await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!(head.tag, new_tag);
    assert_ne!(new_tag, tag);
    assert_eq!(backend.get("k", None).await.unwrap(), body("two"));

    let stale = backend.put_if_match("k", body("three"), &tag).await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));

    let absent = backend.put_if_match("other", body("x"), &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
}

/// A streamed put creates the object from its chunks, whose length is told or not, with the
/// size and the content tag a put of the same bytes gives.
pub async fn a_streamed_put_creates_the_object_from_its_chunks(backend: &dyn Backend) {
    backend.put("whole", body("streamed body")).await.unwrap();
    let told = chunks(&["streamed", " ", "body"]);
    backend.put_stream("told", Some(13), told).await.unwrap();
    let untold = chunks(&["streamed ", "body"]);
    backend.put_stream("untold", None, untold).await.unwrap();
    for key in ["told", "untold"] {
        assert_eq!(backend.get(key, None).await.unwrap(), body("streamed body"));
        let (made, whole) = (
            backend.head(key).await.unwrap(),
            backend.head("whole").await,
        );
        assert!(made.same_content(&whole.unwrap()), "{key}");
    }
    backend
        .put_stream("empty", Some(0), chunks(&[]))
        .await
        .unwrap();
    assert_eq!(backend.get("empty", None).await.unwrap(), body(""));
}

/// A streamed put whose body ends with an error, after chunks or at once, gives that error and
/// leaves nothing: no object under the key and none in a listing; the key can be put again.
pub async fn a_streamed_put_that_fails_leaves_nothing(backend: &dyn Backend) {
    let cut = Error::Other("cut".to_owned());
    let after_chunks = backend
        .put_stream("k", Some(9), broken(&["abc", "def"], &cut))
        .await;
    assert_eq!(after_chunks, Err(cut.clone()));
    let at_once = backend.put_stream("k", None, broken(&[], &cut)).await;
    assert_eq!(at_once, Err(cut));
    assert_eq!(keys(backend).await, Vec::<String>::new());
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(backend.head("k").await.unwrap_err(), missing);
    backend
        .put_stream("k", None, chunks(&["fine"]))
        .await
        .unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("fine"));
}

/// A streamed put of a key that exists is refused with `AlreadyExists`, and the first object
/// stays.
pub async fn a_streamed_put_of_an_existing_key_is_refused(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let again = backend.put_stream("k", Some(3), chunks(&["two"])).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
    assert_eq!(keys(backend).await, ["k"]);
}
