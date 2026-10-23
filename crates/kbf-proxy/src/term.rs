//! Which child holds the operation a `WaitExecution` names (design note `frontend-proxy.md`,
//! section 3, "Routing", and section 5 step 7).
//!
//! An operation's name, in the form a child with no state store writes and a child over one
//! still answers for the callers it holds, is `operations/{term}-{waiter}`, and the term is
//! the state-store writer generation of the child that named it. After a swap the new child
//! answers `NOT_FOUND` to a name of that form of another term, on purpose (issue #154), while
//! the old child, which lives until its last stream ends, still holds the operation. So while
//! the old child lives, a `WaitExecution` whose name carries its term is bound to it
//! ([`crate::slot::ActiveSlot::bind_preferring`]); every other call, and a name of any other
//! term, goes to the active child as before. A child over a state store names an operation
//! `operations/<id>`, with no term: [`operation_term`] reads none from it, and any child
//! answers it from the store, so it goes to the active child.
//!
//! Two halves:
//!
//! - **The request**, plain values: [`operation_term`] reads the term of a name, and [`probe`]
//!   reads the name from the start of a request body, the first length-prefixed gRPC message
//!   (`WaitExecutionRequest` field 1, a string). A compressed message, one over
//!   [`MAX_MESSAGE`] and anything that is not a name of the right shape are [`Probe::Skip`]:
//!   the stream goes to the active child.
//! - **The child**: [`Terms`] keeps the term of each slot's child. The proxy learns it from
//!   the child itself: [`poll`] asks every open slot's admin port for `GET /v1/admin/role`
//!   once a second and keeps `held_generation`, the generation the child took, which is the
//!   one the operation names of the old form carry (its farm machine is built from the
//!   generation of the store it writes through). A slot's term is recorded with the slot's generation, so a slot
//!   that was retired and opened again is a new child and inherits nothing.
//!
//! A term is learned within a poll interval of the child taking the generation (plus up to
//! [`POLL_TIMEOUT`] if the child is slow to answer), long before the next swap makes it the
//! old one. A child that does not answer keeps the term it last gave, and one that says it
//! holds no generation (a candidate) has none.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::config::UpstreamTable;
use crate::core::Core;
use crate::identity::{PROXY_AUTH, ProxySecret};
use crate::pool::Connector;
use crate::probe::{OK_LINE, POLL_EVERY, POLL_TIMEOUT};
use crate::slot::{ActiveSlot, Slot};

/// The `admin_protocol` of the `role` answer this reads (the child's `ADMIN_PROTOCOL`): an
/// answer of another version is not read.
const ADMIN_PROTOCOL: u32 = 1;
/// The most bytes of an admin answer that are read. A role view is a few hundred.
const MAX_ANSWER: u64 = 16 * 1024;
/// The end of the head of an HTTP/1.1 answer.
const HEAD_END: &[u8] = b"\r\n\r\n";
/// The prefix of a gRPC message: the compressed flag, then the length as four big-endian bytes.
const PREFIX: usize = 5;
/// The longest first message that is read. A `WaitExecutionRequest` holds one operation name,
/// a few dozen bytes; a longer message is not one the proxy routes by.
pub(crate) const MAX_MESSAGE: usize = 512;
/// The tag of `name`, field 1 of a `WaitExecutionRequest`, a length-delimited string.
const NAME_TAG: u8 = 0x0a;

/// The term of the operation name `operations/{term}-{waiter}`: `None` for any other
/// spelling. The numbers are read as the server writes them (canonical decimal: no sign, no
/// leading zero), so a name the server would not have written names no term here either.
pub(crate) fn operation_term(name: &str) -> Option<u64> {
    let (term, waiter) = name.strip_prefix("operations/")?.split_once('-')?;
    canonical_u64(waiter)?;
    canonical_u64(term)
}

/// `text` as a `u64`, if it is that number's decimal spelling exactly (`u64`'s `FromStr`
/// also takes a `+` sign and leading zeros).
fn canonical_u64(text: &str) -> Option<u64> {
    let n: u64 = text.parse().ok()?;
    (n.to_string() == text).then_some(n)
}

