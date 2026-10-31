//! The store read grants of `GET /locate`: the header line and the `g` of each line, the split of
//! many keys into several grants, the end of a grant, what a grant may list, and that a grant
//! minted here is accepted by tierstore's own server for exactly the keys it lists.
//!
//! The store is a real `tierstore::server::Server` over an in-memory backend, asked through
//! `Server::handle` with a clock set by hand. Nothing here waits or reads the wall clock.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use kbf_front::job_read::{
    AccessKey, GRANT_LIFETIME, KEYS_PER_GRANT, Reply, StoreGrants, check_store_bucket,
    check_store_url,
};
use kbf_types::Digest;
use serde_json::{Value, json};
use tierstore::server::{GRANT_HEADER, Server, ServerConfig};
use tierstore::{Backend as _, MemoryBackend, TestClock};
use tonic::codegen::http::{Method, Request, StatusCode};

mod locate_common;

use locate_common::*;

const BUCKET: &str = "tiers";
const STORE: &str = "http://store.farm.test:9000";
const KEY_ID: &str = "locate-grant";
const SECRET: &str = "locate-grant-secret-0123456789abcdef";
/// The time the rig's grants are checked at, in unix seconds (see `locate_common::rig`).
const NOW: u64 = 100;
const OBJECT: &[u8] = b"an object of the store";

fn grants() -> StoreGrants {
    StoreGrants::new(AccessKey::new(KEY_ID, SECRET), STORE, BUCKET).expect("a usable key")
}

/// What a tierstore server holds and answers, with the time set by hand.
struct Store {
    server: Arc<Server>,
    clock: Arc<TestClock>,
}

/// A server for `bucket` that trusts the grant key, at `NOW`, holding `keys`.
async fn store(bucket: &str, keys: &[String]) -> Store {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW * 1000);
    let backend = Arc::new(MemoryBackend::default());
    for key in keys {
        let put = backend.put(key, Bytes::from_static(OBJECT)).await;
        put.expect("an object is stored");
    }
    let mut config = ServerConfig::new(
        bucket,
        "full-key-id",
        "full-key-secret-0123456789",
        clock.clone(),
    );
    config.grant_keys = vec![AccessKey::new(KEY_ID, SECRET)];
    Store {
        server: Server::new(backend, config),
        clock,
    }
}

impl Store {
    fn at(&self, seconds: u64) {
        self.clock.set(seconds * 1000);
    }

    async fn send(&self, method: &str, key: &str, token: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(format!("/{BUCKET}/{key}"))
            .header("host", "store.test")
            .header(GRANT_HEADER, token)
            .body(Full::new(Bytes::new()))
            .expect("a request");
        self.server.handle(request).await.status()
    }

    async fn get(&self, key: &str, token: &str) -> StatusCode {
        self.send("GET", key, token).await
    }
}

/// A locate answer taken apart: its header line and the lines after it.
struct Answer {
    header: Value,
    lines: Vec<Value>,
}

impl Answer {
    fn of(reply: &Reply) -> Self {
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        let text = std::str::from_utf8(&reply.body).expect("utf-8");
        let mut lines = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("a JSON line"));
        let header = lines.next().expect("a header line");
        assert!(
            header.get("digest").is_none(),
            "the first line is the header"
        );
        Self {
            header,
            lines: lines.collect(),
        }
    }

    fn tokens(&self) -> Vec<String> {
        let grants = self.header["grants"].as_array().expect("grants");
        grants
            .iter()
            .map(|g| g.as_str().expect("a token").to_owned())
            .collect()
    }

    fn line(&self, digest: &Digest) -> &Value {
        let text = digest.to_string();
        let found = self.lines.iter().find(|line| line["digest"] == text);
        found.expect("the digest has a line")
    }

    /// The store key a line points at, if it points at one.
    fn key_of(line: &Value) -> Option<String> {
        let at = line.get("pack").or_else(|| line.get("large"))?;
        Some(at["key"].as_str().expect("a key").to_owned())
    }

    /// The keys of the lines, by the grant each names.
    fn keys_by_grant(&self) -> BTreeMap<u64, BTreeSet<String>> {
        let mut by = BTreeMap::<u64, BTreeSet<String>>::new();
        for line in &self.lines {
            if let Some(key) = Self::key_of(line) {
                let g = line["g"].as_u64().expect("a located line has a g");
                by.entry(g).or_default().insert(key);
            }
        }
        by
    }

    fn all_keys(&self) -> Vec<String> {
        let keys: BTreeSet<String> = self.lines.iter().filter_map(Self::key_of).collect();
        keys.into_iter().collect()
    }
}

