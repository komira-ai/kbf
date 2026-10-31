//! The bounds of `GET /locate`: the presence memory, the caps on the index ask and the checks in
//! tierstore, the deadline of the store phase, and the read budget and places a table holds.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::job_read::{HEADS_IN_FLIGHT, Limits};
use kbf_types::Digest;
use tonic::codegen::http::{Method, StatusCode};

mod locate_common;

use locate_common::*;

/// Catches: a presence check that is never remembered (a HEAD for every request) or is
/// remembered for ever: a held object and a missing one are each checked once within their
/// times, and again after them; an object written after a negative answer is found once that
/// answer has run out.
#[tokio::test(start_paused = true)]
async fn presence_is_remembered_for_seconds_and_a_negative_for_less() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![7; 2000], vec![8; 3000]]);
    source.large.lock().unwrap().insert(a.files[0], 2000);
    let limits = Limits {
        locate_present_for: Duration::from_secs(10),
        locate_absent_for: Duration::from_secs(2),
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let (held, missing) = (a.files[0], a.files[1]);

    rig.locate(&token).await;
    rig.locate(&token).await;
    assert_eq!(
        (rig.source.heads_of(&held), rig.source.heads_of(&missing)),
        (1, 1)
    );

    // The missing blob is written; its negative answer is still good.
    rig.source.large.lock().unwrap().insert(missing, 3000);
    tokio::time::advance(Duration::from_secs(1)).await;
    let table_now = table(&rig.locate(&token).await);
    assert_eq!(table_now[&missing.to_string()]["absent"], true);

    tokio::time::advance(Duration::from_secs(2)).await;
    let table_later = table(&rig.locate(&token).await);
    assert!(table_later[&missing.to_string()]["large"].is_object());
    assert_eq!(
        (rig.source.heads_of(&held), rig.source.heads_of(&missing)),
        (1, 2)
    );

    // The held object's answer runs out later, and the object is gone by then.
    rig.source.large.lock().unwrap().remove(&held);
    tokio::time::advance(Duration::from_secs(8)).await;
    let table_last = table(&rig.locate(&token).await);
    assert_eq!(table_last[&held.to_string()]["absent"], true);
    assert_eq!(rig.source.heads_of(&held), 2);
}

/// Catches: a scope of more large blobs than the memory holds answered wrongly or partly: with
/// room for every check (the per-request cap is lifted), 4100 large blobs are each answered
/// large, once, and the lines are the scope and nothing else.
#[tokio::test]
async fn a_scope_of_many_large_blobs_is_answered_whole() {
    const MANY: usize = 4100;
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..MANY)
        .map(|i| format!("{i:01100}").into_bytes())
        .collect();
    let a = build(&source, "a", &contents);
    for file in &a.files {
        source.large.lock().unwrap().insert(*file, 1100);
    }
    let limits = Limits {
        locate_heads_per_request: MANY + 10,
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let table = table(&rig.locate(&token).await);
    assert!(
        a.files
            .iter()
            .all(|f| table[&f.to_string()]["large"].is_object())
    );
    let named: BTreeSet<String> = table.keys().cloned().collect();
    let expected: BTreeSet<String> = a.scope().iter().map(ToString::to_string).collect();
    assert_eq!(named, expected);
    // The files and the root Directory (which is itself over the size) are each asked about.
    assert_eq!(rig.source.heads.lock().unwrap().len(), MANY + 1);
    assert_eq!(table[&a.root.to_string()]["absent"], true);
}

/// Catches: a full memory that forgets everything (so that one big scope costs every other
/// Action its answers) or that keeps the oldest: with room for four answers, a scope of three
/// large blobs and then one of two leave the newest four, so the second Action asked again is
/// not checked again.
#[tokio::test]
async fn a_full_memory_drops_the_oldest_answer_and_keeps_the_rest() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![1; 2000], vec![2; 2001], vec![3; 2002]]);
    let b = build(&source, "b", &[vec![4; 2003], vec![5; 2004]]);
    for file in a.files.iter().chain(&b.files) {
        source.large.lock().unwrap().insert(*file, file.size_bytes);
    }
    let limits = Limits {
        locate_remembered: 4,
        ..small()
    };
    let rig = rig(source, limits);
    rig.locate(&rig.token(a.action)).await;
    rig.locate(&rig.token(b.action)).await;
    let heads = |rig: &Rig| rig.source.heads.lock().unwrap().len();
    assert_eq!(heads(&rig), 5);
    let again = table(&rig.locate(&rig.token(b.action)).await);
    assert_eq!(heads(&rig), 5, "the newest answers were kept");
    assert!(
        b.files
            .iter()
            .all(|f| again[&f.to_string()]["large"].is_object())
    );
    // The first Action asked again is checked once, for the one answer that was dropped, and
    // for no other: the answers it still had were taken from the memory before it changed.
    let first = table(&rig.locate(&rig.token(a.action)).await);
    assert!(
        a.files
            .iter()
            .all(|f| first[&f.to_string()]["large"].is_object())
    );
    assert_eq!(heads(&rig), 6);
    let twice = a
        .files
        .iter()
        .filter(|f| rig.source.heads_of(f) == 2)
        .count();
    assert_eq!(twice, 1, "only the oldest answer was lost");
}

