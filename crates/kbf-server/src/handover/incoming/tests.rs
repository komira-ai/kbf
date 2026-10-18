//! A successor asking a frontend that serves to hand over, against a holder the test
//! plays.

use std::future::Future;
use std::path::PathBuf;

use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use super::*;
use crate::handover::tests::Scratch;

fn hello() -> Message {
    Message::Hello {
        version: crate::handover::wire::PROTOCOL,
        instance: Some(crate::handover::Instance::B),
        reapi: "127.0.0.1:1".parse().expect("address"),
        state: PathBuf::from("/state.db"),
    }
}

/// A holder at `path` that plays `script` with the first connection.
fn holder<F, Fut>(path: &Path, script: F) -> JoinHandle<()>
where
    F: FnOnce(Link) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = UnixListener::bind(path).expect("a control socket");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a connection");
        script(Link::new(stream)).await;
    })
}

/// Catches: a start that fails, or waits, when no frontend serves: no socket file (the
/// first start of a machine), a socket file nobody listens on (a frontend that died), are
/// both "nobody serves"; any other failure of the socket (here a path no socket can have)
/// is an error and not a cold start (which could bind the listeners of a frontend that
/// is serving).
#[tokio::test]
async fn nobody_serving_is_a_start_with_no_predecessor() {
    let dir = Scratch::new();
    let path = dir.socket();
    assert!(knock(&path, &hello()).await.expect("no socket").is_none());

    drop(std::os::unix::net::UnixListener::bind(&path).expect("a socket nobody serves"));
    assert!(path.exists());
    assert!(
        knock(&path, &hello())
            .await
            .expect("a stale socket")
            .is_none()
    );

    let too_long = dir.0.join("x".repeat(200));
    let error = knock(&too_long, &hello()).await.err().expect("an error");
    assert!(matches!(error, HandoverError::Socket(_)), "{error}");
}

/// Catches: an offer that does not carry the generation it names, a hello that does not
/// reach the holder as it was, a `ready` that is not answered by `released`, and a
/// successor that cannot tell the predecessor it serves once released: `ready` returns
/// once the holder says it let go, and the connection it returns says `serving`.
#[tokio::test]
async fn an_offer_gives_the_generation_and_ready_ends_at_released() {
    let dir = Scratch::new();
    let asked = hello();
    let expected = asked.clone();
    let served = holder(&dir.socket(), move |mut link| async move {
        assert_eq!(link.hear().await.expect("hello"), expected);
        link.say(&Message::Offer { generation: 9 })
            .await
            .expect("offer");
        assert_eq!(link.hear().await.expect("ready"), Message::Ready);
        link.say(&Message::Released).await.expect("released");
        assert_eq!(link.hear().await.expect("serving"), Message::Serving);
    });
    let offer = knock(&dir.socket(), &asked)
        .await
        .expect("an offer")
        .expect("a frontend serves");
    assert_eq!(offer.generation, 9);
    let released = offer.ready().await.expect("the holder let go");
    released.serving().await.expect("told");
    served.await.expect("the holder");
}

/// Catches: a refusal taken for an offer, and any other first answer taken for one; the
/// refusal's reason reaches the caller.
#[tokio::test]
async fn an_answer_that_is_not_an_offer_is_an_error() {
    let cases: [(Message, &str); 2] = [
        (
            Message::Refused("other flags".to_owned()),
            "refused the handover: other flags",
        ),
        (Message::Ready, "unexpected handover message: ready"),
    ];
    for (answer, said) in cases {
        let dir = Scratch::new();
        let served = holder(&dir.socket(), move |mut link| async move {
            link.hear().await.expect("hello");
            link.say(&answer).await.expect("answer");
        });
        let error = knock(&dir.socket(), &hello())
            .await
            .err()
            .expect("an error");
        assert!(error.to_string().contains(said), "{error}");
        served.await.expect("the holder");
    }
}

/// Catches: a successor that goes on when the holder could not stand down (it would run
/// beside a holder that still writes), or when the holder answers with something else
/// or goes away: each ends `ready` with an error that says which.
#[tokio::test]
async fn ready_ends_in_an_error_unless_the_holder_says_released() {
    let answers = [
        Some(Message::Failed("the writer cannot stop".to_owned())),
        Some(Message::Ready),
        None,
    ];
    let wanted = [
        "could not stand down: the writer cannot stop",
        "unexpected handover message: ready",
        "closed the control connection",
    ];
    for (answer, wanted) in answers.into_iter().zip(wanted) {
        let dir = Scratch::new();
        let served = holder(&dir.socket(), move |mut link| async move {
            link.hear().await.expect("hello");
            link.say(&Message::Offer { generation: 1 })
                .await
                .expect("offer");
            link.hear().await.expect("ready");
            if let Some(answer) = answer {
                link.say(&answer).await.expect("answer");
            }
        });
        let offer = knock(&dir.socket(), &hello())
            .await
            .expect("an offer")
            .expect("a frontend serves");
        let error = offer.ready().await.err().expect("not released");
        assert!(error.to_string().contains(wanted), "{error}");
        served.await.expect("the holder");
    }
}
