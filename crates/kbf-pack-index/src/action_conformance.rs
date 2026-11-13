//! The conformance suite for [`ActionStore`], A1 to A9, over the same [`PackBackend`] factory
//! the pack index's suite uses, so the in-memory index and the Postgres one run the same
//! clauses. Every assertion message starts with its clause and says what a wrong store would
//! do. No clause reads the wall clock: time is the [`ManualClock`] the index is opened with,
//! and waits are joins.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::action::{ActionEntry, ActionPut, ActionStore, MAX_RESULT_BYTES};
use crate::conformance::{
    KEY, OTHER, PackBackend, THIRD, at, created, digest, existing, info, locate, packed, same, yes,
};
use crate::{NewPack, PackIndex as _, PackIndexError, Recorded};
use kbf_state::ManualClock;

/// Runs A1 to A9 against `backend`.
pub fn run_all<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    a1_a_result_is_found_by_its_action(backend);
    a2_a_put_replaces(backend);
    a3_an_empty_result_is_a_result(backend);
    a4_a_long_result_is_refused(backend);
    a5_a_put_records_its_packs(backend);
    a6_a_refused_pack_writes_nothing(backend);
    a7_a_put_repeats(backend);
    a8_concurrent_puts_agree(backend);
    a9_a_second_handle_sees_the_row(backend);
}

fn open<B: PackBackend>(backend: &B) -> (B::Index, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    (backend.open_index(Arc::clone(&clock)), clock)
}

fn get(index: &dyn ActionStore, action: u32, size: u64) -> Option<ActionEntry> {
    index.get_action(&digest(action, size)).expect("read")
}

fn put(index: &dyn ActionStore, action: u32, result: &[u8], packs: &[NewPack]) -> ActionPut {
    index
        .put_action(&digest(action, 5), result, packs)
        .expect("put")
}

fn entry(result: &[u8], written_us: i64) -> Option<ActionEntry> {
    Some(ActionEntry {
        result: result.to_vec(),
        written_us,
    })
}

/// A1: a put result is read back as it was put, with the time of the put; an action never put
/// has no row, and two actions with the same hash and other sizes are two actions.
pub fn a1_a_result_is_found_by_its_action<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, clock) = open(backend);
    same("A1 an action never put had a row", get(&index, 1, 5), None);
    clock.advance(Duration::from_secs(3));
    let first = put(&index, 1, b"result-one", &[]);
    same(
        "A1 a first put was not reported as new, or recorded packs from nowhere",
        first,
        ActionPut {
            replaced: false,
            packs: Vec::new(),
        },
    );
    same(
        "A1 the result read is not the bytes put with the time of the put",
        get(&index, 1, 5),
        entry(b"result-one", at(&clock)),
    );
    same(
        "A1 an action of the same hash and another size read another action's row",
        get(&index, 1, 6),
        None,
    );
    same(
        "A1 a put for one action gave another action a row",
        get(&index, 2, 5),
        None,
    );
    put(&index, 2, b"result-two", &[]);
    same(
        "A1 a second action's put changed the first action's row",
        get(&index, 1, 5).map(|e| e.result),
        Some(b"result-one".to_vec()),
    );
}

/// A2: a later put replaces the row, says so, and moves the time; the bytes may be any bytes.
pub fn a2_a_put_replaces<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, clock) = open(backend);
    put(&index, 1, b"old", &[]);
    clock.advance(Duration::from_secs(9));
    let again = put(&index, 1, &[0, 255, 0, 10], &[]);
    yes(
        "A2 a put over a row did not say it replaced one",
        again.replaced,
    );
    same(
        "A2 the row after a second put is not the second result at the second time",
        get(&index, 1, 5),
        entry(&[0, 255, 0, 10], at(&clock)),
    );
}

