//! Which endpoint answers a call, what a failure does to the endpoint, and what the poll of
//! `/readyz` does. Time is the test clock, which a test moves; nothing waits.

use std::sync::Arc;

use tokio::sync::Notify;

use crate::{Backend, ByteRange, Error, Op};

use super::Passed;
use super::testing::{BACKOFF_MS, Fleet};

const BODY: &[u8] = b"0123456789abcdefghij";

/// A fleet of `count` endpoints that holds `key`.
async fn holding(count: usize, key: &str) -> Fleet {
    let fleet = Fleet::new(count);
    fleet.put(key, BODY).await;
    fleet
}

/// The failovers counted for each reason: failed, not ready, backed off.
fn counted(fleet: &Fleet) -> [u64; 3] {
    let endpoints = &fleet.endpoints;
    [
        endpoints.failovers(Passed::RequestFailed),
        endpoints.failovers(Passed::NotReady),
        endpoints.failovers(Passed::Backoff),
    ]
}

/// Asserts that a read goes to the first endpoint of the key's order and to no other while it
/// answers, so the ranged reads of one pack meet one endpoint, and that nothing is counted.
#[tokio::test]
async fn a_read_goes_to_the_first_endpoint_of_its_order() {
    let key = "cas/b/pack";
    let fleet = holding(3, key).await;
    let first = fleet.order(key)[0];
    let range = ByteRange::new(2, 5);
    for _ in 0..3 {
        let part = fleet.endpoints.get(key, Some(range)).await.unwrap();
        assert_eq!(&part[..], &BODY[2..7]);
    }
    let mut want = vec![0, 0, 0];
    want[first] = 3;
    assert_eq!(fleet.all_gets(), want);
    assert_eq!(counted(&fleet), [0, 0, 0]);
    assert_eq!(fleet.endpoints.unavailable(), 0);
}

/// Asserts that when the first endpoint of the order is down the second answers, the read
/// succeeds, one failover is counted as a failed request, and the third is not asked.
#[tokio::test]
async fn the_next_endpoint_answers_when_the_first_is_down() {
    let key = "cas/b/pack";
    let fleet = holding(3, key).await;
    let order = fleet.order(key);
    fleet.faulty[order[0]].set_down(true);
    let body = fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(&body[..], BODY);
    assert_eq!(fleet.gets(order[0]), 1);
    assert_eq!(fleet.gets(order[1]), 1);
    assert_eq!(fleet.gets(order[2]), 0);
    assert_eq!(counted(&fleet), [1, 0, 0]);
    assert_eq!(fleet.endpoints.unavailable(), 0);
}

/// Asserts that a 5xx (which the S3 backend reports as `Unavailable`) fails over as a refused
/// connection does, for a call that fails once: the endpoint answers the next call.
#[tokio::test]
async fn a_5xx_fails_over() {
    let key = "cas/b/pack";
    let fleet = holding(2, key).await;
    let order = fleet.order(key);
    let error = Error::Unavailable("the store answered 503".to_owned());
    fleet.faulty[order[0]].fail_next_with(Op::Get, error);
    let body = fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(&body[..], BODY);
    assert_eq!(counted(&fleet), [1, 0, 0]);
}

/// Asserts that a 404 is the answer: the first endpoint's `NotFound` goes back to the caller,
/// no second endpoint is asked, nothing is counted and the endpoint is not backed off. A head
/// does the same.
#[tokio::test]
async fn a_404_is_final() {
    let fleet = Fleet::new(3);
    let key = "cas/b/absent";
    let first = fleet.order(key)[0];
    let error = fleet.endpoints.get(key, None).await.unwrap_err();
    assert_eq!(error, Error::NotFound(key.to_owned()));
    let error = fleet.endpoints.head(key).await.unwrap_err();
    assert_eq!(error, Error::NotFound(key.to_owned()));
    let mut want = vec![0, 0, 0];
    want[first] = 1;
    assert_eq!(fleet.all_gets(), want);
    assert_eq!(fleet.faulty[first].calls(Op::Head), 2);
    assert_eq!(counted(&fleet), [0, 0, 0]);
    assert_eq!(fleet.endpoints.unavailable(), 0);
    assert!(fleet.endpoints.is_ready());
}