/// An Action over `n` small files, each in a pack of its own, `cas/f/<tag>/<i>`.
fn many(source: &Source, tag: &str, n: usize) -> Built {
    let contents: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("{tag} blob {i}").into_bytes())
        .collect();
    let a = build(source, tag, &contents);
    for (i, digest) in a.files.iter().enumerate() {
        pack(
            source,
            digest,
            span(&format!("cas/f/{tag}/{i}"), 0, digest.size_bytes),
        );
    }
    a
}

/// Catches: a header that is missing, or that names a store or bucket other than the flags', a
/// line without the index of its grant, a located line pointing at a grant that does not list its
/// key, an absent line that carries a `g`, and the spans of one pack listed under two grants.
#[tokio::test]
async fn a_mixed_answer_has_a_header_and_each_located_line_names_its_grant() {
    let source = Source::default();
    let a = build(
        &source,
        "a",
        &[
            b"alpha".to_vec(),
            b"beta".to_vec(),
            vec![7; 2000],
            b"delta".to_vec(),
        ],
    );
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    pack(&source, &a.files[1], span("cas/f/w/1", 5, 4));
    pack(&source, &a.deep, span("cas/f/w/2", 77, a.deep.size_bytes));
    source.large.lock().unwrap().insert(a.files[2], 2000);
    let rig = rig_granting(source, small(), Some(grants()));
    let reply = rig.locate(&rig.token(a.action)).await;
    let answer = Answer::of(&reply);

    assert_eq!(answer.header["store"], STORE);
    assert_eq!(answer.header["bucket"], BUCKET);
    let tokens = answer.tokens();
    assert_eq!(tokens.len(), 1, "a few keys are one grant");
    // The header is the first line, and the lines are the scope's, one each.
    let named: BTreeSet<String> = answer
        .lines
        .iter()
        .map(|l| l["digest"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        named,
        a.scope()
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
    );
    for located in [&a.files[0], &a.files[1], &a.deep, &a.files[2]] {
        assert_eq!(answer.line(located)["g"], 0, "{located}");
    }
    assert_eq!(
        answer.line(&a.files[2])["large"]["key"],
        large_key_of(&a.files[2])
    );
    for absent in [&a.files[3], &a.action, &a.root, &a.sub, &a.command] {
        assert_eq!(
            answer.line(absent),
            &json!({"digest": absent.to_string(), "absent": true})
        );
    }
    // Two spans of one pack are one key of the grant.
    let store_keys = answer.all_keys();
    assert_eq!(
        store_keys,
        ["cas/f/w/1", "cas/f/w/2", &large_key_of(&a.files[2])]
    );
}

fn large_key_of(digest: &Digest) -> String {
    kbf_front::job_read::large_key(digest)
}

/// Catches: one grant for all the keys of a big answer (tierstore refuses a token over its
/// header limit), a grant over `KEYS_PER_GRANT` keys, and a line whose `g` names a grant that
/// does not list its key: every line's own grant is checked against the store, and so is the
/// grant of another line's key.
#[tokio::test]
async fn more_than_the_cap_of_keys_split_into_grants_that_each_cover_their_lines() {
    let source = Source::default();
    let a = many(&source, "a", 1300);
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);

    assert_eq!(KEYS_PER_GRANT, 600);
    let tokens = answer.tokens();
    assert_eq!(tokens.len(), 3, "1300 keys at 600 a grant");
    let by = answer.keys_by_grant();
    assert_eq!(by.len(), 3);
    assert!(
        by.values().all(|keys| keys.len() <= KEYS_PER_GRANT),
        "{by:?}"
    );
    assert_eq!(by.values().map(BTreeSet::len).sum::<usize>(), 1300);
    assert!(tokens.iter().all(|t| t.len() < 64 * 1024));

    let store = store(BUCKET, &answer.all_keys()).await;
    for (g, keys) in &by {
        for key in keys {
            assert_eq!(
                store.get(key, &tokens[*g as usize]).await,
                StatusCode::OK,
                "{key}"
            );
        }
    }
    // A key of the first grant is not read with the last one's.
    let first = by[&0].iter().next().unwrap();
    assert_eq!(store.get(first, &tokens[2]).await, StatusCode::FORBIDDEN);
}