/// The `name` at the start of a `WaitExecutionRequest`: field 1, a string whose length is a
/// one-byte varint (under 128; a name is a few dozen bytes). `None` for anything else, and
/// for a name that is not UTF-8. The fields after the name are not read.
fn name_of(message: &[u8]) -> Option<&str> {
    let [NAME_TAG, len @ 0..=0x7f, name @ ..] = message else {
        return None;
    };
    let name = name.get(..usize::from(*len))?;
    std::str::from_utf8(name).ok()
}

/// What the start of a request body says about where the stream belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// The first message is not whole yet: read more.
    More,
    /// There is nothing to route by (compressed, too long, not a name of ours): the stream
    /// goes to the active child.
    Skip,
    /// The first message names an operation of this term.
    Term(u64),
}

/// Reads the term from `head`, the first bytes of a gRPC request body.
///
/// The answer is known as soon as the prefix says the message is compressed or longer than
/// [`MAX_MESSAGE`], without waiting for the message; otherwise once the message is whole.
/// Bytes after the first message are not read.
pub(crate) fn probe(head: &[u8]) -> Probe {
    let Some((&[compressed, a, b, c, d], rest)) = head.split_first_chunk::<PREFIX>() else {
        return Probe::More;
    };
    let len = usize::try_from(u32::from_be_bytes([a, b, c, d])).unwrap_or(usize::MAX);
    if (compressed != 0) | (len > MAX_MESSAGE) {
        return Probe::Skip;
    }
    let Some(message) = rest.get(..len) else {
        return Probe::More;
    };
    name_of(message)
        .and_then(operation_term)
        .map_or(Probe::Skip, Probe::Term)
}

/// Whether [`probe`] has an answer for `head`, so reading can stop.
pub(crate) fn decided(head: &[u8]) -> bool {
    probe(head) != Probe::More
}

/// A child's term, and the generation of the slot's open it was learned under.
#[derive(Clone, Copy, Debug)]
struct Known {
    generation: u64,
    term: u64,
}

/// The term of each slot's child, as the child said it.
pub(crate) struct Terms {
    connector: Connector,
    known: Mutex<BTreeMap<Slot, Known>>,
}

/// The fields of the child's `role` answer that are read.
#[derive(Deserialize)]
struct RoleView {
    admin_protocol: u32,
    held_generation: Option<u64>,
}

impl Terms {
    pub(crate) fn new(connector: Connector) -> Self {
        Self {
            connector,
            known: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records the term of the child that `slot` holds at `generation`; `None` forgets any.
    pub(crate) fn learn(&self, slot: Slot, generation: u64, term: Option<u64>) {
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        match term {
            Some(term) => known.insert(slot, Known { generation, term }),
            None => known.remove(&slot),
        };
    }

    /// The slot whose current child has `term`. A term learned under an earlier open of a
    /// slot (a retired and reopened slot is another child) names no slot.
    pub(crate) fn slot_of(&self, slots: &ActiveSlot, term: u64) -> Option<Slot> {
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        known
            .iter()
            .find(|(slot, k)| (k.generation, k.term) == (slots.generation(**slot), term))
            .map(|(slot, _)| *slot)
    }

    /// One `GET /v1/admin/role` of the child at `addr`, with the proxy's secret: its answer,
    /// or nothing for a refused connection, no answer within [`POLL_TIMEOUT`], a status other
    /// than 200, a body that is not a role view and a view of another `admin_protocol`.
    async fn ask(&self, addr: SocketAddr, secret: &ProxySecret) -> Option<RoleView> {
        let asked = async {
            let mut io = (self.connector)(addr).await.ok()?;
            let request = format!(
                "GET /v1/admin/role HTTP/1.1\r\nHost: kbf-proxy\r\nConnection: close\r\n\
                 {PROXY_AUTH}: {}\r\n\r\n",
                secret.hex()
            );
            io.write_all(request.as_bytes()).await.ok()?;
            let mut answer = Vec::new();
            io.take(MAX_ANSWER).read_to_end(&mut answer).await.ok()?;
            parse(&answer)
        };
        timeout(POLL_TIMEOUT, asked).await.ok().flatten()
    }
}

/// The role view in an HTTP/1.1 `answer` read to the end of the connection.
fn parse(answer: &[u8]) -> Option<RoleView> {
    let rest = answer.strip_prefix(OK_LINE)?;
    let body = rest
        .windows(HEAD_END.len())
        .position(|w| w == HEAD_END)
        .and_then(|at| rest.get(at + HEAD_END.len()..))?;
    let view: RoleView = serde_json::from_slice(body).ok()?;
    (view.admin_protocol == ADMIN_PROTOCOL).then_some(view)
}

/// Where `slot`'s child answers its admin calls: loopback, whatever the table says.
pub(crate) const fn admin_addr(table: UpstreamTable, slot: Slot) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), table.admin_port(slot))
}