/// Asserts that an answer that is not a failure of the endpoint goes back as it is and asks no
/// second endpoint: a refusal of the request, a store that is full, a range past the end.
#[tokio::test]
async fn an_answer_that_is_not_a_failure_is_not_failed_over() {
    let key = "cas/b/pack";
    let answers = [
        Error::Other("the store answered 403".to_owned()),
        Error::Full("507".to_owned()),
        Error::Reserved(key.to_owned()),
        Error::Unsupported("501".to_owned()),
    ];
    for answer in answers {
        let fleet = holding(2, key).await;
        let first = fleet.order(key)[0];
        fleet.faulty[first].fail_next_with(Op::Get, answer.clone());
        let got = fleet.endpoints.get(key, None).await.unwrap_err();
        assert_eq!(got, answer);
        assert_eq!(fleet.gets(first), 1);
        assert_eq!(fleet.all_gets().iter().sum::<u64>(), 1);
        assert_eq!(counted(&fleet), [0, 0, 0]);
    }
}

/// Asserts that an endpoint that failed is skipped by the next calls until `unready_backoff` has
/// passed on the clock, to the millisecond, and is asked again at that moment: the call at one
/// millisecond short of it does not reach the endpoint, the call at it does. Each call that
/// goes past it counts a backed-off failover.
#[tokio::test]
async fn a_failed_endpoint_is_skipped_until_the_backoff_has_passed() {
    let key = "cas/b/pack";
    let fleet = holding(2, key).await;
    let order = fleet.order(key);
    let (first, second) = (order[0], order[1]);
    fleet.faulty[first].set_down(true);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 1);
    fleet.faulty[first].set_down(false);

    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 1, "skipped while backed off");
    assert_eq!(fleet.gets(second), 2);
    assert_eq!(counted(&fleet), [1, 0, 1]);

    fleet.clock.advance(BACKOFF_MS - 1);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 1, "one millisecond short of the backoff");
    assert_eq!(counted(&fleet), [1, 0, 2]);

    fleet.clock.advance(1);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 2, "asked again when the backoff is over");
    assert_eq!(counted(&fleet), [1, 0, 2]);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(
        fleet.gets(first),
        3,
        "and it answers, so it keeps its place"
    );
    assert_eq!(fleet.gets(second), 3);
}

/// Asserts that an endpoint that fails again after its backoff is backed off for a whole
/// backoff from that failure, not from the first.
#[tokio::test]
async fn a_second_failure_backs_off_from_its_own_time() {
    let key = "cas/b/pack";
    let fleet = holding(2, key).await;
    let first = fleet.order(key)[0];
    fleet.faulty[first].set_down(true);
    fleet.endpoints.get(key, None).await.unwrap();
    fleet.clock.advance(BACKOFF_MS);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 2);
    fleet.clock.advance(BACKOFF_MS - 1);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(
        fleet.gets(first),
        2,
        "the second failure has its own backoff"
    );
    fleet.clock.advance(1);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.gets(first), 3);
}

/// Asserts that with every endpoint down an uncached call fails with the error of the last
/// endpoint, each endpoint is asked once, the call counts as unavailable and no failover; that
/// the next call asks none (all are backed off) and says why; that the fleet is not ready; and
/// that it is ready again when the backoff has passed.
#[tokio::test]
async fn with_every_endpoint_down_the_call_is_unavailable() {
    let key = "cas/b/pack";
    let fleet = holding(3, key).await;
    for faulty in &fleet.faulty {
        faulty.set_down(true);
    }
    let error = fleet.endpoints.get(key, None).await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert_eq!(fleet.all_gets(), [1, 1, 1]);
    assert_eq!(fleet.endpoints.unavailable(), 1);
    assert_eq!(counted(&fleet), [0, 0, 0]);
    assert!(!fleet.endpoints.is_ready());

    let error = fleet.endpoints.get(key, None).await.unwrap_err();
    assert_eq!(
        error,
        Error::Unavailable("no upstream endpoint is ready".to_owned())
    );
    assert_eq!(fleet.all_gets(), [1, 1, 1]);
    assert_eq!(fleet.endpoints.unavailable(), 2);

    fleet.clock.advance(BACKOFF_MS);
    assert!(fleet.endpoints.is_ready());
}