/// Catches: presence checks that are limited for each request and not for all: two requests that
/// each name 20 large blobs, with a store that holds every check open, never have more than
/// [`HEADS_IN_FLIGHT`] open together, and all of them are answered once the store answers.
#[tokio::test]
async fn presence_checks_of_all_requests_share_the_places() {
    let source = Source {
        head_gate: Some(tokio::sync::Semaphore::new(0)),
        ..Source::default()
    };
    let one: Vec<Vec<u8>> = (0..20).map(|i| vec![i; 2000 + usize::from(i)]).collect();
    let two: Vec<Vec<u8>> = (0..20).map(|i| vec![i; 3000 + usize::from(i)]).collect();
    let a = build(&source, "a", &one);
    let b = build(&source, "b", &two);
    for file in a.files.iter().chain(&b.files) {
        source.large.lock().unwrap().insert(*file, file.size_bytes);
    }
    let rig = Arc::new(rig(source, small()));
    let ask = |action: Digest| {
        let rig = Arc::clone(&rig);
        tokio::spawn(async move { rig.locate(&rig.token(action)).await })
    };
    let (first, second) = (ask(a.action), ask(b.action));
    let open = |rig: &Rig| rig.source.head_open.lock().unwrap().0;
    while open(&rig) < HEADS_IN_FLIGHT {
        tokio::task::yield_now().await;
    }
    // Checks past the places would have started by now.
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(open(&rig), HEADS_IN_FLIGHT);
    rig.source.head_gate.as_ref().unwrap().add_permits(1000);
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!(rig.source.head_open.lock().unwrap().1, HEADS_IN_FLIGHT);
    for (reply, built) in [(first, &a), (second, &b)] {
        let table = table(&reply);
        assert!(
            built
                .files
                .iter()
                .all(|f| table[&f.to_string()]["large"].is_object())
        );
    }
}

/// Catches: a request that checks every large digest it names (a Directory can name a million
/// that do not exist): with a cap of three, six present large blobs are three `large` and three
/// `absent`, and tierstore was asked three times.
#[tokio::test]
async fn a_request_checks_at_most_the_cap_of_large_blobs() {
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..6).map(|i| vec![i; 2000 + usize::from(i)]).collect();
    let a = build(&source, "a", &contents);
    for file in &a.files {
        source.large.lock().unwrap().insert(*file, file.size_bytes);
    }
    let limits = Limits {
        locate_heads_per_request: 3,
        ..small()
    };
    let rig = rig(source, limits);
    let table = table(&rig.locate(&rig.token(a.action)).await);
    let large = table
        .values()
        .filter(|line| line["large"].is_object())
        .count();
    let absent = a
        .files
        .iter()
        .filter(|f| table[&f.to_string()]["absent"] == true)
        .count();
    assert_eq!((large, absent), (3, 3));
    assert_eq!(rig.source.heads.lock().unwrap().len(), 3);
}