/// Catches: a split by count only (a grant of long names over the header limit), a long key
/// alone in a grant dropped for being over the budget, and a grant that cannot be made at all
/// (a token tierstore would refuse as too large) sending lines on with a `g` it has no token for.
#[tokio::test]
async fn the_split_also_follows_the_bytes_and_a_grant_that_cannot_be_made_leaves_its_lines_absent()
{
    let source = Source::default();
    let a = many(&source, "a", 3);
    let tiny = grants().with_bounds(KEYS_PER_GRANT, 1);
    let rig = rig_granting(source, small(), Some(tiny));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);
    assert_eq!(
        answer.tokens().len(),
        3,
        "a key over the byte budget is a grant of its own"
    );
    assert_eq!(
        answer
            .keys_by_grant()
            .values()
            .map(BTreeSet::len)
            .collect::<Vec<_>>(),
        [1, 1, 1]
    );

    // No bound: 80 keys of a thousand bytes are over the 64 KiB tierstore takes.
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..80).map(|i| format!("long {i}").into_bytes()).collect();
    let b = build(&source, "b", &contents);
    for (i, digest) in b.files.iter().enumerate() {
        let key = format!("cas/f/{}/{i}", "x".repeat(1000));
        pack(&source, digest, span(&key, 0, digest.size_bytes));
    }
    let unbounded = grants().with_bounds(usize::MAX, usize::MAX);
    let rig = rig_granting(source, small(), Some(unbounded));
    let answer = Answer::of(&rig.locate(&rig.token(b.action)).await);
    assert!(answer.tokens().is_empty());
    assert!(
        answer
            .lines
            .iter()
            .all(|line| line["absent"] == true && line.get("g").is_none())
    );
}

/// The first store grant of the answer to a job read grant that ends at `not_after`.
async fn first_token(rig: &Rig, action: Digest, not_after: u64) -> String {
    let reply = rig.locate(&rig.token_until(action, not_after)).await;
    Answer::of(&reply).tokens().remove(0)
}

/// Catches: a grant that outlives the job's read grant, one that lasts longer than ten minutes
/// (the store refuses one more than its `--grant-max-lifetime` ahead, and a host whose clock
/// runs ahead would then get every blob refused), and one that ends sooner than it should: the
/// store's own clock decides, second by second.
#[tokio::test]
async fn a_grant_ends_at_the_job_grant_or_ten_minutes_whichever_is_first() {
    assert_eq!(GRANT_LIFETIME, Duration::from_secs(600));
    let source = Source::default();
    let a = many(&source, "a", 2);
    let rig = rig_granting(source, small(), Some(grants()));
    let key = "cas/f/a/0".to_owned();
    let store = store(BUCKET, std::slice::from_ref(&key)).await;

    // The job grant lasts until 1000: the store grant ends ten minutes after now.
    let long = first_token(&rig, a.action, 1000).await;
    store.at(NOW + 599);
    assert_eq!(store.get(&key, &long).await, StatusCode::OK);
    store.at(NOW + 600);
    assert_eq!(store.get(&key, &long).await, StatusCode::FORBIDDEN);

    // The job grant ends at 300: the store grant ends with it.
    let short = first_token(&rig, a.action, 300).await;
    store.at(299);
    assert_eq!(store.get(&key, &short).await, StatusCode::OK);
    store.at(300);
    assert_eq!(store.get(&key, &short).await, StatusCode::FORBIDDEN);

    // The grants follow the clock the job grants are checked by, not a fixed time.
    rig.clock.store(5_000, std::sync::atomic::Ordering::SeqCst);
    let later = first_token(&rig, a.action, 5_100).await;
    store.at(5_000);
    let in_range = store.get(&key, &later).await;
    store.at(5_100);
    assert_eq!(
        (in_range, store.get(&key, &later).await),
        (StatusCode::OK, StatusCode::FORBIDDEN)
    );
}

/// Catches: a header line, or a `g`, in an answer made with no grant key: the lines are exactly
/// the ones before this change, one per digest in digest order, nothing else.
#[tokio::test]
async fn without_a_grant_key_the_answer_is_the_listener_only_table() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec(), vec![7; 2000]]);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    source.large.lock().unwrap().insert(a.files[1], 2000);
    let rig = rig(source, small());
    let reply = rig.locate(&rig.token(a.action)).await;
    assert_eq!(reply.status, StatusCode::OK);

    let mut expected = String::new();
    for digest in a.scope() {
        let line = if digest == a.files[0] {
            json!({"digest": digest.to_string(), "pack": {"key": "cas/f/w/1", "offset": 0, "length": 5}})
        } else if digest == a.files[1] {
            json!({"digest": digest.to_string(), "large": {"key": large_key_of(&digest), "size": 2000}})
        } else {
            json!({"digest": digest.to_string(), "absent": true})
        };
        expected.push_str(&format!("{line}\n"));
    }
    assert_eq!(std::str::from_utf8(&reply.body).unwrap(), expected);
}