/// Asserts what the poll of `/readyz` does: an endpoint that says it is not ready is skipped
/// without a request and a not-ready failover is counted, the endpoint is ready again after a
/// poll that says so, the fleet is ready while one endpoint is, and not ready when none is.
#[tokio::test]
async fn an_endpoint_that_is_not_ready_is_skipped() {
    let fleet = Fleet::new(3);
    let key = fleet.key_first_at(0);
    fleet.put(&key, BODY).await;
    let second = fleet.order(&key)[1];

    fleet.probe.set(&fleet.names[0], false);
    fleet.endpoints.poll().await;
    assert!(fleet.endpoints.is_ready());
    fleet.endpoints.get(&key, None).await.unwrap();
    assert_eq!(fleet.gets(0), 0);
    assert_eq!(fleet.gets(second), 1);
    assert_eq!(counted(&fleet), [0, 1, 0]);

    fleet.probe.set(&fleet.names[0], true);
    fleet.endpoints.poll().await;
    fleet.endpoints.get(&key, None).await.unwrap();
    assert_eq!(fleet.gets(0), 1);
    assert_eq!(counted(&fleet), [0, 1, 0]);

    for name in &fleet.names {
        fleet.probe.set(name, false);
    }
    fleet.endpoints.poll().await;
    assert!(!fleet.endpoints.is_ready());
    let error = fleet.endpoints.get(&key, None).await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert_eq!(fleet.all_gets().iter().sum::<u64>(), 2, "nothing was asked");
    assert_eq!(fleet.endpoints.unavailable(), 1);
    assert_eq!(counted(&fleet), [0, 1, 0]);

    fleet.probe.set(&fleet.names[1], true);
    fleet.endpoints.poll().await;
    assert!(fleet.endpoints.is_ready());
}

/// Asserts that a poll that says ready does not clear a backoff (the endpoint failed a request
/// just now), and that a failed request does not make the endpoint not ready for the poll's
/// sake: the two states are told apart, and the fleet is ready only when a call would try one.
#[tokio::test]
async fn a_ready_poll_does_not_end_a_backoff() {
    let key = "cas/b/pack";
    let fleet = holding(1, key).await;
    fleet.faulty[0].set_down(true);
    assert!(fleet.endpoints.get(key, None).await.is_err());
    fleet.faulty[0].set_down(false);
    fleet.endpoints.poll().await;
    assert!(!fleet.endpoints.is_ready());
    assert!(fleet.endpoints.get(key, None).await.is_err());
    assert_eq!(fleet.gets(0), 1);
    fleet.clock.advance(BACKOFF_MS);
    assert!(fleet.endpoints.is_ready());
    assert_eq!(&fleet.endpoints.get(key, None).await.unwrap()[..], BODY);
}

/// Asserts that every read call fails over and gives the answer of the endpoint that serves it:
/// a get, a head, a listing, a page of a listing and a listing of uploads, with the first
/// endpoint of their order down. The first call counts a failed request and the others, which
/// find the endpoint backed off, count a backoff.
#[tokio::test]
async fn every_read_call_fails_over() {
    let key = "cas/x";
    let fleet = holding(3, key).await;
    let upload = fleet.store.create_upload("cas/x-upload").await.unwrap();
    let first = fleet.order(key)[0];
    fleet.faulty[first].set_down(true);
    let endpoints = &fleet.endpoints;

    assert_eq!(&endpoints.get(key, None).await.unwrap()[..], BODY);
    assert_eq!(endpoints.head(key).await.unwrap().size, BODY.len() as u64);
    let listed = endpoints.list(key).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);
    let page = endpoints.list_page(key, None, 10).await.unwrap();
    assert_eq!(page, listed);
    let none = endpoints.list_page(key, Some(key), 10).await.unwrap();
    assert!(none.is_empty());
    let uploads = endpoints.list_uploads(key, None, 10).await.unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].id, upload);
    assert_eq!(counted(&fleet), [1, 0, 5]);
}

/// Asserts that the three write calls are refused as unsupported and reach no endpoint.
#[tokio::test]
async fn the_writes_are_unsupported_and_reach_no_endpoint() {
    let fleet = Fleet::new(2);
    let endpoints = &fleet.endpoints;
    let body = bytes::Bytes::from_static(b"x");
    let put = endpoints.put("cas/b/new", body.clone()).await;
    assert!(matches!(put, Err(Error::Unsupported(_))), "{put:?}");
    let replace = endpoints.put_if_match("cas/b/new", body, "tag").await;
    assert!(matches!(replace, Err(Error::Unsupported(_))), "{replace:?}");
    let delete = endpoints.delete("cas/b/new").await;
    assert!(matches!(delete, Err(Error::Unsupported(_))), "{delete:?}");
    for faulty in &fleet.faulty {
        for op in [Op::Put, Op::PutIfMatch, Op::Delete] {
            assert_eq!(faulty.calls(op), 0);
        }
    }
}

