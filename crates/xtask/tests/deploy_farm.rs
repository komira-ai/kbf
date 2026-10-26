//! `deploy-farm` against a fake coordinator and a fake runner token service on loopback, over
//! TLS with a CA of the test's own, and the binary for its flags and exit codes.

use std::cell::{Cell, RefCell};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use xtask::deploy_farm::{
    Args, Clock, Error, Plan, Roots, TokenService, deploy_on, pem_roots, run_with,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const RUNNER_KEY: &str = "runner-key";

/// One request the fake saw.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    body: String,
}

/// What the fake coordinator answers.
struct Script {
    /// The answers of `GET /v1/convergence`, one per read (the first is the baseline); the
    /// last repeats. A number is a
    /// status with no body.
    reads: Vec<Result<Value, u16>>,
    /// The status of `POST /v1/desired`.
    post: u16,
    /// How many of the first POSTs answer 503 before `post` is used.
    post_503s: usize,
    /// The status of the runner's token service.
    token: u16,
    /// What the token service answers instead of a token, as bytes.
    token_answer: Option<Vec<u8>>,
}

impl Script {
    /// A script whose first answer is the baseline read before the POST (a farm of two
    /// storage hosts and one daemon, one more daemon down), then `reads`.
    fn new(reads: Vec<Result<Value, u16>>) -> Self {
        Self::after(Ok(farm("converged", 0, 0)), reads)
    }

    /// A script whose baseline read is `baseline`, then `reads`.
    fn after(baseline: Result<Value, u16>, reads: Vec<Result<Value, u16>>) -> Self {
        let mut all = vec![baseline];
        all.extend(reads);
        Self {
            reads: all,
            post: 200,
            post_503s: 0,
            token: 200,
            token_answer: None,
        }
    }
}

struct Fake {
    base: String,
    ca: CertificateDer<'static>,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn requests(&self, path: &str) -> Vec<Seen> {
        let seen = self.seen.lock().expect("lock");
        seen.iter()
            .filter(|r| r.path.starts_with(path))
            .cloned()
            .collect()
    }

    fn service(&self) -> TokenService {
        TokenService {
            url: format!("{}/token?api-version=2.0", self.base),
            key: RUNNER_KEY.to_owned(),
        }
    }

    fn plan(&self) -> Plan {
        Plan {
            url: self.base.clone(),
            audience: self.base.clone(),
            version: "2.0".to_owned(),
            min_up_storage: 1,
            min_up_frontend: 1,
            deadline: Duration::from_secs(30),
            stall: Duration::from_secs(30),
            every: Duration::from_secs(1),
        }
    }
}

fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn read_request(stream: impl Read) -> Option<Seen> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let mut words = first.split_whitespace();
    let method = words.next()?.to_owned();
    let path = words.next()?.to_owned();
    let (mut length, mut authorization) = (0, String::new());
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "authorization" => authorization = value.trim().to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        authorization,
        body: String::from_utf8(body).ok()?,
    })
}