/// A3: a result that encodes to no bytes is a result: it is stored and read back empty, which
/// is not the same as no row.
pub fn a3_an_empty_result_is_a_result<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, clock) = open(backend);
    put(&index, 1, b"", &[]);
    same(
        "A3 an empty result was read as no row, or not stored",
        get(&index, 1, 5),
        entry(b"", at(&clock)),
    );
}

/// A4: a result longer than [`MAX_RESULT_BYTES`] is refused and changes nothing, the longest
/// one is held, and the refusal of a long result writes the packs of the call no more than it
/// writes the row.
pub fn a4_a_long_result_is_refused<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, _) = open(backend);
    let longest = vec![7_u8; MAX_RESULT_BYTES];
    put(&index, 1, &longest, &[]);
    same(
        "A4 the longest result an action row holds was not held",
        get(&index, 1, 5).map(|e| e.result.len()),
        Some(MAX_RESULT_BYTES),
    );
    let long = vec![7_u8; MAX_RESULT_BYTES + 1];
    let pack = packed(KEY, "fe1", &[(1, 10)]);
    let refused = index.put_action(&digest(2, 5), &long, &[pack]);
    same(
        "A4 a result past the limit was not refused as invalid",
        refused.err(),
        Some(PackIndexError::Invalid(
            "the action result is longer than an action row holds",
        )),
    );
    same("A4 a refused put wrote its row", get(&index, 2, 5), None);
    same(
        "A4 a refused put recorded its pack",
        info(&index, KEY),
        None,
    );
}

/// A5: a put records its packs as `record_pack` does and answers for each in the order given
/// (not the order of their keys), and the blobs are then found through the index.
pub fn a5_a_put_records_its_packs<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, _) = open(backend);
    let first = packed(OTHER, "fe1", &[(1, 10), (2, 20)]);
    let second = packed(KEY, "fe1", &[(2, 20), (3, 30)]);
    let put = put(&index, 1, b"r", &[first, second]);
    same(
        "A5 the answers for the packs are not in the order the packs were given, or a blob a \
         pack shares with an earlier one was indexed twice",
        put.packs,
        vec![created(1), created(2)],
    );
    yes(
        "A5 a pack of the put was not recorded",
        [info(&index, KEY), info(&index, OTHER)]
            .iter()
            .flatten()
            .count()
            == 2,
    );
    let found = locate(&index, &[digest(1, 10), digest(2, 20), digest(3, 30)]);
    yes(
        "A5 a blob of a pack of the put has no row",
        found.iter().flatten().count() == found.len(),
    );
    same(
        "A5 the first location of a shared blob was not kept: the pack whose key sorts first \
         records first",
        found[1].as_ref().map(|l| l.pack_key.as_str()),
        Some(KEY),
    );
}

/// A6: a pack the put cannot record (a conflict with a recorded key, or one that cannot exist)
/// leaves nothing behind: no row for the action, no other pack of the call, and the row the
/// action had before is as it was.
pub fn a6_a_refused_pack_writes_nothing<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, _) = open(backend);
    index
        .record_pack(&packed(KEY, "fe1", &[(1, 10)]))
        .expect("recorded");
    let good = packed(OTHER, "fe1", &[(2, 20)]);
    let clash = packed(KEY, "fe2", &[(1, 10)]);
    let refused = index.put_action(&digest(1, 5), b"new", &[good.clone(), clash]);
    same(
        "A6 a pack recorded with other contents was not a conflict",
        refused.err(),
        Some(PackIndexError::Conflict {
            key: KEY.to_owned(),
        }),
    );
    same("A6 a conflict still wrote the row", get(&index, 1, 5), None);
    same(
        "A6 a conflict left an earlier pack of the call recorded",
        info(&index, OTHER),
        None,
    );
    same(
        "A6 a conflict left a blob of an earlier pack indexed",
        locate(&index, &[digest(2, 20)]),
        vec![None],
    );
    let mut bad = packed(THIRD, "fe1", &[(3, 30)]);
    bad.key.clear();
    let invalid = index.put_action(&digest(1, 5), b"new", &[good.clone(), bad]);
    same(
        "A6 a pack that cannot exist was not refused as invalid",
        invalid.err(),
        Some(PackIndexError::Invalid("the pack key is empty")),
    );
    same(
        "A6 an invalid pack still wrote the row",
        get(&index, 1, 5),
        None,
    );
    same(
        "A6 an invalid pack left another pack of the call recorded",
        info(&index, OTHER),
        None,
    );
    put(&index, 1, b"kept", std::slice::from_ref(&good));
    let clash = packed(KEY, "fe2", &[(1, 10)]);
    let refused = index.put_action(&digest(1, 5), b"lost", &[clash]);
    yes("A6 a conflict was not refused", refused.is_err());
    same(
        "A6 a refused put changed the row the action had",
        get(&index, 1, 5).map(|e| e.result),
        Some(b"kept".to_vec()),
    );
}

