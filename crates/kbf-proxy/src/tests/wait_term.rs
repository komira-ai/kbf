//! A `WaitExecution` that names an operation of the old child's term is bound to the old child
//! while it lives, and every other stream goes where it went before; the proxy learns each
//! child's term from the child's own `role` answer. The children are the kit's fakes and a swap
//! is the flip of the gate. The tests run under paused time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use super::kit::{
    CAPS, Client, EXECUTE, Rig, Script, WRITE, finish, grpc_ok, read_all, script, send_end, tagged,
    until,
};
use crate::slot::{Retire, Slot};
use crate::stream::PEEK_FOR;
use crate::term::{MAX_MESSAGE, admin_addr, frame, learn_slot, name_message, poll};

const WAIT: &str = "/build.bazel.remote.execution.v2.Execution/WaitExecution";
/// The term of the old child, in slot A, and of the new one, in slot B.
const OLD: u64 = 7;
const NEW: u64 = 8;
const HOLD: Duration = Duration::from_secs(20);

/// The bodies a recording child was sent, one per call.
type Bodies = Arc<Mutex<Vec<Vec<u8>>>>;
/// The heads of the calls a fake admin port took.
type Heads = Arc<Mutex<Vec<String>>>;

/// A rig after a swap: B, of term `NEW`, is active, and A, of term `OLD`, is open and serves.
async fn swapped(a: Script, b: Script) -> Rig {
    let rig = Rig::start(a, b).await;
    let old = rig.core.slots.generation(Slot::A);
    rig.core.terms.learn(Slot::A, old, Some(OLD));
    rig.flip_to_b();
    let new = rig.core.slots.generation(Slot::B);
    rig.core.terms.learn(Slot::B, new, Some(NEW));
    rig
}

/// A child that answers `tag` and keeps the body of each call it takes.
fn recording(tag: &'static str, bodies: &Bodies) -> Script {
    let bodies = Arc::clone(bodies);
    script(move |mut call| {
        let bodies = Arc::clone(&bodies);
        async move {
            let body = read_all(&mut call.body).await;
            bodies.lock().expect("lock").push(body);
            grpc_ok(&mut call.respond, tag);
        }
    })
}

/// The slot the proxy would take a stream for an operation of `term` to, if any.
fn term_of(rig: &Rig, term: u64) -> Option<Slot> {
    rig.core.terms.slot_of(&rig.core.slots, term)
}

/// The one message of a `WaitExecution` for the operation `name`, as a client sends it.
fn named(name: &str) -> Bytes {
    Bytes::from(frame(0, &name_message(name)))
}

/// A `WaitExecution` with `message` as its one message; the answer is the tag of the child
/// that took it.
async fn wait_with(client: &mut Client, message: Vec<u8>) -> String {
    let (response, mut body) = client.start(WAIT, false).await;
    send_end(&mut body, Bytes::from(message));
    finish(response).await.text()
}

/// A `WaitExecution` for the operation `name`.
async fn wait_for(client: &mut Client, name: &str) -> String {
    wait_with(client, frame(0, &name_message(name))).await
}

/// While the old child lives, a `WaitExecution` for an operation of its term is the old
/// child's; a name of the new term goes where it always went.
///
/// Catches: routing by the active slot only (the old term answered by the new child, which
/// answers NOT_FOUND); a term matched to the wrong slot.
#[tokio::test(start_paused = true)]
async fn a_wait_for_the_old_childs_term_goes_to_the_old_child() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    assert_eq!(wait_for(&mut client, "operations/7-3").await, "a");
    assert_eq!(wait_for(&mut client, "operations/8-3").await, "b");
    assert_eq!(rig.a.state.dispatches(), 1);
    assert_eq!(rig.b.state.dispatches(), 1);
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
}

/// A name of a term no child has, a spelling the server does not write, a message that is no
/// name and a body that is no message at all are the active child's: the answer it always gave.
///
/// Catches: an unknown term sent anywhere but the active child; a name read leniently (a
/// leading zero, a missing waiter) so that it reaches the old child; a body that fails to
/// parse failing the stream.
#[tokio::test(start_paused = true)]
async fn a_name_of_another_term_or_no_name_goes_to_the_active_child() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    for name in [
        "operations/9-3",
        "operations/0-3",
        "operations/7",
        "operations/07-3",
        "operations/-3",
        "garbage",
        "",
    ] {
        assert_eq!(wait_for(&mut client, name).await, "b", "{name:?}");
    }
    // A body that is not a gRPC message.
    assert_eq!(client.unary(WAIT).await.text(), "b");
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// After the old child retires nothing is bound to it: the active child answers the old term
/// as it did before.
///
/// Catches: a term kept for a closed slot taking a stream to a retired child.
#[tokio::test(start_paused = true)]
async fn after_the_old_child_retires_its_term_goes_to_the_active_child() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    assert_eq!(wait_for(&mut client, "operations/7-3").await, "a");
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
    assert_eq!(rig.core.slots.retire(Slot::A), Retire::Closed);
    assert_eq!(wait_for(&mut client, "operations/7-3").await, "b");
    assert_eq!(rig.a.state.dispatches(), 1);
}