/// A TLS server on loopback that answers by `script`.
fn serve(script: Script) -> Fake {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .expect("server config"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let tokens = AtomicUsize::new(0);
    let reads = AtomicUsize::new(0);
    let posts = AtomicUsize::new(0);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
            let mut tls = rustls::StreamOwned::new(conn, stream.expect("accept"));
            let Some(request) = read_request(&mut tls) else {
                continue;
            };
            let answer = if request.path.starts_with("/token") {
                if request.authorization != format!("Bearer {RUNNER_KEY}") || script.token != 200 {
                    response(script.token.max(401), "{}")
                } else if let Some(raw) = &script.token_answer {
                    let mut answer = format!(
                        "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        raw.len()
                    )
                    .into_bytes();
                    answer.extend_from_slice(raw);
                    answer
                } else {
                    let n = tokens.fetch_add(1, Ordering::SeqCst);
                    response(200, &json!({"value": format!("tok-{n}")}).to_string())
                }
            } else if !request.authorization.starts_with("Bearer tok-") {
                response(401, r#"{"error":"unauthorized"}"#)
            } else if request.method == "POST" {
                if posts.fetch_add(1, Ordering::SeqCst) < script.post_503s {
                    response(503, "{}")
                } else {
                    response(script.post, "{}")
                }
            } else if request.path == "/v1/elsewhere" {
                response(200, &farm("converged", 0, 0).to_string())
            } else {
                let at = reads
                    .fetch_add(1, Ordering::SeqCst)
                    .min(script.reads.len() - 1);
                match &script.reads[at] {
                    Ok(body) => response(200, &body.to_string()),
                    Err(302) => b"HTTP/1.1 302 X\r\nLocation: /v1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    Err(status) => response(*status, "{}"),
                }
            };
            log.lock().expect("lock").push(request);
            let _ = tls.write_all(&answer);
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    Fake {
        base,
        ca: ca.der().clone(),
        ca_pem: ca.pem(),
        seen,
    }
}

fn farm(state: &str, updating: u64, failed: u64) -> Value {
    json!({"desired": "2.0", "quiet": false, "state": state, "roles": [
        {"role": "storage", "at_desired": 2, "updating": updating, "failed": failed, "down": 0},
        {"role": "daemon", "at_desired": 1, "updating": 0, "failed": 0, "down": 1},
    ]})
}

/// A virtual clock: `sleep` moves `now` and is recorded, so no test waits for a deadline.
#[derive(Default)]
struct Time {
    now: Cell<Duration>,
    slept: RefCell<Vec<Duration>>,
}

impl Time {
    fn clock(&self) -> Clock<'_> {
        Clock {
            now: Box::new(|| self.now.get()),
            sleep: Box::new(|d| {
                self.now.set(self.now.get() + d);
                self.slept.borrow_mut().push(d);
            }),
        }
    }
}

/// `deploy_on` a fresh virtual clock.
fn deploy(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    deploy_on(plan, service, roots, &Time::default().clock(), out)
}

fn run(fake: &Fake, plan: &Plan) -> (Result<(), Error>, String) {
    run_timed(fake, plan, &Time::default())
}

fn run_timed(fake: &Fake, plan: &Plan, time: &Time) -> (Result<(), Error>, String) {
    let mut out = Vec::new();
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let result = deploy_on(plan, &fake.service(), &roots, &time.clock(), &mut out);
    (result, String::from_utf8(out).expect("utf8"))
}

#[test]
fn the_desired_version_is_posted_then_the_farm_is_polled_to_converged() {
    // Proves: one read of the farm, one POST of the version and the minimums, then reads until `converged`;
    // exit 0; each read's table is printed; every call carries a token of its own, asked
    // for the coordinator's audience and masked in the log.
    let mut first = farm("in_progress", 1, 0);
    first["quiet"] = json!(true);
    let fake = serve(Script::new(vec![
        Ok(first),
        Ok(farm("in_progress", 0, 0)),
        Ok(farm("converged", 0, 0)),
    ]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let posts = fake.requests("/v1/desired");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].method, "POST");
    let body: Value = serde_json::from_str(&posts[0].body).expect("json");
    assert_eq!(
        body,
        json!({"version": "2.0", "min_up": {"storage": 1, "frontend": 1}})
    );
    assert_eq!(fake.requests("/v1/convergence").len(), 4);
    let tokens: Vec<String> = ["/v1/desired", "/v1/convergence"]
        .iter()
        .flat_map(|p| fake.requests(p))
        .map(|r| r.authorization)
        .collect();
    assert_eq!(tokens.len(), 5);
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 5, "a token is used once: {tokens:?}");
    let asked = fake.requests("/token");
    assert_eq!(asked.len(), 5);
    let audience = format!(
        "audience=https%3A%2F%2F127.0.0.1%3A{}",
        fake.base.rsplit(':').next().expect("port")
    );
    assert!(
        asked.iter().all(|r| r.path.ends_with(&audience)),
        "{asked:?}"
    );
    for n in 0..5 {
        assert!(out.contains(&format!("::add-mask::tok-{n}")), "{out}");
    }
    assert!(
        out.contains("farm: in_progress, desired 2.0 (coordinator just started)"),
        "{out}"
    );
    assert!(out.contains("farm: converged, desired 2.0"), "{out}");
    assert!(out.contains("storage"), "{out}");
    assert!(out.contains("daemon"), "{out}");
}