/// A7: a put repeated with the same packs succeeds: the row is replaced and each pack says it
/// was recorded before, with the blobs it indexed then.
pub fn a7_a_put_repeats<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, _) = open(backend);
    let pack = packed(KEY, "fe1", &[(1, 10), (2, 20)]);
    let first = put(&index, 1, b"r", std::slice::from_ref(&pack));
    same("A7 the first put's pack", first.packs, vec![created(2)]);
    let again = put(&index, 1, b"r", std::slice::from_ref(&pack));
    same(
        "A7 a repeated put was refused, or its pack not recognised as recorded before",
        again,
        ActionPut {
            replaced: true,
            packs: vec![existing(2)],
        },
    );
}

/// A8: puts from two connections at once all succeed. Two actions that name the same pack
/// record it once (one of them `created`), and a row of one action put by both ends as the
/// result of one of them.
pub fn a8_concurrent_puts_agree<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, _) = open(backend);
    let other = backend.reconnect(&index);
    let shared = packed(KEY, "fe1", &[(1, 10)]);
    let own = |n: u32| packed(&format!("cas/f/t{n}/1/1"), "fe1", &[(10 + n, 5)]);
    let (a, b) = thread::scope(|scope| {
        let a = scope.spawn(|| {
            (0..20)
                .map(|n| put(&index, 100 + n, b"a", &[shared.clone(), own(n)]))
                .collect::<Vec<_>>()
        });
        let b = scope.spawn(|| {
            (0..20)
                .map(|n| put(&other, 100 + n, b"b", &[own(n + 100), shared.clone()]))
                .collect::<Vec<_>>()
        });
        (
            a.join().expect("the first writer did not panic"),
            b.join().expect("the second writer did not panic"),
        )
    });
    let recorded: Vec<Recorded> = a.iter().chain(&b).flat_map(|p| p.packs.clone()).collect();
    let news = recorded.iter().filter(|r| r.created).count();
    same(
        "A8 the shared pack was recorded as new other than once, or a pack of one action was \
         not recorded as new",
        news,
        1 + 40,
    );
    for n in 0..20 {
        let result = get(&index, 100 + n, 5).map(|e| e.result);
        let either = [Some(b"a".to_vec()), Some(b"b".to_vec())];
        yes(
            "A8 an action put by both connections has neither connection's result",
            either.contains(&result),
        );
    }
}

/// A9: a row put through one handle is read through another handle on the same data, which is
/// the proof that it was committed and not held by the connection that put it.
pub fn a9_a_second_handle_sees_the_row<B: PackBackend>(backend: &B)
where
    B::Index: ActionStore,
{
    let (index, clock) = open(backend);
    put(&index, 1, b"r", &[packed(KEY, "fe1", &[(1, 10)])]);
    let other = backend.reconnect(&index);
    same(
        "A9 a row put through one handle was not read through another",
        get(&other, 1, 5),
        entry(b"r", at(&clock)),
    );
}