/// Asks `slot`'s child for its term, if the slot is open, and records the answer. The slot's
/// generation is read before the question, so a child that is replaced while it is asked is
/// recorded under the old generation, which no longer names the slot. A child that gives no
/// answer leaves what was recorded; one that holds no generation (a candidate, or a view that
/// could not read the store) has no term.
pub(crate) async fn learn_slot(core: &Core, slot: Slot) {
    if !core.slots.is_open(slot) {
        return;
    }
    let generation = core.slots.generation(slot);
    let addr = admin_addr(core.table(), slot);
    if let Some(view) = core.terms.ask(addr, &core.security.secret).await {
        let held = view.held_generation.filter(|held| *held != 0);
        core.terms.learn(slot, generation, held);
    }
}

/// Asks both slots every [`POLL_EVERY`] until the task is dropped. A poll of one slot never
/// delays the other.
pub(crate) async fn poll(core: Arc<Core>) {
    let mut tick = interval(POLL_EVERY);
    // A poll that took its 3 s is not made up for with a burst.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        futures::future::join_all(Slot::ALL.map(|slot| learn_slot(&core, slot))).await;
    }
}

/// The first message of a `WaitExecutionRequest` naming `name`, as a client sends it.
#[cfg(test)]
pub(crate) fn name_message(name: &str) -> Vec<u8> {
    let mut message = vec![NAME_TAG, u8::try_from(name.len()).expect("a short name")];
    message.extend_from_slice(name.as_bytes());
    message
}