#[test]
fn a_blocked_farm_fails_at_once_with_its_table_printed() {
    // Proves: `blocked` ends the wait without further reads, as an error, after the table
    // that shows the failed host was printed.
    let fake = serve(Script::new(vec![
        Ok(farm("in_progress", 1, 0)),
        Ok(farm("blocked", 0, 1)),
        Ok(farm("converged", 0, 0)),
    ]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Err(Error::Blocked("2.0".to_owned())));
    assert_eq!(fake.requests("/v1/convergence").len(), 3);
    assert!(out.contains("farm: blocked, desired 2.0"), "{out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("storage") && l.split_whitespace().nth(3) == Some("1")),
        "{out}"
    );
}

#[test]
fn a_farm_that_keeps_moving_but_does_not_converge_fails_at_the_deadline() {
    // Proves: counts that change on every read are progress, so only the deadline ends
    // the wait; it ends it as an error, after a read at the deadline, and the last sleep
    // is cut short to land on the deadline (3.5 s of deadline, 1 s between reads).
    let reads = (0..40).map(|n| Ok(farm("in_progress", n % 3, 0))).collect();
    let fake = serve(Script::new(reads));
    let mut plan = fake.plan();
    plan.deadline = Duration::from_millis(3500);
    let time = Time::default();
    let (result, out) = run_timed(&fake, &plan, &time);
    assert_eq!(result, Err(Error::Deadline(3)), "{out}");
    let second = Duration::from_secs(1);
    assert_eq!(
        *time.slept.borrow(),
        [second, second, second, Duration::from_millis(500)]
    );
    // The read before the deploy, then the reads at 0, 1, 2, 3 and 3.5 s.
    assert_eq!(fake.requests("/v1/convergence").len(), 6);
}

#[test]
fn counts_that_do_not_change_fail_as_a_stall_before_the_deadline() {
    // Proves: the same counts for the stall time end the wait as a stall, though the
    // deadline is far off: reads at 0, 1, 2 and 3 s, and the stall is judged at the
    // fourth.
    let fake = serve(Script::new(vec![Ok(farm("in_progress", 1, 0))]));
    let mut plan = fake.plan();
    plan.stall = Duration::from_secs(3);
    let time = Time::default();
    let (result, _) = run_timed(&fake, &plan, &time);
    assert_eq!(result, Err(Error::Stalled(3)));
    assert_eq!(time.slept.borrow().len(), 3);
    assert_eq!(fake.requests("/v1/convergence").len(), 5);
}

#[test]
fn a_read_that_fails_is_tried_again_and_a_refused_one_is_not() {
    // Proves: a 503 on a read is reported and the wait goes on to converged; a 401 on a read
    // ends the wait at once as a refusal, without a second read; a 401 on the read before the
    // deploy ends it before anything is posted.
    let fake_ok = serve(Script::new(vec![Err(503), Ok(farm("converged", 0, 0))]));
    let (result, out) = run(&fake_ok, &fake_ok.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("the read failed, trying again"), "{out}");
    let early = serve(Script::after(Err(401), vec![]));
    let (result, _) = run(&early, &early.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "read the farm before the deploy",
            status: 401
        })
    );
    assert!(early.requests("/v1/desired").is_empty());
    let fake_no = serve(Script::new(vec![Err(401), Ok(farm("converged", 0, 0))]));
    let (result, _) = run(&fake_no, &fake_no.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "read the convergence",
            status: 401
        })
    );
    assert_eq!(fake_no.requests("/v1/convergence").len(), 2);
}