/// A slot opened again is another child: the term the old child gave names it no more.
///
/// Catches: a term kept past the open it was learned under, which sends a stream for the old
/// child's operation to the child that took its slot.
#[tokio::test(start_paused = true)]
async fn a_slot_opened_again_does_not_inherit_the_term() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    assert_eq!(rig.core.slots.retire(Slot::A), Retire::Closed);
    assert_eq!(rig.core.slots.open(Slot::A), 2);
    assert_eq!(term_of(&rig, OLD), None);
    let mut client = rig.client().await;
    assert_eq!(wait_for(&mut client, "operations/7-3").await, "b");
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// Only `WaitExecution` is read for a term. Any other call, whatever its message says, is
/// dispatched at its headers, before the client has sent a byte of its body, and goes to the
/// active child.
///
/// Catches: the start of every request buffered before dispatch (the stream would not reach
/// the child until the client sent its message); a term read from the message of `Execute`,
/// the capabilities call or a ByteStream write.
#[tokio::test(start_paused = true)]
async fn only_wait_execution_is_read_and_every_other_call_is_dispatched_at_its_headers() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    for (n, path) in [EXECUTE, CAPS, WRITE].into_iter().enumerate() {
        let (response, mut body) = client.start(path, false).await;
        until(|| rig.b.state.dispatches() == n + 1).await;
        send_end(&mut body, named("operations/7-3"));
        assert_eq!(finish(response).await.text(), "b", "{path}");
    }
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A first message over the cap, or one marked compressed, is not read: the stream goes to the
/// active child. One at the cap is read.
///
/// Catches: the cap removed (a long message buffered and read) or moved to refuse one at the
/// cap; the compressed flag ignored (compressed bytes read as a name).
#[tokio::test(start_paused = true)]
async fn a_message_over_the_cap_or_compressed_goes_to_the_active_child() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let name = name_message("operations/7-3");
    let mut long = name.clone();
    long.resize(MAX_MESSAGE + 1, 0);
    let mut at_cap = name.clone();
    at_cap.resize(MAX_MESSAGE, 0);
    // Over the cap: the name is the old child's, and the message is not read.
    assert_eq!(wait_with(&mut client, frame(0, &long)).await, "b");
    // Compressed: the same name, flagged.
    assert_eq!(wait_with(&mut client, frame(1, &name)).await, "b");
    // At the cap, and whole: read.
    assert_eq!(wait_with(&mut client, frame(0, &at_cap)).await, "a");
    assert_eq!(wait_with(&mut client, frame(0, &name)).await, "a");
}