/// `message` as one gRPC message with the compressed flag `flag`.
#[cfg(test)]
pub(crate) fn frame(flag: u8, message: &[u8]) -> Vec<u8> {
    let len = u32::try_from(message.len()).expect("a small message");
    let mut framed = vec![flag];
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(message);
    framed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The term is the number between the prefix and the dash, in the spelling the server
    /// writes, and the waiter after the dash is one too.
    ///
    /// Catches: a term read from a sign, a leading zero or a number that wraps; a name with
    /// no waiter, a second dash or another prefix taken for ours.
    #[test]
    fn a_term_is_the_canonical_number_before_the_dash() {
        assert_eq!(operation_term("operations/7-3"), Some(7));
        assert_eq!(operation_term("operations/0-0"), Some(0));
        let max = format!("operations/{}-1", u64::MAX);
        assert_eq!(operation_term(&max), Some(u64::MAX));
        for bad in [
            "",
            "operations/",
            "operations/7",
            "operations/-3",
            "operations/7-",
            "operations/+7-3",
            "operations/07-3",
            "operations/7-03",
            "operations/7-+3",
            "operations/18446744073709551616-1",
            "operations/a-1",
            "operations/7-b",
            "operations/7-3-4",
            "operation/7-3",
            "/operations/7-3",
        ] {
            assert_eq!(operation_term(bad), None, "{bad:?}");
        }
    }

    /// A whole first message gives its name's term; what follows the name or the message is
    /// not read.
    ///
    /// Catches: a name read from a later message or field; a term read from a message that
    /// is cut short.
    #[test]
    fn the_first_message_names_a_term() {
        let name = name_message("operations/7-3");
        let whole = frame(0, &name);
        assert_eq!(probe(&whole), Probe::Term(7));
        let mut two = whole.clone();
        two.extend_from_slice(&frame(0, &name_message("operations/8-1")));
        assert_eq!(probe(&two), Probe::Term(7), "later messages are not read");
        let mut tail = name;
        tail.extend_from_slice(&[0x12, 0x01, b'x']);
        assert_eq!(probe(&frame(0, &tail)), Probe::Term(7), "a later field");
        for cut in 0..whole.len() {
            assert_eq!(probe(&whole[..cut]), Probe::More, "cut at {cut}");
        }
        assert!(decided(&whole));
        assert!(!decided(&whole[..whole.len() - 1]));
    }

    /// A message that cannot be routed by is known so before it arrives when its prefix says
    /// so, and is the active child's either way.
    ///
    /// Catches: the compressed flag ignored (compressed bytes read as a protobuf); the cap
    /// removed or moved (a long message buffered, or one at the cap refused); a name of the
    /// wrong tag, a two-byte length, a cut name, bytes that are not UTF-8 or a name that is not
    /// an operation's taken for a term.
    #[test]
    fn what_cannot_be_routed_by_is_skipped() {
        let good = name_message("operations/7-3");
        assert_eq!(probe(&frame(1, &good)), Probe::Skip, "compressed");
        assert_eq!(probe(&frame(2, &good)), Probe::Skip, "any flag but 0");
        let mut long = good.clone();
        long.resize(MAX_MESSAGE + 1, 0);
        assert_eq!(probe(&frame(0, &long)), Probe::Skip, "over the cap");
        // And known from the prefix alone, before the message has arrived.
        assert_eq!(probe(&frame(0, &long)[..PREFIX]), Probe::Skip);
        assert!(decided(&frame(0, &long)[..PREFIX]));
        let mut at_cap = good.clone();
        at_cap.resize(MAX_MESSAGE, 0);
        assert_eq!(probe(&frame(0, &at_cap)), Probe::Term(7), "at the cap");
        for message in [
            vec![],
            vec![0x12, 1, b'x'],
            vec![NAME_TAG, 0x80, 0x01],
            vec![NAME_TAG, 14, b'o'],
            vec![NAME_TAG, 2, 0xff, 0xfe],
            name_message("garbage"),
            name_message("operations/x-1"),
            name_message(""),
        ] {
            assert_eq!(probe(&frame(0, &message)), Probe::Skip, "{message:?}");
        }
    }

    /// The role view is read from the end of the head of a 200, of the one protocol.
    ///
    /// Catches: a body read from inside the head; a status other than 200, a protocol other
    /// than 1 or a head that never ends taken for an answer.
    #[test]
    fn a_role_view_is_read_from_a_200_of_the_protocol() {
        let view = |status: &str, body: &str| format!("HTTP/1.1 {status}\r\nx: y\r\n\r\n{body}");
        let body = r#"{"role":"active","admin_protocol":1,"held_generation":7,"extra":[1]}"#;
        let got = parse(view("200 OK", body).as_bytes()).expect("a view");
        assert_eq!(got.held_generation, Some(7));
        let none = r#"{"admin_protocol":1,"held_generation":null}"#;
        let got = parse(view("200 OK", none).as_bytes()).expect("a view");
        assert_eq!(got.held_generation, None);
        let missing = r#"{"admin_protocol":1}"#;
        let got = parse(view("200 OK", missing).as_bytes()).expect("a view");
        assert_eq!(got.held_generation, None);
        assert!(parse(view("403 Forbidden", body).as_bytes()).is_none());
        assert!(parse(view("200 OK", &body.replace(":1,", ":2,")).as_bytes()).is_none());
        assert!(parse(view("200 OK", "not json").as_bytes()).is_none());
        assert!(parse(view("200 OK", "").as_bytes()).is_none());
        assert!(parse(b"HTTP/1.1 200 OK\r\nx: y\r\n").is_none());
        assert!(parse(b"HTTP/2 200").is_none());
        assert!(parse(b"").is_none());
    }
}