#[test]
fn a_refused_post_reads_nothing() {
    // Proves: when the coordinator refuses the token on the POST (403), the verb fails with
    // the refusal and makes no read but the baseline.
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post = 403;
    let fake = serve(script);
    let (result, _) = run(&fake, &fake.plan());
    assert_eq!(
        result,
        Err(Error::Refused {
            what: "set the desired version",
            status: 403
        })
    );
    assert_eq!(fake.requests("/v1/convergence").len(), 1);
    let mut script = Script::new(vec![]);
    script.post = 400;
    let bad = serve(script);
    let (result, _) = run(&bad, &bad.plan());
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "set the desired version",
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn a_post_the_coordinator_could_not_serve_is_tried_again_and_a_final_one_is_not() {
    // Proves: a 503 on the POST (the coordinator could not reach the token issuer) is
    // reported and tried again, and the deploy goes on to converged; a 503 that never ends
    // fails after a few tries, not at once and not forever.
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 2;
    let fake = serve(script);
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert_eq!(fake.requests("/v1/desired").len(), 3);
    assert!(
        out.contains("the call failed, trying again: set the desired version"),
        "{out}"
    );
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 100;
    let down = serve(script);
    let (result, _) = run(&down, &down.plan());
    assert!(matches!(result, Err(Error::Call { .. })), "{result:?}");
    assert_eq!(down.requests("/v1/desired").len(), 4);
    assert_eq!(down.requests("/v1/convergence").len(), 1);
}

#[test]
fn a_convergence_on_another_version_is_not_this_deploys() {
    // Proves: when the coordinator's desired version is not the one this deploy posted
    // (another writer replaced it), a `converged` answer ends the deploy as an error naming
    // the other version, and so does an in-progress one.
    for state in ["converged", "in_progress"] {
        let mut other = farm(state, 0, 0);
        other["desired"] = json!("3.0");
        let fake = serve(Script::new(vec![Ok(other)]));
        let (result, _) = run(&fake, &fake.plan());
        assert_eq!(result, Err(Error::Superseded("3.0".to_owned())));
        assert_eq!(fake.requests("/v1/convergence").len(), 2);
    }
}

#[test]
fn what_the_coordinator_prints_cannot_start_a_workflow_command() {
    // Proves: a version or role that starts with `::` or holds a line break reaches the log
    // as one plain line, so no line of the baseline or the table is a workflow command.
    let mut hostile = farm("converged", 0, 0);
    hostile["desired"] = json!("2.0");
    hostile["roles"][0]["role"] = json!("::error::boom\n::add-mask::x");
    let fake = serve(Script::after(Ok(hostile.clone()), vec![Ok(hostile)]));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("error::boom::add-mask::x"), "{out}");
    assert!(
        out.contains("baseline: hosts live before the deploy: error::boom::add-mask::x 2"),
        "{out}"
    );
    let masks = out.lines().filter(|l| l.starts_with("::")).count();
    assert_eq!(masks, 3, "only the client's own masks: {out}");
    assert!(!out.lines().any(|l| l.starts_with("::error")), "{out}");
}

#[test]
fn a_redirect_from_the_coordinator_is_not_followed() {
    // Proves: a 302 on the read is a failed read, never a second request to the place it
    // names; the wait ends as a stall.
    let fake = serve(Script::new(vec![Err(302)]));
    let mut plan = fake.plan();
    plan.stall = Duration::from_secs(3);
    let (result, out) = run(&fake, &plan);
    assert_eq!(result, Err(Error::Stalled(3)), "{out}");
    assert!(fake.requests("/v1/elsewhere").is_empty());
}

