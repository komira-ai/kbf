//! The stop signal, against listeners the tests bind.

use futures::FutureExt as _;

use super::*;

/// Asserts that a stop does not complete before it is fired, completes for every waiter (a
/// clone, a waiter spawned earlier, a late waiter) once it is fired, and is not lost when
/// fired before anything waits.
#[tokio::test]
async fn a_stop_completes_for_every_waiter_once_it_is_fired() {
    let stop = Stop::new();
    let clone = stop.clone();
    assert!(stop.fired().now_or_never().is_none(), "fired before it was");
    let waiting = tokio::spawn(stop.fired());
    clone.fire();
    waiting.await.expect("the waiter completes");
    assert!(stop.fired().now_or_never().is_some(), "a late waiter waits");
    assert!(clone.fired().now_or_never().is_some());
}

/// Asserts that firing a stop closes the registered listeners at once (their address
/// refuses a connection), and that a listener registered after the stop was given is
/// closed at once.
#[tokio::test]
async fn a_stop_closes_the_listeners_registered_with_it_when_it_is_given() {
    let any = || "127.0.0.1:0".parse().expect("address");
    let stop = Stop::new();
    let socket = crate::listen::Socket::bind(any(), &crate::listen::Binder::new()).expect("bind");
    let addr = socket.local;
    let (incoming, latch) = socket.grpc(None).expect("registered");
    stop.closes(latch);
    assert!(
        std::net::TcpStream::connect(addr).is_ok(),
        "closed before the stop"
    );
    stop.fire();
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "still open after the stop"
    );
    drop(incoming);

    // Registered after the stop was given: closed at once.
    let socket = crate::listen::Socket::bind(any(), &crate::listen::Binder::new()).expect("bind");
    let addr = socket.local;
    let (_incoming, latch) = socket.grpc(None).expect("registered");
    stop.closes(latch);
    assert!(
        std::net::TcpStream::connect(addr).is_err(),
        "a late listener stays open"
    );
}
