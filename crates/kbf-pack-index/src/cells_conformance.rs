//! The rules of [`crate::cells`], stated once for every index: the conformance suite of the
//! cell rows (`C1` to `C8`) and of the unlisted trees and the live-pack sweep (`U1` to `U6`),
//! over the same [`PackBackend`] factory the pack index's suite uses, so the in-memory index and
//! the Postgres one run the same clauses. Every assertion message starts with its clause and
//! says what a wrong index would do. Time is the [`ManualClock`] the index is opened with.

use std::sync::Arc;
use std::time::Duration;

use crate::cells::{CELL_BYTES, LivePack, MAX_PAGE, ObjectCells, Unlisted};
use crate::conformance::{PackBackend, at, digest, packed, same, yes};
use crate::{MAX_BATCH, PackIndex, PackIndexError};
use kbf_state::ManualClock;

/// Runs every clause against `backend`.
pub fn run_all<B: PackBackend>(backend: &B) {
    c1_recorded_cells_are_read_back(backend);
    c2_the_same_cells_again_change_nothing(backend);
    c3_other_cells_conflict_and_change_nothing(backend);
    c4_cells_that_cannot_exist_are_refused(backend);
    c5_the_size_boundaries(backend);
    c6_a_read_of_keys_is_bounded(backend);
    c7_a_key_must_not_be_empty(backend);
    c8_cells_are_kept_apart_from_packs(backend);
    u1_an_unlisted_tree_is_recorded_and_moved(backend);
    u2_unlisted_trees_come_by_id_after_a_cursor(backend);
    u3_the_window_edge_is_listed_and_older_is_not(backend);
    u4_a_page_is_bounded(backend);
    u5_a_tree_is_dropped_once(backend);
    u6_live_packs_skip_dead_ones_and_say_whether_they_have_cells(backend);
}

fn open<B: PackBackend>(backend: &B) -> (B::Index, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    (backend.open_index(Arc::clone(&clock)), clock)
}

/// The cells of an object of `size` bytes, each digest filled with `seed + its place`.
#[must_use]
pub fn cells(size: u64, seed: u8) -> ObjectCells {
    let count = ObjectCells::cells_of(size);
    ObjectCells {
        size,
        digests: (0..count)
            .map(|n| [seed.wrapping_add(u8::try_from(n % 200).unwrap_or(0)); 32])
            .collect(),
    }
}

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|k| (*k).to_owned()).collect()
}

fn read(index: &dyn PackIndex, names: &[&str]) -> Vec<Option<ObjectCells>> {
    index.object_cells(&keys(names)).expect("read")
}

fn listed(index: &dyn PackIndex, within: Duration) -> Vec<[u8; 32]> {
    let all = index.unlisted(within, None, MAX_PAGE).expect("list");
    all.iter().map(|u| u.tree).collect()
}

const HOUR: Duration = Duration::from_secs(3600);

/// C1: recorded cells are read back whole, in the order asked, a key asked twice answers twice,
/// and a key never recorded has none; a read writes nothing.
pub fn c1_recorded_cells_are_read_back<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let (one, two) = (cells(1, 1), cells(CELL_BYTES * 2 + 5, 10));
    same(
        "C1 a record of new cells did not answer true",
        index.record_cells("cas/f/one", &one),
        Ok(true),
    );
    same(
        "C1 a record of new cells of three cells did not answer true",
        index.record_cells("cas/f/two", &two),
        Ok(true),
    );
    same(
        "C1 a read did not answer each key with its cells, in the order asked",
        read(
            &index,
            &["cas/f/two", "cas/f/none", "cas/f/one", "cas/f/two"],
        ),
        vec![Some(two.clone()), None, Some(one), Some(two)],
    );
    same(
        "C1 a read of no keys answered something",
        read(&index, &[]),
        Vec::new(),
    );
}

/// C2: the same cells recorded again answer false and change nothing.
pub fn c2_the_same_cells_again_change_nothing<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let one = cells(CELL_BYTES + 1, 3);
    same(
        "C2 the first record did not answer true",
        index.record_cells("cas/f/one", &one),
        Ok(true),
    );
    same(
        "C2 the same cells again did not answer false",
        index.record_cells("cas/f/one", &one),
        Ok(false),
    );
    same(
        "C2 the same cells again changed the row",
        read(&index, &["cas/f/one"]),
        vec![Some(one)],
    );
}