/// Asserts the text added to `/metricz`, whole: the failovers by reason, the calls that found
/// no endpoint, and the ready gauge of each endpoint, which is 0 for one that is not ready, and
/// for one that is backed off, until its backoff has passed.
#[tokio::test]
async fn the_metrics_show_failovers_by_reason_and_the_ready_gauges() {
    let key = "cas/b/pack";
    let fleet = holding(2, key).await;
    let first = fleet.order(key)[0];
    let a = &fleet.names[first];
    let metrics = |failed: u64, down: u64, backed: u64, none: u64, ready: [u8; 2]| {
        format!(
            "tierstore_cache_upstream_failovers_total{{reason=\"request_failed\"}} {failed}\n\
             tierstore_cache_upstream_failovers_total{{reason=\"not_ready\"}} {down}\n\
             tierstore_cache_upstream_failovers_total{{reason=\"backoff\"}} {backed}\n\
             tierstore_cache_upstream_unavailable_total {none}\n\
             tierstore_cache_upstream_endpoint_ready{{endpoint=\"{}\"}} {}\n\
             tierstore_cache_upstream_endpoint_ready{{endpoint=\"{}\"}} {}\n",
            fleet.names[0], ready[0], fleet.names[1], ready[1],
        )
    };
    let ready = |first_ready: u8| {
        let mut ready = [1, 1];
        ready[first] = first_ready;
        ready
    };
    assert_eq!(fleet.endpoints.metrics(), metrics(0, 0, 0, 0, [1, 1]));

    fleet.faulty[first].set_down(true);
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.endpoints.metrics(), metrics(1, 0, 0, 0, ready(0)));
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.endpoints.metrics(), metrics(1, 0, 1, 0, ready(0)));

    fleet.clock.advance(BACKOFF_MS);
    fleet.probe.set(a, false);
    fleet.endpoints.poll().await;
    fleet.endpoints.get(key, None).await.unwrap();
    assert_eq!(fleet.endpoints.metrics(), metrics(1, 1, 1, 0, ready(0)));

    fleet.probe.set(a, true);
    fleet.endpoints.poll().await;
    assert_eq!(fleet.endpoints.metrics(), metrics(1, 1, 1, 0, [1, 1]));
}

/// Asserts that the poll runs every interval until it is told to stop: the first round is after
/// one interval, an endpoint that stops being ready is seen in the next round and one that
/// returns is seen in the one after, and the loop ends when it is notified, at once. Time is the
/// paused clock of the runtime, which moves on when nothing else can run.
#[tokio::test(start_paused = true)]
async fn the_poll_runs_every_interval_until_halted() {
    let fleet = Fleet::new(2);
    let halt = Arc::new(Notify::new());
    let endpoints = Arc::clone(&fleet.endpoints);
    let stop = Arc::clone(&halt);
    let watching = tokio::spawn(async move { endpoints.watch(stop).await });
    let name = fleet.names[0].clone();
    let line = |ready: u8| format!("endpoint_ready{{endpoint=\"{name}\"}} {ready}\n");

    assert!(fleet.endpoints.metrics().contains(&line(1)));
    fleet.probe.set(&name, false);
    fleet.probe.asked.acquire_many(2).await.unwrap().forget();
    assert!(fleet.endpoints.metrics().contains(&line(0)));
    fleet.probe.set(&name, true);
    fleet.probe.asked.acquire_many(2).await.unwrap().forget();
    assert!(fleet.endpoints.metrics().contains(&line(1)));

    halt.notify_one();
    watching.await.unwrap();
    assert_eq!(fleet.probe.asked.available_permits(), 0);
}

/// Asserts that a halt that arrives before the loop first looks for it ends the loop without a
/// round: the permit of `notify_one` waits for the loop.
#[tokio::test(start_paused = true)]
async fn a_halt_before_the_first_round_ends_the_poll() {
    let fleet = Fleet::new(2);
    let halt = Arc::new(Notify::new());
    halt.notify_one();
    fleet.endpoints.watch(halt).await;
    assert_eq!(fleet.probe.asked.available_permits(), 0);
}