/// Catches: a grant that lists a key no line of the answer names (another Action's pack that the
/// index also holds, an object nobody asked about) and a digest outside the scope getting a line
/// or a grant. Every object of the store is tried with every grant: it is readable exactly when a
/// line of this Action's answer names it.
#[tokio::test]
async fn grants_list_only_keys_of_lines_in_the_scope() {
    let source = Source::default();
    let a = many(&source, "a", 4);
    let b = many(&source, "b", 4);
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);

    for digest in b.scope() {
        let text = digest.to_string();
        assert!(
            answer.lines.iter().all(|line| line["digest"] != text),
            "{text} has a line"
        );
    }
    let named = answer.all_keys();
    assert_eq!(named.len(), 4);
    let mut universe: Vec<String> = named.clone();
    universe.extend((0..4).map(|i| format!("cas/f/b/{i}")));
    universe.push("cas/f/stray/0".to_owned());
    let store = store(BUCKET, &universe).await;
    let tokens = answer.tokens();
    for key in &universe {
        let mut readable = false;
        for token in &tokens {
            readable |= store.get(key, token).await == StatusCode::OK;
        }
        assert_eq!(readable, named.contains(key), "{key}");
    }
}

/// Catches: keys tierstore refuses (`_journal/`, `_tiering/`) put in a grant, which would make
/// tierstore refuse the whole token, and their lines left pointing at a key no grant lists:
/// they are `absent`, the others are located, and an answer in which every key is refused has a
/// header with no grants.
#[tokio::test]
async fn keys_tierstore_refuses_are_never_listed() {
    let source = Source::default();
    let a = build(
        &source,
        "a",
        &[b"one".to_vec(), b"two".to_vec(), b"three".to_vec()],
    );
    pack(&source, &a.files[0], span("_journal/seg-1", 0, 3));
    pack(&source, &a.files[1], span("_tiering/map", 0, 3));
    pack(&source, &a.files[2], span("cas/f/w/1", 0, 5));
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);
    for refused in [&a.files[0], &a.files[1]] {
        let line = answer.line(refused);
        assert_eq!(
            line,
            &json!({"digest": refused.to_string(), "absent": true})
        );
    }
    assert_eq!(answer.line(&a.files[2])["g"], 0);
    let store = store(BUCKET, &["cas/f/w/1".to_owned()]).await;
    assert_eq!(
        store.get("cas/f/w/1", &answer.tokens()[0]).await,
        StatusCode::OK
    );

    let source = Source::default();
    let b = build(&source, "b", &[b"one".to_vec()]);
    pack(&source, &b.files[0], span("_journal/seg-1", 0, 3));
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(b.action)).await);
    assert!(answer.tokens().is_empty());
    assert!(answer.lines.iter().all(|line| line["absent"] == true));
}

/// Catches: a header that is left out when nothing in the answer is located: with a key
/// configured the header is the first line whatever the lines say, with no grants, and every line
/// is `absent`.
#[tokio::test]
async fn an_answer_with_nothing_located_still_has_its_header() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec()]);
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);
    assert_eq!(
        answer.header,
        json!({"store": STORE, "bucket": BUCKET, "grants": []})
    );
    assert_eq!(answer.lines.len(), a.scope().len());
    assert!(answer.lines.iter().all(|line| line["absent"] == true));
}

