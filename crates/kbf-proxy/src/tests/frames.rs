//! How a request's end reaches the child: END_STREAM rides on the last DATA frame (or on the
//! HEADERS of a request with no body), and no empty DATA frame is sent. The child's h2
//! GOAWAYs a connection after about a hundred empty DATA frames that do not end a stream, and
//! the frontend turns each separate empty END it gets into one of those for its own node.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use h2::client::SendRequest;
use http::{HeaderValue, Request, Response};
use tokio::task::JoinSet;

use super::kit::{
    CAPS, DATA, HEADERS, Rig, Seen, WRITE, finish, grpc_ok, next_of, read_all, script, tagged,
    until,
};

/// `(kind, ends, len)` of each recorded frame.
fn shape(frames: &[Seen]) -> Vec<(u8, bool, usize)> {
    frames.iter().map(|f| (f.kind, f.ends(), f.len)).collect()
}

async fn recording_rig() -> Rig {
    Rig::start(tagged("a"), tagged("b")).await
}

#[tokio::test(start_paused = true)]
async fn a_request_of_n_chunks_arrives_as_n_data_frames_the_last_one_ending_the_stream() {
    let rig = recording_rig().await;
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, mut body) = client.start(WRITE, false).await;
    body.send_data(Bytes::from_static(b"one"), false)
        .expect("1");
    body.send_data(Bytes::from_static(b"second"), false)
        .expect("2");
    body.send_data(Bytes::from_static(b"third!"), true)
        .expect("3");
    until(|| rig.a.state.recorded_end()).await;
    assert_eq!(
        shape(&rig.a.state.recorded()),
        vec![
            (HEADERS, false, rig.a.state.recorded()[0].len),
            (DATA, false, 3),
            (DATA, false, 6),
            (DATA, true, 6),
        ],
        "END_STREAM on the last chunk, no empty frame"
    );
}

#[tokio::test(start_paused = true)]
async fn a_client_that_ends_with_a_separate_empty_frame_costs_at_most_one_empty_frame() {
    let rig = recording_rig().await;
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, mut body) = client.start(WRITE, false).await;
    body.send_data(Bytes::from_static(b"one"), false)
        .expect("1");
    body.send_data(Bytes::from_static(b"two"), false)
        .expect("2");
    until(|| rig.a.state.recorded().len() == 3).await;
    body.send_data(Bytes::new(), true).expect("end");
    until(|| rig.a.state.recorded_end()).await;
    let frames = rig.a.state.recorded();
    let data: Vec<_> = frames.iter().filter(|f| f.kind == DATA).collect();
    assert_eq!(data.iter().filter(|f| f.ends()).count(), 1, "one END");
    assert!(
        data.last().expect("data").ends(),
        "and it is the last frame"
    );
    assert!(
        data.iter().filter(|f| f.len == 0).count() <= 1,
        "at most one empty frame: {frames:?}"
    );
    assert_eq!(
        data.iter().map(|f| f.len).sum::<usize>(),
        6,
        "every byte arrived"
    );
}

#[tokio::test(start_paused = true)]
async fn an_end_that_is_already_queued_rides_on_the_last_data_frame() {
    let rig = recording_rig().await;
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, mut body) = client.start(WRITE, false).await;
    // Written with no await between: the proxy reads all three at once.
    body.send_data(Bytes::from_static(b"one"), false)
        .expect("1");
    body.send_data(Bytes::from_static(b"two"), false)
        .expect("2");
    body.send_data(Bytes::new(), true).expect("end");
    until(|| rig.a.state.recorded_end()).await;
    let frames = rig.a.state.recorded();
    assert_eq!(
        shape(&frames)[1..],
        [(DATA, false, 3), (DATA, true, 3)],
        "no empty END frame: {frames:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_bodiless_request_ends_on_its_headers() {
    let rig = recording_rig().await;
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, _body) = client.start(CAPS, true).await;
    until(|| rig.a.state.recorded_end()).await;
    let frames = rig.a.state.recorded();
    assert_eq!(frames.len(), 1, "HEADERS alone: {frames:?}");
    assert!(frames[0].kind == HEADERS && frames[0].ends());
}

#[tokio::test(start_paused = true)]
async fn a_request_with_trailers_passes_them_as_trailers_without_an_empty_frame() {
    let rig = recording_rig().await;
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, mut body) = client.start(WRITE, false).await;
    body.send_data(Bytes::from_static(b"msg"), false)
        .expect("data");
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-sum", HeaderValue::from_static("1"));
    body.send_trailers(trailers).expect("trailers");
    until(|| rig.a.state.recorded_end()).await;
    let frames = rig.a.state.recorded();
    assert_eq!(
        shape(&frames)[1..],
        [(DATA, false, 3), (HEADERS, true, frames[2].len)],
        "the data, then the trailers as the ending HEADERS"
    );
}

