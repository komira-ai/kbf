//! A fake coordinator that keeps a desired state as the real one does, for the tests of
//! `deploy-farm restore`: every accepted write raises the generation, a change of version
//! records what it replaced, a write names its `write_id`, and a write made on another
//! generation (`if_generation`) is a 409 that changes nothing. It is driven by handshakes
//! only: what it does is chosen by the number of the request (the n-th POST, the n-th read),
//! never by time.
//!
//! It also refuses a bearer token it has seen before with 401, as the coordinator refuses a
//! token whose `jti` it has seen.
//!
//! A test file includes it after `deploy_fake`, whose TLS listener and request reader it uses.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::deploy_fake::{Listening, RUNNER_KEY, Seen, listen, read_request, response};

/// The release the tests deploy, and the one the farm ran before it.
pub const RELEASE: &str = "2.0.0+g000000000002";
pub const BEFORE: &str = "1.0.0+g000000000001";

/// A desired state and the record of its writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub version: String,
    pub min_up: Value,
    pub generation: u64,
    pub replaced: Option<(String, Value)>,
    pub write_id: String,
}

impl State {
    /// The farm after `BEFORE` was set (generation 1, as by a write without an id).
    pub fn before() -> Self {
        Self {
            version: BEFORE.to_owned(),
            min_up: json!({"storage": 2, "frontend": 1}),
            generation: 1,
            replaced: None,
            write_id: String::new(),
        }
    }

    /// The farm after an earlier attempt set `RELEASE` over `BEFORE`.
    pub fn released() -> Self {
        Self {
            version: RELEASE.to_owned(),
            min_up: json!({"storage": 1, "frontend": 1}),
            generation: 2,
            replaced: Some((BEFORE.to_owned(), json!({"storage": 2, "frontend": 1}))),
            write_id: String::new(),
        }
    }
}

/// What the fake does with a POST.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Applies it and answers.
    Answer,
    /// Applies it and closes the connection without an answer.
    Drop,
    /// Applies it and answers 503.
    Lands503,
    /// Answers this status and applies nothing.
    Status(u16),
}

/// A write of another writer, accepted right after the read with this number (from 0) is
/// answered: between that read and the caller's next call.
#[derive(Clone, Debug)]
pub struct Competing {
    pub at_read: usize,
    pub version: String,
    pub min_up: Value,
    pub write_id: String,
}

/// What the fake is and does.
pub struct Script {
    /// The state before the first request; none before the first deploy.
    pub state: Option<State>,
    /// The state the convergence read reports and the storage hosts at the desired version,
    /// one per read (the first is the deploy's baseline); the last repeats. The farm has two
    /// storage hosts and one daemon.
    pub farm: Vec<(&'static str, u64)>,
    /// What happens to each POST, by number; a POST past the end is answered.
    pub fates: Vec<Fate>,
    /// Writes of other writers.
    pub competing: Vec<Competing>,
    /// Whether the reads show `generation`, `replaced` and `write_id` (a coordinator older
    /// than the restore shows none of them).
    pub generation_shown: bool,
}

impl Script {
    /// A farm that ran `BEFORE` and converges on whatever is set.
    pub fn new() -> Self {
        Self {
            state: Some(State::before()),
            farm: vec![("converged", 2)],
            fates: Vec::new(),
            competing: Vec::new(),
            generation_shown: true,
        }
    }
}

/// One POST the fake saw, with what came of it.
#[derive(Clone, Debug)]
pub struct Post {
    pub body: Value,
    /// The status answered (0 for a dropped answer).
    pub status: u16,
}

#[derive(Default)]
struct Inner {
    state: Option<State>,
    reads: usize,
    posts: Vec<Post>,
    tokens: usize,
    used: BTreeSet<String>,
    seen: Vec<Seen>,
}

/// The fake, serving.
pub struct Fake {
    pub base: String,
    pub ca: rustls::pki_types::CertificateDer<'static>,
    pub ca_pem: String,
    inner: Arc<Mutex<Inner>>,
}

impl Fake {
    /// The state now.
    pub fn state(&self) -> Option<State> {
        self.inner.lock().expect("lock").state.clone()
    }

    /// Every POST of `/v1/desired`, in order.
    pub fn posts(&self) -> Vec<Post> {
        self.inner.lock().expect("lock").posts.clone()
    }

    /// Every request to the coordinator (not to the token service), in order.
    pub fn seen(&self) -> Vec<Seen> {
        self.inner.lock().expect("lock").seen.clone()
    }