/// A writer that keeps what the logs say.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches: a token (or the grant key's secret) in a `Debug` of the reply that carries it or of
/// the grants that sign it, and in anything logged while grants are made, a refused one's
/// warning included.
#[tokio::test]
async fn a_token_and_the_secret_are_in_no_debug_and_no_log() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);

    let source = Source::default();
    let a = many(&source, "a", 3);
    let granting = grants();
    let shown = format!("{granting:?}");
    assert!(shown.contains(KEY_ID) && !shown.contains(SECRET), "{shown}");
    let rig = rig_granting(source, small(), Some(granting));
    let reply = rig.locate(&rig.token(a.action)).await;
    let tokens = Answer::of(&reply).tokens();
    assert_eq!(tokens.len(), 1);
    let debug = format!("{reply:?} {:?}", reply.body.len());
    assert!(debug.contains("200") || debug.contains("OK"), "{debug}");
    assert!(
        !debug.contains(&tokens[0]) && !debug.contains(SECRET),
        "{debug}"
    );

    // A grant that cannot be made says so in the log, and says nothing of any token.
    let source = Source::default();
    let contents: Vec<Vec<u8>> = (0..80).map(|i| format!("long {i}").into_bytes()).collect();
    let b = build(&source, "b", &contents);
    for (i, digest) in b.files.iter().enumerate() {
        pack(
            &source,
            digest,
            span(
                &format!("cas/f/{}/{i}", "x".repeat(1000)),
                0,
                digest.size_bytes,
            ),
        );
    }
    let unbounded = grants().with_bounds(usize::MAX, usize::MAX);
    let rig = rig_granting(source, small(), Some(unbounded));
    rig.locate(&rig.token(b.action)).await;

    let logged = String::from_utf8(log.0.lock().unwrap().clone()).expect("utf-8 logs");
    assert!(
        logged.contains("could not be made"),
        "the warning is logged: {logged}"
    );
    assert!(
        !logged.contains(&tokens[0]) && !logged.contains(SECRET),
        "{logged}"
    );
    assert!(!logged.contains("tierstore-grant"), "{logged}");
}

/// Catches: a store or bucket that is not the one configured (and nothing of the request in
/// them: a request has no way to name either), and a token for one bucket accepted by a store
/// of another.
#[tokio::test]
async fn the_store_and_the_bucket_are_the_configured_ones() {
    let make = |url: &str, bucket: &str| {
        StoreGrants::new(AccessKey::new(KEY_ID, SECRET), url, bucket).expect("a usable key")
    };
    for (url, bucket) in [
        ("https://tier-a.farm.test", "alpha"),
        ("http://store-b.farm.test:9000", "beta-2"),
    ] {
        let source = Source::default();
        let a = many(&source, "a", 1);
        let rig = rig_granting(source, small(), Some(make(url, bucket)));
        let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);
        assert_eq!(answer.header["store"], url);
        assert_eq!(answer.header["bucket"], bucket);
        // The token is for that bucket only: the store of "tiers" refuses it.
        let other = store(BUCKET, &["cas/f/a/0".to_owned()]).await;
        assert_eq!(
            other.get("cas/f/a/0", &answer.tokens()[0]).await,
            StatusCode::FORBIDDEN
        );
    }
}

/// Catches: a key or bucket tierstore would refuse being found out at the first locate and not
/// at the start.
#[test]
fn a_key_or_bucket_tierstore_refuses_is_refused_when_the_grants_are_made() {
    assert!(StoreGrants::new(AccessKey::new("", SECRET), STORE, BUCKET).is_err());
    assert!(StoreGrants::new(AccessKey::new("id/with/slash", SECRET), STORE, BUCKET).is_err());
    assert!(StoreGrants::new(AccessKey::new(KEY_ID, ""), STORE, BUCKET).is_err());
    assert!(StoreGrants::new(AccessKey::new(KEY_ID, SECRET), STORE, "").is_err());
}

/// Catches: a store URL a task could not append `/<bucket>/<key>` to (no http or https scheme,
/// no host, a path, a query, a fragment, a space or a control character) taken, and one it could
/// use refused.
#[test]
fn a_store_url_is_a_scheme_and_a_host_and_nothing_else() {
    for good in [
        "http://127.0.0.1:9000",
        "https://store.farm.test",
        "https://[::1]:9000/",
    ] {
        assert_eq!(check_store_url(good), Ok(()), "{good}");
    }
    for (bad, why) in [
        ("ftp://store:9000", "http"),
        ("store:9000", "http"),
        ("", "http"),
        ("http://store:9000?x=1", "character"),
        ("http://store:9000#top", "character"),
        ("http://sto re:9000", "character"),
        ("http://sto\u{7}re:9000", "character"),
        ("http://user@store:9000", "character"),
        ("http://store\\9000", "character"),
        ("http://st\u{f6}re:9000", "character"),
        ("https://", "host"),
        ("http://store:9000/tiers", "path"),
    ] {
        let found = check_store_url(bad).expect_err(bad);
        assert!(found.contains(why), "{bad}: {found}");
    }
}

