use kbf_testkit::dir::TestDir;

use super::*;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

async fn list_all(store: &FsStore, prefix: &KeyPrefix) -> Vec<String> {
    let mut keys = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .list(prefix, after.as_ref(), PageSize::new(2).expect("size"))
            .await
            .expect("list");
        keys.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
        match page.next {
            Some(t) => after = Some(t),
            None => return keys,
        }
    }
}

/// Catches a listing that reads every header again for each page (a rebuild over
/// a directory of N objects would open N files per page, N squared over the
/// listing): 7 keys in pages of 2 take one walk. A listing interleaved with one of
/// another prefix walks again rather than cutting its pages from the other's keys,
/// a token of a listing no longer kept or used under another prefix walks again,
/// and a listing that ended keeps nothing for the next.
#[tokio::test]
async fn a_paged_listing_reads_the_headers_once() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-objstore-walks");
    let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    for i in 0..7 {
        for p in ["a", "b"] {
            let k = key(&format!("{p}/{i}"));
            store
                .put_new(&k, Bytes::from_static(b"x"), None)
                .await
                .expect("put");
        }
    }
    let walks = || store.inner.walks.load(Ordering::Relaxed);
    let a = KeyPrefix::new("a/").expect("prefix");
    let b = KeyPrefix::new("b/").expect("prefix");
    let want: Vec<String> = (0..7).map(|i| format!("a/{i}")).collect();
    assert_eq!(list_all(&store, &a).await, want);
    assert_eq!(walks(), 1);
    assert!(store.inner.listing.lock().expect("lock").is_none());

    let size = PageSize::new(2).expect("size");
    let first = store.list(&a, None, size).await.expect("list");
    let other = store.list(&b, None, size).await.expect("list");
    assert_eq!(other.objects[0].key.as_str(), "b/0");
    let second = store
        .list(&a, first.next.as_ref(), size)
        .await
        .expect("list");
    let keys: Vec<&str> = second.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a/2", "a/3"]);
    assert_eq!(walks(), 4);

    // A token of a listing no longer kept (here, of a's first walk) walks again,
    // and so does a token of the kept listing used under another prefix.
    let stale = store
        .list(&a, first.next.as_ref(), size)
        .await
        .expect("list");
    let keys: Vec<&str> = stale.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a/2", "a/3"]);
    assert_eq!(walks(), 5);
    let elsewhere = store
        .list(&b, stale.next.as_ref(), size)
        .await
        .expect("list");
    let keys: Vec<&str> = elsewhere.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["b/0", "b/1"]);
    assert_eq!(walks(), 6);
    drop(store);
}

/// Catches a listing that keeps every key under its prefix (memory growing with the
/// prefix, not the window), one that ends at the end of its first window, and one
/// that cuts the next page from a used-up window instead of reading the next: 7
/// keys through a window of 3 in pages of 2 come back whole, in order, never more
/// than 3 kept, in 3 walks. Pages that stay within a window walk no more. A file in
/// a shard directory that is not an object is skipped by every walk.
#[tokio::test]
async fn a_listing_longer_than_its_window_keeps_one_window_at_a_time() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-objstore-window");
    let mut store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    Arc::get_mut(&mut store.inner).expect("one handle").window = 3;
    for i in 0..7 {
        for p in ["a", "b"] {
            let k = key(&format!("{p}/{i}"));
            store
                .put_new(&k, Bytes::from_static(b"x"), None)
                .await
                .expect("put");
        }
    }
    let shard = store.inner.objects.join("00");
    fs::create_dir_all(&shard).expect("shard");
    fs::write(shard.join("foreign"), b"not an object").expect("plant");
    let a = KeyPrefix::new("a/").expect("prefix");
    let size = PageSize::new(2).expect("size");
    let (mut got, mut after, mut most) = (Vec::new(), None, 0);
    loop {
        let page = store.list(&a, after.as_ref(), size).await.expect("list");
        if let Some(kept) = store.inner.listing.lock().expect("lock").as_ref() {
            most = most.max(kept.keys.len());
        }
        got.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
        match page.next {
            Some(t) => after = Some(t),
            None => break,
        }
    }
    let want: Vec<String> = (0..7).map(|i| format!("a/{i}")).collect();
    assert_eq!(got, want);
    assert_eq!(most, 3, "the most keys a listing kept");
    assert_eq!(store.inner.walks.load(Ordering::Relaxed), 3);
    assert!(store.inner.listing.lock().expect("lock").is_none());
    drop(store);
}

/// Catches a fresh listing [`ListToken::after`] a key that walks from the start of
/// the prefix (with more keys before the resume key than the window holds, the
/// window is used up before it and the listing ends empty), or that starts at the
/// key: resumed after a/2 and after a/5 through a window of 3, in pages of 1 and 2,
/// the listing is exactly the keys after it, in order.
#[tokio::test]
async fn a_file_store_listing_resumed_past_its_window_lists_only_the_rest() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-objstore-resume");
    let mut store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    Arc::get_mut(&mut store.inner).expect("one handle").window = 3;
    for i in 0..9 {
        let k = key(&format!("a/{i}"));
        store
            .put_new(&k, Bytes::from_static(b"x"), None)
            .await
            .expect("put");
    }
    let a = KeyPrefix::new("a/").expect("prefix");
    for (from, first) in [("a/2", 3), ("a/5", 6)] {
        for size in [1, 2] {
            let size = PageSize::new(size).expect("size");
            let (mut got, mut after) = (Vec::new(), Some(ListToken::after(&key(from))));
            loop {
                let page = store.list(&a, after.as_ref(), size).await.expect("list");
                got.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
                // A listing that repeats a key would otherwise never end.
                assert!(got.len() <= 9, "after {from}: a key listed twice: {got:?}");
                match page.next {
                    Some(t) => after = Some(t),
                    None => break,
                }
            }
            let want: Vec<String> = (first..9).map(|i| format!("a/{i}")).collect();
            assert_eq!(got, want, "after {from} in pages of {}", size.get());
        }
    }
    drop(store);
}