/// C3: other cells under a recorded key (another digest, another size) are a conflict that
/// names the key, and the first row is kept.
pub fn c3_other_cells_conflict_and_change_nothing<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let first = cells(CELL_BYTES + 1, 3);
    let other_digest = cells(CELL_BYTES + 1, 4);
    let other_size = cells(CELL_BYTES + 2, 3);
    let other_count = cells(1, 3);
    index.record_cells("cas/f/one", &first).expect("first");
    let conflict = Err(PackIndexError::Conflict {
        key: "cas/f/one".to_owned(),
    });
    for other in [other_digest, other_size, other_count] {
        same(
            "C3 other cells under a recorded key were not a conflict",
            index.record_cells("cas/f/one", &other),
            conflict.clone(),
        );
    }
    same(
        "C3 a conflicting record changed the first row",
        read(&index, &["cas/f/one"]),
        vec![Some(first)],
    );
}

/// C4: cells that cannot exist are invalid and write nothing: no bytes, a size a `bigint` does
/// not hold, and a count of digests that is not the size's.
pub fn c4_cells_that_cannot_exist_are_refused<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let empty = ObjectCells::default();
    same(
        "C4 an object of no bytes was not refused",
        index.record_cells("cas/f/e", &empty),
        Err(PackIndexError::Invalid(
            "an object of no bytes has no cells",
        )),
    );
    let huge = ObjectCells {
        size: u64::try_from(i64::MAX).unwrap_or(0) + 1,
        digests: Vec::new(),
    };
    same(
        "C4 an object longer than a bigint was not refused",
        index.record_cells("cas/f/h", &huge),
        Err(PackIndexError::Invalid(
            "the object is longer than the index can hold",
        )),
    );
    let wrong = "the cells are not those of the object's size";
    let mut few = cells(CELL_BYTES + 1, 1);
    few.digests.pop();
    let mut many = cells(CELL_BYTES, 1);
    many.digests.push([9; 32]);
    let none = ObjectCells {
        size: 5,
        digests: Vec::new(),
    };
    for bad in [few, many, none] {
        same(
            "C4 a wrong number of digests was not refused",
            index.record_cells("cas/f/w", &bad),
            Err(PackIndexError::Invalid(wrong)),
        );
    }
    same(
        "C4 a refused record wrote a row",
        read(&index, &["cas/f/e", "cas/f/h", "cas/f/w"]),
        vec![None, None, None],
    );
}

/// C5: the boundaries of the size: one byte has one cell, exactly one cell has one, one byte
/// more has two, and the largest size a `bigint` holds is refused only for its digests.
pub fn c5_the_size_boundaries<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    for (key, size, seed) in [
        ("cas/f/a", 1, 1),
        ("cas/f/b", CELL_BYTES, 2),
        ("cas/f/c", CELL_BYTES + 1, 3),
        ("cas/f/d", CELL_BYTES * 2, 4),
    ] {
        let row = cells(size, seed);
        same(
            "C5 cells of a size at a cell boundary were not taken",
            index.record_cells(key, &row),
            Ok(true),
        );
        same(
            "C5 cells of a size at a cell boundary were not read back",
            read(&index, &[key]),
            vec![Some(row)],
        );
    }
    same(
        "C5 the digest counts of the sizes at the boundaries are not 1, 1, 2, 2",
        [1, CELL_BYTES, CELL_BYTES + 1, CELL_BYTES * 2].map(ObjectCells::cells_of),
        [1, 1, 2, 2],
    );
    let top = ObjectCells {
        size: u64::try_from(i64::MAX).unwrap_or(0),
        digests: vec![[1; 32]],
    };
    same(
        "C5 the largest size a bigint holds, with too few digests, was not refused for them",
        index.record_cells("cas/f/top", &top),
        Err(PackIndexError::Invalid(
            "the cells are not those of the object's size",
        )),
    );
}

/// C6: a read of more than [`MAX_BATCH`] keys is refused with the count, and one of exactly
/// that many is taken.
pub fn c6_a_read_of_keys_is_bounded<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let names: Vec<String> = (0..=MAX_BATCH).map(|n| format!("cas/f/{n}")).collect();
    same(
        "C6 a read of more than the batch bound was not refused",
        index.object_cells(&names),
        Err(PackIndexError::TooMany {
            given: MAX_BATCH + 1,
            max: MAX_BATCH,
        }),
    );
    let found = index
        .object_cells(&names[..MAX_BATCH])
        .expect("at the bound");
    same(
        "C6 a read of exactly the batch bound did not answer every key",
        found.len(),
        MAX_BATCH,
    );
}

/// C7: an empty key is refused, whatever the cells, and writes nothing.
pub fn c7_a_key_must_not_be_empty<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    same(
        "C7 an empty key was not refused",
        index.record_cells("", &cells(1, 1)),
        Err(PackIndexError::Invalid("the object key is empty")),
    );
    same(
        "C7 a refused empty key was readable",
        read(&index, &[""]),
        vec![None],
    );
}

