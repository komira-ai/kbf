//! The kept places themselves: what keeping one twice and forgetting one not kept do.

use super::*;

fn digest(n: u8) -> BlobDigest {
    BlobDigest {
        hash: [n; 32],
        size: u64::from(n),
    }
}

fn place(key: &str) -> Location {
    Location {
        pack_key: key.to_owned(),
        offset: 0,
        length: 1,
        last_touch_us: 0,
    }
}

/// Asserts that a place kept again replaces the one kept, and is kept once: it does not take a
/// second turn in the order, so it does not push another place out early.
#[test]
fn a_place_kept_again_is_kept_once() {
    let mut kept = Kept::default();
    kept.keep(digest(1), place("cas/d/n/b/1"), 2);
    kept.keep(digest(1), place("cas/d/n/b/2"), 2);
    kept.keep(digest(2), place("cas/d/n/b/3"), 2);
    assert_eq!(kept.order.len(), 2);
    assert_eq!(
        kept.at.get(&digest(1)).map(|p| p.pack_key.as_str()),
        Some("cas/d/n/b/2")
    );
    assert!(kept.at.contains_key(&digest(2)));
}

/// Asserts that forgetting a blob whose place is not kept changes nothing.
#[test]
fn forgetting_a_place_not_kept_changes_nothing() {
    let asked: Arc<dyn PackIndex> = Arc::new(kbf_pack_index::memory::MemoryPackIndex::new(
        Arc::new(kbf_state::ManualClock::default()),
    ));
    let placed = Placed::new(asked);
    placed
        .kept()
        .keep(digest(1), place("cas/d/n/b/1"), MAX_PLACED);
    placed.forget(&digest(9));
    assert_eq!(placed.len(), 1);
    placed.forget(&digest(1));
    assert_eq!((placed.len(), placed.kept().order.len()), (0, 0));
}

/// Asserts that dropping a location through `Placed` evicts the kept place for the digest and
/// removes the inner index's row only when it points into the named pack, so the next `locate`
/// asks the inner index and finds none.
#[test]
fn dropping_a_location_forgets_the_kept_place_and_the_row() {
    let inner =
        kbf_pack_index::memory::MemoryPackIndex::new(Arc::new(kbf_state::ManualClock::default()));
    let held = digest(4);
    let pack = NewPack {
        key: "cas/d/n/b/1".to_owned(),
        writer: "n".to_owned(),
        size: 20,
        blobs: vec![kbf_pack_index::PackBlob {
            digest: held,
            offset: 0,
        }],
    };
    inner.record_pack(&pack).unwrap();
    let placed = Placed::new(Arc::new(inner));
    assert!(placed.locate(&[held]).unwrap()[0].is_some());
    assert_eq!(placed.len(), 1);

    assert!(!placed.drop_location(&held, "cas/d/n/b/other").unwrap());
    assert_eq!(placed.len(), 0);
    assert!(placed.locate(&[held]).unwrap()[0].is_some());
    assert!(placed.drop_location(&held, "cas/d/n/b/1").unwrap());

    assert_eq!(placed.len(), 0);
    assert_eq!(placed.locate(&[held]).unwrap(), [None]);
}