/// Catches: a presence check with no deadline, which a store that never answers turns into a
/// request that never ends: the blob is `absent` once the check's time is up (on a paused
/// clock), the request is a 200, and the missing answer is not remembered (the next request
/// asks again).
#[tokio::test(start_paused = true)]
async fn a_check_that_is_not_answered_in_time_is_absent_and_not_remembered() {
    let source = Source::default();
    let a = build(&source, "a", &[vec![7; 2000], b"small".to_vec()]);
    pack(&source, &a.files[1], span("cas/f/w/1", 0, 5));
    source.large.lock().unwrap().insert(a.files[0], 2000);
    source.head_hang.store(true, Ordering::SeqCst);
    let limits = Limits {
        locate_head_timeout: Duration::from_secs(5),
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let first = table(&rig.locate(&token).await);
    assert_eq!(first[&a.files[0].to_string()]["absent"], true);
    assert!(first[&a.files[1].to_string()]["pack"].is_object());
    rig.source.head_hang.store(false, Ordering::SeqCst);
    let second = table(&rig.locate(&token).await);
    assert!(second[&a.files[0].to_string()]["large"].is_object());
    assert_eq!(rig.source.heads_of(&a.files[0]), 2);
}

/// Catches: a table built before its bytes are charged to the budget: with a budget of one
/// table, a second request waits (while the first reply is held) and goes on when the first is
/// dropped.
#[tokio::test]
async fn the_table_is_charged_to_the_budget_before_it_is_built() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    let limits = Limits {
        read_budget_bytes: 8 * 1024,
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let first = rig.locate(&token).await;
    assert_eq!(first.status, StatusCode::OK);
    let mut second = Box::pin(rig.locate(&token));
    assert!(
        futures::poll!(second.as_mut()).is_pending(),
        "the held table holds the budget"
    );
    drop(first);
    assert_eq!(second.await.status, StatusCode::OK);
}

/// Catches: a deadline that covers a check but not the wait for a place among all requests'
/// checks, so that a store that never answers keeps a request (and its reads place) for as long
/// as the queue takes: with one reads place, 40 large blobs and a store that never answers, the
/// request ends at its deadline on a paused clock with every large blob `absent`, only the
/// places' worth of checks ever started, a GET issued while it waits is served when the deadline
/// gives the place back, and
/// the next locate, with the store answering, finds the blobs (the stuck checks gave their
/// places back).
#[tokio::test(start_paused = true)]
async fn the_deadline_covers_the_wait_for_a_place_and_a_get_is_served_after() {
    let source = Source::default();
    let mut contents: Vec<Vec<u8>> = (0..40).map(|i| vec![i; 2000 + usize::from(i)]).collect();
    contents.push(b"small".to_vec());
    let a = build(&source, "a", &contents);
    for file in &a.files[..40] {
        source.large.lock().unwrap().insert(*file, file.size_bytes);
    }
    source.head_hang.store(true, Ordering::SeqCst);
    let limits = Limits {
        reads_in_flight: 1,
        locate_head_timeout: Duration::from_secs(5),
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let header = format!("Bearer {token}");
    let mut locate = Box::pin(rig.locate(&token));
    // Run the locate until its checks are stuck: it holds the one reads place.
    while rig.source.heads.lock().unwrap().len() < HEADS_IN_FLIGHT {
        assert!(futures::poll!(locate.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
    let path = format!("/{}", a.files[40]);
    let mut get = Box::pin(rig.read.respond(&Method::GET, &path, Some(&header)));
    assert!(
        futures::poll!(get.as_mut()).is_pending(),
        "the GET waits for the place the locate holds"
    );
    // The deadline frees the place: the locate ends with every large blob absent, only the
    // places' worth of checks ever started, and the GET is served.
    let reply = locate.await;
    let first = table(&reply);
    assert!(
        a.files[..40]
            .iter()
            .all(|f| first[&f.to_string()]["absent"] == true)
    );
    assert_eq!(rig.source.heads.lock().unwrap().len(), HEADS_IN_FLIGHT);
    drop(reply);
    let got = get.await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(&got.body[..], b"small");

    rig.source.head_hang.store(false, Ordering::SeqCst);
    let again = table(&rig.locate(&token).await);
    assert!(
        a.files[..40]
            .iter()
            .all(|f| again[&f.to_string()]["large"].is_object())
    );
}

/// Catches: an index ask that names every digest of a scope (a Directory can name a million):
/// with a cap of three, five packed blobs are three pack entries and two `absent`, and the index
/// was asked about three.
#[tokio::test]
async fn a_request_asks_the_index_about_at_most_the_cap() {
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..5).map(|i| vec![i; 10 + usize::from(i)]).collect();
    let a = build(&source, "a", &contents);
    // Every digest of the scope has a span, so which three are asked about does not matter.
    for digest in a.scope() {
        pack(&source, &digest, span("cas/f/w/1", 0, digest.size_bytes));
    }
    let limits = Limits {
        locate_packed_per_request: 3,
        ..small()
    };
    let rig = rig(source, limits);
    let table = table(&rig.locate(&rig.token(a.action)).await);
    let packed = table.values().filter(|l| l["pack"].is_object()).count();
    let absent = table.values().filter(|l| l["absent"] == true).count();
    // The Action, Command, root, subdirectory and deep file, and five files.
    assert_eq!((table.len(), packed, absent), (10, 3, 7));
    let asked = rig.source.asked();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].len(), 3);
}

/// Catches: an index that never answers holding a reads place for ever: on a paused clock the
/// locate is a 503 `store_unavailable` at its deadline, a GET issued meanwhile is served once
/// the deadline frees the place, and a locate after the index is back is answered.
#[tokio::test(start_paused = true)]
async fn an_index_that_never_answers_is_unavailable_at_the_deadline_and_frees_the_place() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    source.index_hang.store(true, Ordering::SeqCst);
    let limits = Limits {
        reads_in_flight: 1,
        locate_head_timeout: Duration::from_secs(5),
        ..small()
    };
    let rig = rig(source, limits);
    let token = rig.token(a.action);
    let header = format!("Bearer {token}");
    let mut locate = Box::pin(rig.locate(&token));
    while rig.source.asked().is_empty() {
        assert!(futures::poll!(locate.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
    let path = format!("/{}", a.files[0]);
    let mut get = Box::pin(rig.read.respond(&Method::GET, &path, Some(&header)));
    assert!(futures::poll!(get.as_mut()).is_pending());
    let refused = locate.await;
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(&refused.body[..], b"store_unavailable\n");
    assert_eq!(get.await.status, StatusCode::OK);
    rig.source.index_hang.store(false, Ordering::SeqCst);
    assert_eq!(rig.locate(&token).await.status, StatusCode::OK);
}