/// C8: cells under a key do not make a pack of that key, and a pack does not make cells.
pub fn c8_cells_are_kept_apart_from_packs<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    index
        .record_cells("cas/p/one", &cells(30, 1))
        .expect("cells");
    same(
        "C8 cells made a pack row",
        index.pack("cas/p/one"),
        Ok(None),
    );
    index
        .record_pack(&packed("cas/p/two", "w", &[(1, 10)]))
        .expect("pack");
    same(
        "C8 a pack made cells",
        read(&index, &["cas/p/two"]),
        vec![None],
    );
}

/// U1: an unlisted tree is a row with the index's clock as its first and last time; recorded
/// again it keeps the first and moves the last; trees are separate rows.
pub fn u1_an_unlisted_tree_is_recorded_and_moved<B: PackBackend>(backend: &B) {
    let (index, clock) = open(backend);
    clock.advance(Duration::from_secs(5));
    let first_at = at(&clock);
    same(
        "U1 a new tree was not recorded",
        index.record_unlisted(&[1; 32]),
        Ok(()),
    );
    same(
        "U1 a new tree did not have the clock as its first and last time",
        index.unlisted(HOUR, None, 10),
        Ok(vec![Unlisted {
            tree: [1; 32],
            first_us: first_at,
            last_us: first_at,
        }]),
    );
    clock.advance(Duration::from_secs(7));
    let later = at(&clock);
    index.record_unlisted(&[1; 32]).expect("again");
    index.record_unlisted(&[2; 32]).expect("another");
    same(
        "U1 a tree recorded again did not keep its first time and take the clock as its last",
        index.unlisted(HOUR, None, 10),
        Ok(vec![
            Unlisted {
                tree: [1; 32],
                first_us: first_at,
                last_us: later,
            },
            Unlisted {
                tree: [2; 32],
                first_us: later,
                last_us: later,
            },
        ]),
    );
}

/// U2: trees are listed in the order of their ids as bytes, after a cursor that is itself not
/// listed, and a page ends at the limit.
pub fn u2_unlisted_trees_come_by_id_after_a_cursor<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let trees = [[0x80; 32], [3; 32], [1; 32], [0x7f; 32], [2; 32]];
    for tree in &trees {
        index.record_unlisted(tree).expect("record");
    }
    let ordered = vec![[1; 32], [2; 32], [3; 32], [0x7f; 32], [0x80; 32]];
    same(
        "U2 trees were not listed by id as bytes",
        listed(&index, HOUR),
        ordered,
    );
    let page = index.unlisted(HOUR, None, 2).expect("page");
    same(
        "U2 a page did not end at the limit",
        page.iter().map(|u| u.tree).collect::<Vec<_>>(),
        vec![[1; 32], [2; 32]],
    );
    let after = index.unlisted(HOUR, Some(&[2; 32]), 2).expect("after");
    same(
        "U2 a page after a cursor did not start past it",
        after.iter().map(|u| u.tree).collect::<Vec<_>>(),
        vec![[3; 32], [0x7f; 32]],
    );
    let between = index.unlisted(HOUR, Some(&[4; 32]), 10).expect("between");
    same(
        "U2 a cursor that is not a recorded tree did not start at the next one",
        between.iter().map(|u| u.tree).collect::<Vec<_>>(),
        vec![[0x7f; 32], [0x80; 32]],
    );
    same(
        "U2 a cursor at the last tree did not end the list",
        index.unlisted(HOUR, Some(&[0x80; 32]), 10),
        Ok(Vec::new()),
    );
    same(
        "U2 a limit of nothing listed a tree",
        index.unlisted(HOUR, None, 0),
        Ok(Vec::new()),
    );
}

/// U3: a tree last recorded exactly the window ago is listed and one a microsecond older is
/// not; recording it again brings it back.
pub fn u3_the_window_edge_is_listed_and_older_is_not<B: PackBackend>(backend: &B) {
    let (index, clock) = open(backend);
    clock.advance(Duration::from_secs(100));
    let within = Duration::from_secs(30);
    index.record_unlisted(&[1; 32]).expect("record");
    clock.advance(within);
    same(
        "U3 a tree last recorded exactly the window ago was not listed",
        listed(&index, within),
        vec![[1; 32]],
    );
    clock.advance(Duration::from_micros(1));
    same(
        "U3 a tree last recorded a microsecond before the window was listed",
        listed(&index, within),
        Vec::<[u8; 32]>::new(),
    );
    same(
        "U3 a wider window did not list the tree",
        listed(&index, HOUR),
        vec![[1; 32]],
    );
    index.record_unlisted(&[1; 32]).expect("again");
    same(
        "U3 a tree recorded again was not listed in the window",
        listed(&index, within),
        vec![[1; 32]],
    );
    same(
        "U3 a window longer than the clock's age listed nothing",
        listed(&index, Duration::MAX),
        vec![[1; 32]],
    );
}

