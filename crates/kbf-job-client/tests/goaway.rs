//! A server that closes the connection with a GOAWAY `ENHANCE_YOUR_CALM`, as an HTTP/2 server
//! does when its client has reset more streams than its guard allows. A call open on that
//! connection loses its answer when the server closes it (the broken pipe of a transport
//! error, which may have applied); a call the server had not taken when the GOAWAY came reads
//! `RESOURCE_EXHAUSTED`, tonic's name for that reason. Neither says anything of the call, and
//! the client's next call is answered on a new connection: the channel dials one by itself, so
//! a client is never left on a connection the server has closed.
//!
//! The server is h2's own on a loopback port the system picks, since hyper's server sends a
//! GOAWAY only with `NO_ERROR`. Its first connection takes one call and closes; every later one
//! answers each `GetJob` with a job.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::{BufMut as _, Bytes, BytesMut};
use h2::Reason;
use js_proto::v1::{GetJobResponse, Job};
use kbf_job_client::{Client, Config, Error, Token};
use prost::Message as _;
use tokio::net::{TcpListener, TcpStream};

/// The id of the job every answer holds.
const JOB: &str = "job-1";

/// `message` as one gRPC frame: not compressed, its length, its bytes.
fn framed(message: &[u8]) -> Bytes {
    let mut frame = BytesMut::with_capacity(5 + message.len());
    frame.put_u8(0);
    frame.put_u32(u32::try_from(message.len()).unwrap());
    frame.put_slice(message);
    frame.freeze()
}

/// Serves one connection: the first takes a call and closes with GOAWAY `ENHANCE_YOUR_CALM`
/// with that call open; any other answers every call with the job [`JOB`].
async fn serve(tcp: TcpStream, first: bool) {
    let mut connection = h2::server::handshake(tcp).await.unwrap();
    if first {
        let Some(Ok((request, _respond))) = connection.accept().await else {
            return;
        };
        // The whole request is read, so the call is open and waiting for its answer.
        let mut body = request.into_body();
        let read = async { while let Some(Ok(_)) = body.data().await {} };
        tokio::select! {
            () = read => {}
            _ = std::future::poll_fn(|cx| connection.poll_closed(cx)) => return,
        }
        connection.abrupt_shutdown(Reason::ENHANCE_YOUR_CALM);
        let _ = std::future::poll_fn(|cx| connection.poll_closed(cx)).await;
        return;
    }
    while let Some(Ok((_request, mut respond))) = connection.accept().await {
        let answer = GetJobResponse {
            job: Some(Job {
                id: JOB.to_owned(),
                ..Job::default()
            }),
        };
        let head = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .unwrap();
        let mut body = respond.send_response(head, false).unwrap();
        body.send_data(framed(&answer.encode_to_vec()), false)
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        body.send_trailers(trailers).unwrap();
    }
}

/// The server on a loopback port, and the count of connections it accepted.
async fn server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let first = counted.fetch_add(1, Ordering::SeqCst) == 0;
            tokio::spawn(serve(tcp, first));
        }
    });
    (origin, accepted)
}

/// Catches a call cut by a GOAWAY `ENHANCE_YOUR_CALM` read as an answer, and a client left on
/// the connection the server closed: the call open on it fails as a lost answer, and the next
/// call is answered with the job on a second connection.
#[tokio::test]
async fn after_a_goaway_enhance_your_calm_the_next_call_is_answered_on_a_new_connection() {
    let (origin, accepted) = server().await;
    let client = Client::new(Config::new(
        origin.parse().unwrap(),
        Token::new("t").unwrap(),
    ))
    .unwrap();
    let cut = client
        .get_job(JOB)
        .await
        .expect_err("the connection was closed");
    assert!(matches!(cut, Error::Transport(_)), "{cut:?}");
    assert!(cut.may_have_applied(), "{cut}");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    let job = client.get_job(JOB).await.unwrap();
    assert_eq!(job.id, JOB);
    assert_eq!(accepted.load(Ordering::SeqCst), 2, "a new connection");
}