#[tokio::test(start_paused = true)]
async fn a_redispatched_request_also_ends_on_its_last_frame() {
    let rig = recording_rig().await;
    // The first connection answers the stream with GOAWAY(last = 0); the retry goes to the
    // second, which records.
    rig.a.state.raw_goaway_first.store(true, Ordering::Release);
    rig.a.state.raw_record.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let (_response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"one"), false)
        .expect("1");
    body.send_data(Bytes::from_static(b"two!"), false)
        .expect("2");
    body.send_data(Bytes::from_static(b"three"), true)
        .expect("3");
    until(|| rig.a.state.recorded_end()).await;
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    let frames = rig.a.state.recorded();
    assert_eq!(
        shape(&frames)[1..],
        [(DATA, false, 3), (DATA, false, 4), (DATA, true, 5)],
        "replayed, ending on the last frame: {frames:?}"
    );
}

/// The frontend of the report: every DATA frame it receives, empty or not, it forwards to a
/// node as a non-END DATA frame, and ends the node's stream afterwards. The node is a plain
/// h2 server with h2's default limits.
fn forwarding_frontend(
    node: SendRequest<Bytes>,
    failures: Arc<std::sync::atomic::AtomicUsize>,
) -> super::kit::Script {
    script(move |mut call| {
        let node = node.clone();
        let failures = Arc::clone(&failures);
        async move {
            let forwarded = async {
                let request = Request::builder()
                    .method("POST")
                    .uri("http://node.test/write")
                    .body(())
                    .expect("a request");
                let mut node = node.ready().await?;
                let (response, mut out) = node.send_request(request, false)?;
                while let Some(Ok(chunk)) = call.body.data().await {
                    let _ = call.body.flow_control().release_capacity(chunk.len());
                    if !chunk.is_empty() {
                        out.reserve_capacity(chunk.len());
                        std::future::poll_fn(|cx| out.poll_capacity(cx)).await;
                    }
                    out.send_data(chunk, false)?;
                }
                out.send_data(Bytes::new(), true)?;
                response.await.map(|_| ())
            };
            match forwarded.await {
                Ok(()) => grpc_ok(&mut call.respond, "ok"),
                Err(_) => {
                    failures.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
    })
}

#[tokio::test(start_paused = true)]
async fn many_concurrent_uploads_never_get_the_upstream_link_goawayed() {
    // The node: h2's default limits, one connection for every stream.
    let (near, far) = tokio::io::duplex(1 << 20);
    let node_error = Arc::new(Mutex::new(None::<String>));
    {
        let node_error = Arc::clone(&node_error);
        tokio::spawn(async move {
            let mut conn = h2::server::handshake(far).await.expect("node handshake");
            while let Some(accepted) = conn.accept().await {
                match accepted {
                    Ok((request, mut respond)) => {
                        tokio::spawn(async move {
                            let mut body = request.into_body();
                            read_all(&mut body).await;
                            let response = Response::builder().status(200).body(()).expect("ok");
                            let _ = respond.send_response(response, true);
                        });
                    }
                    Err(error) => {
                        *node_error.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(error.to_string());
                        return;
                    }
                }
            }
        });
    }
    let (node, connection) = h2::client::handshake(near).await.expect("client handshake");
    let link_down = Arc::new(Mutex::new(None::<String>));
    {
        let link_down = Arc::clone(&link_down);
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                *link_down.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
            }
        });
    }
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rig = Rig::start(
        forwarding_frontend(node, Arc::clone(&failures)),
        tagged("b"),
    )
    .await;
    let mut client = rig.client().await;
    let mut open = JoinSet::new();
    for _ in 0..150 {
        let (response, mut body) = client.start(WRITE, false).await;
        // Over h2's 256-byte small-frame threshold, so only the empty frames can exhaust
        // the node's budgets.
        body.send_data(Bytes::from(vec![1_u8; 300]), false)
            .expect("1");
        body.send_data(Bytes::from(vec![2_u8; 400]), false)
            .expect("2");
        body.send_data(Bytes::from(vec![3_u8; 500]), true)
            .expect("3");
        open.spawn(finish(response));
    }
    let mut done = 0;
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.status, Some(200), "{reply:?}");
        assert_eq!(reply.grpc_status().as_deref(), Some("0"), "{reply:?}");
        done += 1;
    }
    assert_eq!(done, 150);
    assert_eq!(failures.load(Ordering::Acquire), 0, "no forward failed");
    assert_eq!(
        *node_error.lock().unwrap_or_else(PoisonError::into_inner),
        None,
        "the node link was never closed (ENHANCE_YOUR_CALM)"
    );
    assert_eq!(
        *link_down.lock().unwrap_or_else(PoisonError::into_inner),
        None
    );
}