/// U4: a page of more than [`MAX_PAGE`] is refused with the count, for the trees and for the
/// packs, and a page of exactly that many is taken.
pub fn u4_a_page_is_bounded<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    let refused = PackIndexError::TooMany {
        given: MAX_PAGE + 1,
        max: MAX_PAGE,
    };
    same(
        "U4 a page of trees over the bound was not refused",
        index.unlisted(HOUR, None, MAX_PAGE + 1),
        Err(refused.clone()),
    );
    same(
        "U4 a page of packs over the bound was not refused",
        index.live_packs(None, MAX_PAGE + 1),
        Err(refused),
    );
    same(
        "U4 a page of trees of exactly the bound was not taken",
        index.unlisted(HOUR, None, MAX_PAGE),
        Ok(Vec::new()),
    );
    same(
        "U4 a page of packs of exactly the bound was not taken",
        index.live_packs(None, MAX_PAGE),
        Ok(Vec::new()),
    );
}

/// U5: dropping a recorded tree answers true and removes it, dropping it again or a tree never
/// recorded answers false, and the other trees stay.
pub fn u5_a_tree_is_dropped_once<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    index.record_unlisted(&[1; 32]).expect("one");
    index.record_unlisted(&[2; 32]).expect("two");
    same(
        "U5 a drop of a tree never recorded answered true",
        index.drop_unlisted(&[9; 32]),
        Ok(false),
    );
    same(
        "U5 a drop of a recorded tree did not answer true",
        index.drop_unlisted(&[1; 32]),
        Ok(true),
    );
    same(
        "U5 a second drop answered true",
        index.drop_unlisted(&[1; 32]),
        Ok(false),
    );
    same(
        "U5 a drop removed another tree or kept the dropped one",
        listed(&index, HOUR),
        vec![[2; 32]],
    );
    index.record_unlisted(&[1; 32]).expect("again");
    yes(
        "U5 a dropped tree could not be recorded anew",
        listed(&index, HOUR).len() == 2,
    );
}

/// U6: the live packs are those with a blob row pointing at them, by key as bytes, after a
/// cursor, with whether their cells are recorded; a pack whose rows were all dropped, and a pack
/// of no blobs, are not listed.
pub fn u6_live_packs_skip_dead_ones_and_say_whether_they_have_cells<B: PackBackend>(backend: &B) {
    let (index, _) = open(backend);
    // Byte order puts capitals before `_` before lower case; a locale's order does not.
    let (upper, under, lower) = ("cas/f/B1", "cas/f/_x", "cas/f/a1");
    index
        .record_pack(&packed(lower, "w", &[(1, 10)]))
        .expect("a");
    index
        .record_pack(&packed(upper, "w", &[(2, 20)]))
        .expect("b");
    index
        .record_pack(&packed(under, "w", &[(3, 30)]))
        .expect("c");
    index
        .record_pack(&packed("cas/f/dead", "w", &[(4, 40)]))
        .expect("d");
    index
        .record_pack(&packed("cas/f/empty", "w", &[]))
        .expect("e");
    yes(
        "U6 the drop of a pack's only location found no row",
        index
            .drop_location(&digest(4, 40), "cas/f/dead")
            .expect("drop"),
    );
    index.record_cells(under, &cells(5, 1)).expect("cells");
    let live = |key: &str, size: u64, has_cells: bool| LivePack {
        key: key.to_owned(),
        size,
        has_cells,
    };
    let all = vec![
        live(upper, 36, false),
        live(under, 46, true),
        live(lower, 26, false),
    ];
    same(
        "U6 live packs were not those with a blob row, by key, with whether they have cells",
        index.live_packs(None, 10),
        Ok(all.clone()),
    );
    same(
        "U6 a page did not end at the limit",
        index.live_packs(None, 2),
        Ok(all[..2].to_vec()),
    );
    same(
        "U6 a page after a cursor did not start past it",
        index.live_packs(Some(upper), 10),
        Ok(all[1..].to_vec()),
    );
    same(
        "U6 a cursor at the last pack did not end the list",
        index.live_packs(Some(lower), 10),
        Ok(Vec::new()),
    );
    index.record_cells(lower, &cells(5, 2)).expect("cells");
    same(
        "U6 cells recorded after a listing were not reported",
        index
            .live_packs(Some(under), 10)
            .map(|p| p.iter().map(|p| p.has_cells).collect::<Vec<_>>()),
        Ok(vec![true]),
    );
}