/// Catches: a bucket name tierstore takes and kbf-task's client refuses (so every task would
/// turn the store down) accepted at start, and one the client takes refused.
#[test]
fn a_bucket_is_what_the_task_client_takes() {
    let long = "a".repeat(63);
    for good in ["tiers", "tier-s.2", "a", long.as_str()] {
        assert_eq!(check_store_bucket(good), Ok(()), "{good}");
    }
    let too_long = "a".repeat(64);
    for bad in [
        "",
        "Tiers",
        "a_b",
        "a b",
        ".a",
        "a.",
        "..",
        "t\u{e9}",
        too_long.as_str(),
    ] {
        assert!(check_store_bucket(bad).is_err(), "{bad:?}");
    }
}

/// Catches: the answer drifting from the shape kbf-task parses: the real answer for a fixed
/// scope, source, key and clock is compared line by line (as JSON, so key order is free) with
/// `tests/golden/locate_with_grants.ndjson`, which kbf-task's own tests parse with its
/// `Table::parse` (`locate_tests.rs`, `a_real_answer_of_the_listener_is_a_table`). kbf-front
/// cannot depend on kbf-task, so the two sides share the file.
///
/// The grant is a real token: tierstore's own server accepts it for exactly its two keys.
#[tokio::test]
async fn the_answer_is_the_golden_file_kbf_task_parses() {
    let source = Source::default();
    let contents = [b"alpha".to_vec(), vec![7; 2000], b"delta".to_vec()];
    let a = build(&source, "a", &contents);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    source.large.lock().unwrap().insert(a.files[1], 2000);
    let rig = rig_granting(source, small(), Some(grants()));
    let reply = rig.locate(&rig.token(a.action)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let text = std::str::from_utf8(&reply.body).expect("utf-8");
    let golden = include_str!("golden/locate_with_grants.ndjson");
    let lines = |t: &str| -> Vec<Value> {
        t.lines()
            .map(|l| serde_json::from_str(l).expect("JSON"))
            .collect()
    };
    assert_eq!(lines(text), lines(golden));

    let answer = Answer::of(&reply);
    let store = store(BUCKET, &answer.all_keys()).await;
    for key in answer.all_keys() {
        assert_eq!(
            store.get(&key, &answer.tokens()[0]).await,
            StatusCode::OK,
            "{key}"
        );
    }
}

/// Catches: a grant minted here that tierstore's server does not accept, or accepts for more
/// than it lists: the listed objects are read whole, ranged and by HEAD; an object that is in the
/// store but not listed, a write and a listing are refused.
#[tokio::test]
async fn a_grant_minted_here_is_accepted_by_tierstore_for_exactly_the_listed_keys() {
    let source = Source::default();
    let a = build(&source, "a", &[b"alpha".to_vec(), vec![7; 2000]]);
    pack(&source, &a.files[0], span("cas/f/w/1", 0, 5));
    source.large.lock().unwrap().insert(a.files[1], 2000);
    let rig = rig_granting(source, small(), Some(grants()));
    let answer = Answer::of(&rig.locate(&rig.token(a.action)).await);
    let listed = answer.all_keys();
    assert_eq!(listed, ["cas/f/w/1".to_owned(), large_key_of(&a.files[1])]);

    let mut everything = listed.clone();
    everything.push("cas/f/w/2".to_owned());
    everything.push("cas/l/not-listed".to_owned());
    let store = store(BUCKET, &everything).await;
    let token = &answer.tokens()[0];
    for key in &listed {
        assert_eq!(store.get(key, token).await, StatusCode::OK, "{key}");
        assert_eq!(
            store.send("HEAD", key, token).await,
            StatusCode::OK,
            "{key}"
        );
        assert_eq!(
            store.send("PUT", key, token).await,
            StatusCode::FORBIDDEN,
            "{key}"
        );
        assert_eq!(
            store.send("DELETE", key, token).await,
            StatusCode::FORBIDDEN,
            "{key}"
        );
    }
    for key in ["cas/f/w/2", "cas/l/not-listed"] {
        assert_eq!(store.get(key, token).await, StatusCode::FORBIDDEN, "{key}");
    }
    // The refusals of a GET are the listener's, with no grants in them.
    let none = rig.read.respond(&Method::GET, "/locate", None).await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    assert_eq!(&none.body[..], b"missing_token\n");
}