#[test]
fn a_runner_with_no_token_and_an_answer_that_is_no_table_fail_with_reasons() {
    // Proves: a token service that fails ends the verb before anything is sent to the
    // coordinator; an answer without the fields of the table is an error that says so, not a
    // panic or a silent wait.
    let mut script = Script::new(vec![Ok(json!({}))]);
    script.token = 500;
    let down = serve(script);
    let (result, _) = run(&down, &down.plan());
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "ask the runner for an OIDC token",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(down.requests("/v1/desired").is_empty());
    for answer in [
        json!({}),
        json!({"state": "converged"}),
        json!({"state": "converged", "roles": [{"role": "storage"}]}),
        json!({"state": "converged", "roles": [{"at_desired": 1, "updating": 0, "failed": 0, "down": 0}]}),
    ] {
        let odd = serve(Script::new(vec![Ok(answer)]));
        let (result, _) = run(&odd, &odd.plan());
        assert!(matches!(result, Err(Error::Answer(_))), "{result:?}");
    }
    for answer in [
        json!({}),
        json!({"state": "converged", "roles": [{"role": "storage"}]}),
    ] {
        let odd = serve(Script::after(Ok(answer), vec![]));
        let (result, _) = run(&odd, &odd.plan());
        assert!(matches!(result, Err(Error::Answer(_))), "{result:?}");
        assert!(odd.requests("/v1/desired").is_empty());
    }
    let empty = serve(Script::new(vec![Ok(
        json!({"state": "in_progress", "roles": []}),
    )]));
    let mut plan = empty.plan();
    plan.stall = Duration::from_secs(3);
    let (_, out) = run(&empty, &plan);
    assert!(out.contains("desired (none)"), "{out}");
}

/// A writer that fails once more than `room` bytes have been written.
struct Room {
    room: usize,
}

