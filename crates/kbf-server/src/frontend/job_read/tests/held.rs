//! A response whose bytes are charged to the read budget, sent through the listener's own
//! `response`: the budget stays taken until the body is consumed or dropped.

use std::sync::Arc;

use axum::http::Method;
use kbf_front::grants::{GrantKey, Grants};
use kbf_front::job_read::{JobRead, Limits};

use super::SECRET;
use super::stream::large;
use crate::frontend::job_read::response;

/// Catches: a response that gives the budget back before its body is sent (the reply's hold
/// dropped when `response` returns, while a slow client still has the bytes to read): with a
/// budget that holds one blob of 10 KiB, a second read of it waits while the first response's
/// body is unread, goes on when that body is consumed, and the same when a body is dropped
/// without being read.
#[tokio::test]
async fn a_response_keeps_the_budget_until_its_body_is_consumed_or_dropped() {
    let large = large();
    let limits = Limits {
        read_budget_bytes: 12 * 1024,
        ..Limits::default()
    };
    let key = GrantKey::from_secret(SECRET).expect("the secret is long enough");
    let read = JobRead::new(large.source(false), Arc::new(Grants::system(key)), limits);
    let header = format!("Bearer {}", large.token());
    let path = format!("/{}", large.big);
    let ask = || read.respond(&Method::GET, &path, Some(&header));

    let first = response(ask().await, "GET");
    assert_eq!(first.status(), 200);
    let mut second = Box::pin(ask());
    assert!(
        futures::poll!(second.as_mut()).is_pending(),
        "the unread body holds the budget"
    );
    let body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("the body");
    assert_eq!(body, large.bytes);

    let held = response(second.await, "GET");
    let mut third = Box::pin(ask());
    assert!(futures::poll!(third.as_mut()).is_pending());
    drop(held);
    assert_eq!(third.await.status, 200);
}