/// A message that arrives a byte at a time is not decided within the frames the proxy reads
/// ahead, so the stream goes to the active child, and the child gets every byte in order: the
/// bytes read ahead are replayed, and the rest follow.
///
/// Catches: bytes read ahead and not sent; bytes sent twice; the end of the request lost.
#[tokio::test(start_paused = true)]
async fn a_message_that_trickles_in_reaches_the_active_child_whole() {
    let bodies = Bodies::default();
    let rig = swapped(tagged("a"), recording("b", &bodies)).await;
    let mut client = rig.client().await;
    let message = frame(0, &name_message("operations/7-3"));
    let (response, mut body) = client.start(WAIT, false).await;
    let last = message.len() - 1;
    for (at, byte) in message.iter().enumerate() {
        let chunk = Bytes::copy_from_slice(&[*byte]);
        body.send_data(chunk, at == last).expect("the body");
    }
    assert_eq!(finish(response).await.text(), "b");
    assert_eq!(*bodies.lock().expect("lock"), vec![message]);
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A client that sends its message late is not waited for beyond `PEEK_FOR`: the stream is
/// dispatched to the active child with no message, which then arrives and is sent on.
///
/// Catches: a read-ahead with no bound (a client that never sends holds the stream for ever);
/// the message lost when the wait is given up.
#[tokio::test(start_paused = true)]
async fn a_message_that_comes_late_is_dispatched_without_being_read() {
    let bodies = Bodies::default();
    let rig = swapped(tagged("a"), recording("b", &bodies)).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(WAIT, false).await;
    tokio::time::sleep(PEEK_FOR + Duration::from_secs(1)).await;
    until(|| rig.b.state.dispatches() == 1).await;
    let message = frame(0, &name_message("operations/7-3"));
    send_end(&mut body, Bytes::from(message.clone()));
    assert_eq!(finish(response).await.text(), "b");
    assert_eq!(*bodies.lock().expect("lock"), vec![message]);
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A `WaitExecution` with no body at all, and one that ends in trailers with no message, are
/// not read and go to the active child.
///
/// Catches: a read of a request that has nothing to read (and an empty frame added to a
/// request that was sent as headers only); trailers taken for a message.
#[tokio::test(start_paused = true)]
async fn a_wait_with_no_message_goes_to_the_active_child() {
    let rig = swapped(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let (response, _body) = client.start(WAIT, true).await;
    assert_eq!(finish(response).await.text(), "b");
    let (response, mut body) = client.start(WAIT, false).await;
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-note", HeaderValue::from_static("no message"));
    body.send_trailers(trailers).expect("the trailers");
    assert_eq!(finish(response).await.text(), "b");
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A `WaitExecution` held by a swap is bound when the hold ends, by the children as they are
/// then: the old child's operation goes to the old child, which is no longer the active one.
///
/// Catches: a term looked up before the hold ends instead of at the bind (the term is learned
/// only once the stream is held, so a lookup made at the headers or before the hold finds no
/// child and the stream goes to the new one).
#[tokio::test(start_paused = true)]
async fn a_held_wait_for_the_old_term_binds_to_the_old_child_on_release() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    rig.core.slots.open(Slot::B);
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let (response, mut body) = client.start(WAIT, false).await;
    send_end(&mut body, named("operations/7-3"));
    until(|| rig.core.queue.held() == 1).await;
    let old = rig.core.slots.generation(Slot::A);
    rig.core.terms.learn(Slot::A, old, Some(OLD));
    rig.core.slots.release_into(Slot::B).expect("B is open");
    drop(guard);
    assert_eq!(finish(response).await.text(), "a");
    assert_eq!(rig.b.state.dispatches(), 0);
}

/// Puts a fake admin port where `slot`'s child answers its admin calls: it reads the head of
/// each call, keeps it, answers `reply` and closes. A port put at the same place later
/// replaces this one.
fn serve_admin(rig: &Rig, slot: Slot, reply: String) -> (Heads, JoinHandle<()>) {
    let heads = Heads::default();
    let mut incoming = rig.net.listen(admin_addr(rig.table, slot));
    let kept = Arc::clone(&heads);
    let task = tokio::spawn(async move {
        while let Some(mut io) = incoming.recv().await {
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                io.read_exact(&mut byte).await.expect("a request head");
                head.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&head).into_owned();
            kept.lock().expect("lock").push(line);
            io.write_all(reply.as_bytes()).await.expect("the answer");
            io.shutdown().await.expect("a clean close");
        }
    });
    (heads, task)
}

/// An HTTP/1.1 answer.
fn http(status: &str, body: &str) -> String {
    let len = body.len();
    format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}")
}

/// The answer to `GET /v1/admin/role` of a child whose `held_generation` is `held` (JSON).
fn role(held: &str) -> String {
    let body = format!(r#"{{"role":"active","admin_protocol":1,"held_generation":{held}}}"#);
    http("200 OK", &body)
}

/// The term of a slot's child is the generation the child says it holds, asked with the
/// proxy's secret on the slot's admin port, and it follows the child.
///
/// Catches: a term read from the wrong field; a call without the secret (the child refuses
/// it); a call to the wrong path or port; a term that does not move when the child's does.
#[tokio::test(start_paused = true)]
async fn the_term_is_the_generation_the_child_says_it_holds() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let (heads, _admin) = serve_admin(&rig, Slot::A, role("7"));
    learn_slot(&rig.core, Slot::A).await;
    assert_eq!(term_of(&rig, 7), Some(Slot::A));
    assert_eq!(term_of(&rig, 8), None);
    let auth = format!("x-kbf-proxy-auth: {}", rig.core.security.secret.hex());
    let want = [
        "GET /v1/admin/role HTTP/1.1",
        "Host: kbf-proxy",
        "Connection: close",
        auth.as_str(),
        "",
        "",
    ]
    .join("\r\n");
    assert_eq!(*heads.lock().expect("lock"), vec![want]);
    let (_, _admin) = serve_admin(&rig, Slot::A, role("9"));
    learn_slot(&rig.core, Slot::A).await;
    assert_eq!(term_of(&rig, 9), Some(Slot::A));
    assert_eq!(term_of(&rig, 7), None);
}

/// A child that does not give a usable answer keeps the term it gave: a refusal, a body that
/// is not a role view, another admin protocol, something that is not HTTP, nothing at all, a
/// refused connection and a child that never answers.
///
/// Catches: a term forgotten on a failed poll, which leaves a window in which the old child's
/// operations are not found; an answer of another protocol, or after a 403, read as the
/// child's.
#[tokio::test(start_paused = true)]
async fn a_child_that_gives_no_usable_answer_keeps_the_term_it_gave() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let (_, _admin) = serve_admin(&rig, Slot::A, role("7"));
    learn_slot(&rig.core, Slot::A).await;
    assert_eq!(term_of(&rig, 7), Some(Slot::A));
    let other_protocol = http("200 OK", r#"{"admin_protocol":2,"held_generation":9}"#);
    for reply in [
        http("403 Forbidden", "{}"),
        http("200 OK", "not json"),
        other_protocol,
        "not http at all".to_owned(),
        String::new(),
    ] {
        let (_, _admin) = serve_admin(&rig, Slot::A, reply);
        learn_slot(&rig.core, Slot::A).await;
        assert_eq!(term_of(&rig, 7), Some(Slot::A));
        assert_eq!(term_of(&rig, 9), None);
    }
    // Nothing listens at B's admin port, and then nothing there ever answers.
    rig.core.slots.open(Slot::B);
    let generation = rig.core.slots.generation(Slot::B);
    rig.core.terms.learn(Slot::B, generation, Some(5));
    learn_slot(&rig.core, Slot::B).await;
    assert_eq!(term_of(&rig, 5), Some(Slot::B));
    rig.net.blackhole(admin_addr(rig.table, Slot::B));
    learn_slot(&rig.core, Slot::B).await;
    assert_eq!(term_of(&rig, 5), Some(Slot::B));
}

/// A child that holds no generation has no term: a candidate says `null`, and a child that
/// cannot read its store says 0, which no store is at.
///
/// Catches: a candidate's or an unreadable child's answer recorded as a term, which would
/// match an operation name of term 0.
#[tokio::test(start_paused = true)]
async fn a_child_that_holds_no_generation_has_no_term() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    for none in ["null", "0"] {
        let (_, _admin) = serve_admin(&rig, Slot::A, role("7"));
        learn_slot(&rig.core, Slot::A).await;
        assert_eq!(term_of(&rig, 7), Some(Slot::A));
        let (_, _admin) = serve_admin(&rig, Slot::A, role(none));
        learn_slot(&rig.core, Slot::A).await;
        assert_eq!(term_of(&rig, 7), None);
        assert_eq!(term_of(&rig, 0), None);
    }
}