impl Write for Room {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.room {
            return Err(io::Error::other("full"));
        }
        self.room -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_token_the_service_does_not_give_is_an_error_whatever_it_says() {
    // Proves: an answer with no `value`, an empty one, one that is not JSON and one that is
    // not text each end the deploy at the token, before the coordinator is called; a token
    // service address with no query gets its `audience` after a `?`.
    for raw in [&b"{}"[..], br#"{"value":""}"#, b"not json", b"\xff\xfe"] {
        let mut script = Script::new(vec![]);
        script.token_answer = Some(raw.to_vec());
        let down = serve(script);
        let mut service = down.service();
        service.url = format!("{}/token", down.base);
        let mut out = Vec::new();
        let ca = [down.ca.clone()];
        let roots = Roots {
            runner: &ca,
            coordinator: &ca,
        };
        let result = deploy(&down.plan(), &service, &roots, &mut out);
        assert!(
            matches!(
                result,
                Err(Error::Call {
                    what: "ask the runner for an OIDC token",
                    ..
                })
            ),
            "{raw:?}: {result:?}"
        );
        assert!(down.requests("/v1/desired").is_empty());
        assert!(
            down.requests("/token?audience=").len() == 1,
            "{:?}",
            down.requests("/token")
        );
    }
}

#[test]
fn a_log_that_cannot_be_written_ends_the_deploy_with_an_error() {
    // Proves: a failed write of the log (a closed pipe) is an error, not a panic or a silent
    // wait, at each line of the deploy: the masking of a token, the baseline, the notice that
    // the version is set, a table, and the note that a call is tried again.
    let mask = "::add-mask::tok-0\n".len();
    let baseline = "baseline: hosts live before the deploy: storage 2, daemon 1\n".len();
    let desired = "desired version 2.0 set\n".len();
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    for room in [
        0,
        mask,
        mask + baseline,
        mask + baseline + mask,
        mask + baseline + mask + desired,
        mask + baseline + mask + desired + mask,
    ] {
        let result = deploy(&fake.plan(), &fake.service(), &roots, &mut Room { room });
        assert!(
            matches!(result, Err(Error::Output(_))),
            "{room}: {result:?}"
        );
    }
    let mut script = Script::new(vec![Ok(farm("converged", 0, 0))]);
    script.post_503s = 1;
    let retried = serve(script);
    let ca = [retried.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let result = deploy(
        &retried.plan(),
        &retried.service(),
        &roots,
        &mut Room {
            room: mask + baseline + mask,
        },
    );
    assert!(matches!(result, Err(Error::Output(_))), "{result:?}");
}

/// A farm answer with the given counts per role: (role, at_desired, updating, failed, down).
fn counts(state: &str, desired: &str, roles: &[(&str, u64, u64, u64, u64)]) -> Value {
    let rows: Vec<Value> = roles
        .iter()
        .map(|(role, at, updating, failed, down)| {
            json!({"role": role, "at_desired": at, "updating": updating, "failed": failed, "down": down})
        })
        .collect();
    json!({"desired": desired, "quiet": false, "state": state, "roles": rows})
}

#[test]
fn a_host_that_goes_down_during_the_deploy_fails_it_though_the_farm_says_converged() {
    // Proves: three storage hosts live before the deploy, one stops heartbeating, and the
    // coordinator (which answers for the hosts it hears from) says `converged`: the deploy
    // fails naming the role, the 3 expected, the 2 reached and the 1 that went down.
    let fake = serve(Script::after(
        Ok(counts("in_progress", "1.0", &[("storage", 3, 0, 0, 0)])),
        vec![
            Ok(counts("in_progress", "2.0", &[("storage", 1, 2, 0, 0)])),
            Ok(counts("converged", "2.0", &[("storage", 2, 0, 0, 1)])),
        ],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(
        result,
        Err(Error::Lost {
            role: "storage".to_owned(),
            expected: 3,
            reached: 2,
            version: "2.0".to_owned()
        }),
        "{out}"
    );
    let message = result.expect_err("lost").to_string();
    assert!(message.contains("storage"), "{message}");
    assert!(message.contains("3 hosts were live"), "{message}");
    assert!(message.contains("2 run 2.0"), "{message}");
    assert!(message.contains("1 went down"), "{message}");
}

#[test]
fn a_host_that_was_down_before_the_deploy_does_not_block_it() {
    // Proves: a host already down at the baseline is not expected: two storage hosts live and
    // one down before, two at the version after, passes (and the daemon role, with its one
    // host down at both reads and none live, is not expected either).
    let fake = serve(Script::after(
        Ok(counts(
            "in_progress",
            "1.0",
            &[("storage", 2, 0, 0, 1), ("daemon", 0, 0, 0, 1)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0",
            &[("storage", 2, 0, 0, 1), ("daemon", 0, 0, 0, 1)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
}

#[test]
fn a_host_that_joins_during_the_deploy_is_fine() {
    // Proves: more hosts at the version than were live at the baseline, and a role the
    // baseline did not list at all, pass; so does a baseline with no host at all (printed as
    // "none").
    let fake = serve(Script::after(
        Ok(counts("in_progress", "1.0", &[("storage", 1, 0, 0, 0)])),
        vec![Ok(counts(
            "converged",
            "2.0",
            &[("storage", 3, 0, 0, 0), ("frontend", 1, 0, 0, 0)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let fake = serve(Script::after(
        Ok(counts("in_progress", "1.0", &[])),
        vec![Ok(counts("converged", "2.0", &[("storage", 1, 0, 0, 0)]))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(out.contains("before the deploy: none"), "{out}");
}

#[test]
fn a_role_that_vanishes_from_the_answer_counts_as_all_its_hosts_lost() {
    // Proves: a baseline role the final answer does not list has no host at the version.
    let fake = serve(Script::after(
        Ok(counts("in_progress", "1.0", &[("storage", 2, 0, 0, 0)])),
        vec![Ok(counts("converged", "2.0", &[("frontend", 1, 0, 0, 0)]))],
    ));
    let (result, _) = run(&fake, &fake.plan());
    assert!(
        matches!(
            result,
            Err(Error::Lost {
                expected: 2,
                reached: 0,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn the_hosts_live_before_the_deploy_are_printed_before_the_post() {
    // Proves: updating and failed hosts count as live and down ones do not, per role, in a
    // line printed before the desired version is set.
    let fake = serve(Script::after(
        Ok(counts(
            "blocked",
            "1.0",
            &[("storage", 2, 1, 1, 5), ("frontend", 1, 0, 0, 0)],
        )),
        vec![Ok(counts(
            "converged",
            "2.0",
            &[("storage", 4, 0, 0, 5), ("frontend", 1, 0, 0, 0)],
        ))],
    ));
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    let line = "baseline: hosts live before the deploy: storage 4, frontend 1";
    let at = out.find(line).unwrap_or_else(|| panic!("{out}"));
    let posted = out.find("desired version 2.0 set").expect("posted");
    assert!(at < posted, "{out}");
}

#[test]
fn a_503_on_the_read_before_the_deploy_is_tried_again() {
    // Proves: the baseline read is retried on a 503 like the POST, and the deploy goes on.
    let fake = serve(Script::after(Err(503), vec![Ok(farm("converged", 0, 0))]));
    // The first read is a 503, the second answers; the baseline is the second.
    let (result, out) = run(&fake, &fake.plan());
    assert_eq!(result, Ok(()), "{out}");
    assert!(
        out.contains("the call failed, trying again: read the farm before the deploy"),
        "{out}"
    );
}

fn args_for(fake: &Fake, ca_pem: &str) -> Args {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }
    Cli::parse_from([
        "x",
        "--url",
        &fake.base,
        "--version",
        "2.0",
        "--ca-pem",
        ca_pem,
        "--every-secs",
        "1",
    ])
    .args
}

#[test]
fn the_verb_trusts_the_system_roots_or_the_ca_it_is_given() {
    // Proves: with no `--ca-pem` the coordinator is trusted by the system roots given; with one
    // it is trusted by that CA instead (a CA that did not sign its certificate refuses it
    // though the system roots would have trusted it), and the verb's line says what converged.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let system = [fake.ca.clone()];
    let mut out = Vec::new();
    let lines = run_with(&args_for(&fake, ""), &fake.service(), &system, &mut out).expect("run");
    assert_eq!(
        lines,
        [format!("ok: every live host runs 2.0 ({})", fake.base)]
    );
    let lines = run_with(
        &args_for(&fake, &fake.ca_pem),
        &fake.service(),
        &system,
        &mut out,
    )
    .expect("pinned to its own CA");
    assert_eq!(lines.len(), 1);
    let (_, other_pem) = fake_ca();
    let refused = run_with(
        &args_for(&fake, &other_pem),
        &fake.service(),
        &system,
        &mut out,
    );
    let Err(xtask::Error::Failed(why)) = refused else {
        panic!("a certificate of another CA was trusted: {refused:?}");
    };
    assert!(why.contains("read the farm before the deploy"), "{why}");
    let posts = fake.requests("/v1/desired").len();
    assert_eq!(posts, 2, "the pinned-to-another-CA run sent nothing");
}

#[test]
fn only_an_https_address_without_a_path_is_used() {
    // Proves: a plain-http address, a path and an empty host are refused before any
    // token is asked for.
    let fake = serve(Script::new(vec![]));
    for url in [
        "http://127.0.0.1:1",
        "https://127.0.0.1:1/v1",
        "127.0.0.1:1",
    ] {
        let mut plan = fake.plan();
        plan.url = url.to_owned();
        let (result, out) = run(&fake, &plan);
        assert_eq!(result, Err(Error::Url(url.to_owned())));
        assert!(out.is_empty());
    }
    assert!(fake.requests("/token").is_empty());
}

#[test]
fn a_certificate_the_roots_do_not_hold_is_not_trusted() {
    // Proves: a coordinator whose certificate does not chain to the roots given for it is
    // refused at the handshake and nothing is sent to it, though the runner's token service
    // (its own roots) is reached and a token is asked for; and the other way, a token
    // service the roots given for it do not hold ends the deploy before any call to the
    // coordinator.
    let fake = serve(Script::new(vec![Ok(farm("converged", 0, 0))]));
    let (other, _) = fake_ca();
    let ca = [fake.ca.clone()];
    let mut out = Vec::new();
    let roots = Roots {
        runner: &ca,
        coordinator: &other,
    };
    let result = deploy(&fake.plan(), &fake.service(), &roots, &mut out);
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "read the farm before the deploy",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(fake.requests("/v1/desired").is_empty());
    assert_eq!(fake.requests("/token").len(), 1);
    let roots = Roots {
        runner: &other,
        coordinator: &ca,
    };
    let result = deploy(&fake.plan(), &fake.service(), &roots, &mut out);
    assert!(
        matches!(
            result,
            Err(Error::Call {
                what: "ask the runner for an OIDC token",
                ..
            })
        ),
        "{result:?}"
    );
    assert!(fake.requests("/v1/desired").is_empty());
}

/// A CA certificate that signed nothing the fake serves.
fn fake_ca() -> (Vec<CertificateDer<'static>>, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    (vec![ca.der().clone()], ca.pem())
}

#[test]
fn a_ca_in_pem_text_is_read_and_text_without_one_is_refused() {
    // Proves: the PEM of `--ca-pem` gives its certificates (one or several), and text that
    // holds none, or is not PEM, is an error rather than an empty set that trusts nothing.
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let one = ca.pem();
    assert_eq!(pem_roots(&one).expect("one"), [ca.der().clone()]);
    assert_eq!(pem_roots(&format!("{one}\n{one}")).expect("two").len(), 2);
    for text in [
        "",
        "not pem",
        "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
    ] {
        assert_eq!(pem_roots(text), Err(Error::Ca), "{text:?}");
    }
}

#[test]
fn the_verb_with_an_empty_url_prints_a_notice_and_exits_0() {
    // Proves: a repository whose variable is unset (an empty `--url`, or blanks) deploys
    // nothing and passes, with a notice the log shows.
    for url in ["", "  "] {
        let output = Command::new(XTASK)
            .args(["deploy-farm", "--url", url, "--version", "2.0"])
            .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
            .output()
            .expect("run");
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.starts_with("::notice::the deploy step is off"),
            "{text}"
        );
    }
}

#[test]
fn the_verb_with_an_address_and_no_token_service_fails_and_names_the_permission() {
    // Proves: a job without `id-token: write` fails with a message that names it, and
    // sends nothing; flags out of range are usage errors (exit 2).
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "https://127.0.0.1:9",
            "--version",
            "2.0",
        ])
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("id-token: write"));
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "u",
            "--version",
            "2.0",
            "--deadline-min",
            "0",
        ])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn the_verb_with_a_ca_that_is_no_pem_fails_before_any_call() {
    // Proves: a `--ca-pem` that holds no certificate fails the verb (exit 1) and names the
    // flag, instead of trusting nothing and failing at a handshake.
    let output = Command::new(XTASK)
        .args([
            "deploy-farm",
            "--url",
            "https://127.0.0.1:9",
            "--version",
            "2.0",
            "--ca-pem",
            "junk",
        ])
        .env("ACTIONS_ID_TOKEN_REQUEST_URL", "https://127.0.0.1:9/token")
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "k")
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--ca-pem holds no certificate"));
}

#[test]
fn the_plan_of_the_flags_is_in_seconds_and_defaults_the_audience_to_the_address() {
    // Proves: the defaults (60 minutes, 20 minutes, 30 seconds, the address as audience) and
    // that a trailing slash of the address is not part of a request path.
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: xtask::deploy_farm::Args,
    }
    let cli = Cli::parse_from(["x", "--url", "https://c.example:8443/", "--version", "2.0"]);
    let plan = Plan::of(&cli.args);
    assert_eq!(plan.url, "https://c.example:8443");
    assert_eq!(plan.audience, "https://c.example:8443/");
    assert_eq!(plan.deadline, Duration::from_secs(3600));
    assert_eq!(plan.stall, Duration::from_secs(1200));
    assert_eq!(plan.every, Duration::from_secs(30));
    assert_eq!((plan.min_up_storage, plan.min_up_frontend), (1, 1));
    let cli = Cli::parse_from(["x", "--url", "u", "--version", "2.0", "--audience", "aud"]);
    assert_eq!(Plan::of(&cli.args).audience, "aud");
}