    /// Whether the fake refuses `token` now, as it would a replayed one.
    pub fn refuses(&self, token: &str) -> bool {
        self.inner.lock().expect("lock").used.contains(token)
    }
}

/// Applies a write to `state` as the coordinator does: 409 when `if_generation` is given and
/// is not the generation, else the state replaced, the generation raised, the version replaced
/// recorded when it changes, and the write's id kept.
fn apply(state: &mut Option<State>, body: &Value) -> u16 {
    let generation = state.as_ref().map_or(0, |s| s.generation);
    if let Some(asked) = body.get("if_generation")
        && asked.as_u64() != Some(generation)
    {
        return 409;
    }
    let version = body["version"].as_str().expect("a version").to_owned();
    let replaced = match state.as_ref() {
        Some(old) if old.version != version => Some((old.version.clone(), old.min_up.clone())),
        Some(old) => old.replaced.clone(),
        None => None,
    };
    *state = Some(State {
        version,
        min_up: body["min_up"].clone(),
        generation: generation + 1,
        replaced,
        write_id: body["write_id"].as_str().unwrap_or_default().to_owned(),
    });
    200
}

fn read_answer(script: &Script, state: Option<&State>, read: usize) -> Value {
    let (farm, at) = script.farm[read.min(script.farm.len() - 1)];
    let mut answer = json!({
        "desired": state.map(|s| s.version.clone()),
        "quiet": false,
        "state": farm,
        "roles": [
            {"role": "storage", "at_desired": at, "updating": 0, "failed": 0, "down": 0},
            {"role": "daemon", "at_desired": 1, "updating": 0, "failed": 0, "down": 0},
        ],
    });
    if script.generation_shown {
        answer["generation"] = json!(state.map_or(0, |s| s.generation));
        answer["replaced"] = state
            .and_then(|s| s.replaced.clone())
            .map_or(Value::Null, |(v, m)| json!({"version": v, "min_up": m}));
        answer["write_id"] = json!(state.map(|s| s.write_id.clone()).unwrap_or_default());
    }
    answer
}

/// The fake on loopback over TLS, answering by `script`.
pub fn serve(script: Script) -> Fake {
    let Listening {
        listener,
        config,
        base,
        ca,
        ca_pem,
    } = listen();
    let inner = Arc::new(Mutex::new(Inner {
        state: script.state.clone(),
        ..Inner::default()
    }));
    let shared = Arc::clone(&inner);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
            let mut tls = rustls::StreamOwned::new(conn, stream.expect("accept"));
            let Some(request) = read_request(&mut tls) else {
                continue;
            };
            let answer = {
                let mut inner = shared.lock().expect("lock");
                answer(&script, &mut inner, request)
            };
            if let Some(answer) = answer {
                use std::io::Write;
                let _ = tls.write_all(&answer);
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        }
    });
    Fake {
        base,
        ca,
        ca_pem,
        inner,
    }
}

/// The bytes to answer `request` with; none to close without an answer.
fn answer(script: &Script, inner: &mut Inner, request: Seen) -> Option<Vec<u8>> {
    if request.path.starts_with("/token") {
        if request.authorization != format!("Bearer {RUNNER_KEY}") {
            return Some(response(401, "{}"));
        }
        inner.tokens += 1;
        return Some(response(
            200,
            &json!({"value": format!("tok-{}", inner.tokens)}).to_string(),
        ));
    }
    inner.seen.push(request.clone());
    let token = request
        .authorization
        .strip_prefix("Bearer ")
        .unwrap_or_default()
        .to_owned();
    if !token.starts_with("tok-") || !inner.used.insert(token) {
        return Some(response(401, r#"{"error":"unauthorized"}"#));
    }
    if request.method == "POST" {
        let body: Value = serde_json::from_str(&request.body).expect("a JSON body");
        let fate = script
            .fates
            .get(inner.posts.len())
            .copied()
            .unwrap_or(Fate::Answer);
        let status = match fate {
            Fate::Status(status) => status,
            _ => apply(&mut inner.state, &body),
        };
        let answered = match (fate, status) {
            (Fate::Drop, 200) => 0,
            (Fate::Lands503, 200) => 503,
            _ => status,
        };
        inner.posts.push(Post {
            body,
            status: answered,
        });
        return match answered {
            0 => None,
            200 => Some(response(200, "{}")),
            409 => Some(response(409, r#"{"error":"conflict"}"#)),
            other => Some(response(other, "{}")),
        };
    }
    let read = inner.reads;
    inner.reads += 1;
    let answer = read_answer(script, inner.state.as_ref(), read);
    for write in script.competing.iter().filter(|w| w.at_read == read) {
        let body =
            json!({"version": write.version, "min_up": write.min_up, "write_id": write.write_id});
        assert_eq!(apply(&mut inner.state, &body), 200, "the competing write");
    }
    Some(response(200, &answer.to_string()))
}