/// Only an open slot is asked: a slot never opened, or retired, has no child to ask.
///
/// Catches: a poll of a slot nobody runs in (a dial to a port another process may hold).
#[tokio::test(start_paused = true)]
async fn only_an_open_slot_is_asked() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let (heads, _admin) = serve_admin(&rig, Slot::B, role("8"));
    learn_slot(&rig.core, Slot::B).await;
    // B was never opened.
    assert!(heads.lock().expect("lock").is_empty());
    rig.core.slots.open(Slot::B);
    learn_slot(&rig.core, Slot::B).await;
    assert_eq!(heads.lock().expect("lock").len(), 1);
    assert_eq!(term_of(&rig, 8), Some(Slot::B));
    // B is retired.
    assert_eq!(rig.core.slots.retire(Slot::B), Retire::Closed);
    learn_slot(&rig.core, Slot::B).await;
    assert_eq!(heads.lock().expect("lock").len(), 1);
}

/// The poll asks both slots, and again every second.
///
/// Catches: a poll that asks one slot only, or only once.
#[tokio::test(start_paused = true)]
async fn the_poll_learns_both_children_every_second() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    rig.core.slots.open(Slot::B);
    let (_, _a) = serve_admin(&rig, Slot::A, role("7"));
    let (_, _b) = serve_admin(&rig, Slot::B, role("8"));
    let polling = tokio::spawn(poll(Arc::clone(&rig.core)));
    until(|| term_of(&rig, 7).is_some()).await;
    until(|| term_of(&rig, 8).is_some()).await;
    let (_, _a) = serve_admin(&rig, Slot::A, role("9"));
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(term_of(&rig, 9), Some(Slot::A));
    assert_eq!(term_of(&rig, 7), None);
    polling.abort();
}
