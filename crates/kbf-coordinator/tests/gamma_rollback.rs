//! The `gamma` job's two steps (`cargo xtask deploy-farm`, then `deploy-farm restore`) against
//! this crate's coordinator as it ships: no services file, so kbf rolls back on its own. The
//! coordinator serves its control listener over TLS on loopback, as in `farm.rs`; the runner's
//! token service is a stand-in on loopback that mints the testkit's CI tokens. Six daemon
//! hosts beat in process, one round each 15 s of the deploy's virtual clock: a round runs when
//! the deploy sleeps between two reads, so the test moves only when the deploy does.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_coordinator::identity::Identity;
use kbf_coordinator::service::Coordinator;
use kbf_oidc::testkit::{STATIC_ISSUER, good_claims, keys, mint};
use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, ServiceReply};
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use support::{Farm, ci_token};
use xtask::deploy_farm::restore::{self, restore_on};
use xtask::deploy_farm::{Clock, Error, Plan, Roots, TokenService, deploy_on};

/// The version the farm ran before the release, and the release.
const BEFORE: &str = "1.0.0+g000000000001";
const RELEASE: &str = "2.0.0+g000000000002";

/// The hosts' heartbeat interval, and the deploy's reads: three rounds between two reads.
const BEAT_SECS: u64 = 15;
const EVERY_SECS: u64 = 45;

/// The coordinator's clock when the hosts first beat: past its quiet start.
const START: u64 = 2_000;

/// A runner token service on loopback over TLS: every request gets a fresh CI token, and the
/// requests are counted (each call of the deploy and the restore asks for one).
struct Tokens {
    base: String,
    ca: CertificateDer<'static>,
    asked: Arc<AtomicUsize>,
}

impl Tokens {
    fn start() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("a key"))
            .expect("a CA");
        let key = KeyPair::generate().expect("a key");
        let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("signed");
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .expect("a server config"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bound");
        let base = format!(
            "https://127.0.0.1:{}",
            listener.local_addr().expect("an address").port()
        );
        let asked = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&asked);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("a conn");
                let mut tls = rustls::StreamOwned::new(conn, stream);
                // The head of a GET: lines up to the empty one.
                let mut reader = BufReader::new(&mut tls);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    line.clear();
                }
                count.fetch_add(1, Ordering::SeqCst);
                let token = mint(&keys()[0], &good_claims(STATIC_ISSUER));
                let body = json!({ "value": token }).to_string();
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(answer.as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        Self {
            base,
            ca: ca.der().clone(),
            asked,
        }
    }

    fn service(&self) -> TokenService {
        TokenService {
            url: format!("{}/token?api-version=2.0", self.base),
            key: "runner-key".to_owned(),
        }
    }
}

/// One host as its agent reports it: what it runs, its last update, and the order it was
/// given on its last beat.
struct Host {
    name: &'static str,
    running: &'static str,
    last_update: Option<Value>,
    ordered: Option<String>,
}

/// Six daemon hosts at [`BEFORE`]. A host ordered [`RELEASE`] fails it after the commit (its
/// helper puts [`BEFORE`] back) and says so on its next beat and every beat after; a host
/// ordered any other version reaches it.
struct Fleet {
    hosts: Vec<Host>,
}

impl Fleet {
    fn new() -> Self {
        let hosts = ["d1", "d2", "d3", "d4", "d5", "d6"]
            .into_iter()
            .map(|name| Host {
                name,
                running: BEFORE,
                last_update: None,
                ordered: None,
            })
            .collect();
        Self { hosts }
    }

    /// One heartbeat of every host, in order, as the coordinator's clock now stands.
    fn round(&mut self, coordinator: &Coordinator) {
        for host in &mut self.hosts {
            if let Some(target) = host.ordered.take() {
                host.last_update = Some(if target == RELEASE {
                    json!({"target": target, "outcome": "failed",
                           "phase": "after_commit", "class": "release"})
                } else {
                    json!({"target": target, "outcome": "succeeded"})
                });
            }
            let mut beat = json!({"platform": "linux-x86_64", "version": host.running,
                                  "health": "healthy"});
            if let Some(last) = &host.last_update {
                beat["last_update"] = last.clone();
            }
            let beat: Heartbeat = serde_json::from_value(beat).expect("a heartbeat");
            let who = Identity {
                node_id: host.name.parse().expect("a node id"),
                role: HostRole::DAEMON,
            };
            if let ServiceReply::Update { version, .. } = coordinator.heartbeat(who, beat).reply {
                host.ordered = Some(version.to_string());
            }
        }
    }

    fn ordered_ever(&self) -> usize {
        self.hosts
            .iter()
            .filter(|h| h.last_update.is_some())
            .count()
    }
}

/// The deploy's clock, which is the coordinator's too: a sleep moves both, and runs a round of
/// the fleet for every [`BEAT_SECS`] it crosses.
struct Time {
    now: AtomicU64,
    clock: Arc<AtomicU64>,
    coordinator: Arc<Coordinator>,
    fleet: Mutex<Fleet>,
}

impl Time {
    fn clock(&self) -> Clock<'_> {
        Clock {
            now: Box::new(|| Duration::from_secs(self.now.load(Ordering::SeqCst))),
            sleep: Box::new(|d| {
                let from = self.now.load(Ordering::SeqCst);
                let to = from + d.as_secs();
                for at in (from + 1..=to).filter(|s| s % BEAT_SECS == 0) {
                    self.clock.store(START + at, Ordering::SeqCst);
                    self.fleet
                        .lock()
                        .expect("the fleet")
                        .round(&self.coordinator);
                }
                self.now.store(to, Ordering::SeqCst);
                self.clock.store(START + to, Ordering::SeqCst);
            }),
        }
    }
}

/// `GET /v1/convergence` as CI reads it.
async fn convergence(farm: &Farm) -> Value {
    let (code, body) = farm
        .control("GET", "/v1/convergence", Some(&ci_token()), "")
        .await
        .expect("answered");
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).expect("JSON")
}

/// Catches: a release that two hosts fail after the commit left desired while the job waits,
/// and a restore that writes over the coordinator's own rollback. The farm converged on
/// BEFORE, so it is the last good version. The job sets RELEASE; between two of its reads the
/// first host ordered RELEASE fails it, the next host is ordered it and fails it too, and the
/// coordinator marks RELEASE bad and sets BEFORE back. The deploy then reads BEFORE desired and
/// ends `Superseded`; the restore that follows reads the same, sets nothing and says why: it
/// asks for one token, for its read, and the generation the rollback left is the one after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_rollback_by_the_coordinator_ends_the_deploy_superseded_and_the_restore_posts_nothing() {
    let farm = Farm::start("gamma-rollback").await;
    let set_before = json!({"version": BEFORE, "min_up": {"storage": 0, "frontend": 0}});
    let (code, body) = farm
        .control(
            "POST",
            "/v1/desired",
            Some(&ci_token()),
            &set_before.to_string(),
        )
        .await
        .expect("answered");
    assert_eq!(code, 200, "{body}");
    farm.at(START);
    let time = Arc::new(Time {
        now: AtomicU64::new(0),
        clock: Arc::clone(&farm.clock),
        coordinator: Arc::clone(&farm.coordinator),
        fleet: Mutex::new(Fleet::new()),
    });
    time.fleet
        .lock()
        .expect("the fleet")
        .round(&farm.coordinator);

    let tokens = Arc::new(Tokens::start());
    let url = format!("https://127.0.0.1:{}", farm.bound.control.port());
    let farm_ca = CertificateDer::from(farm.pki.ca.root_der().to_vec());
    let plan = Plan {
        url: url.clone(),
        service: "kbf".to_owned(),
        audience: url.clone(),
        version: RELEASE.to_owned(),
        min_up_storage: 0,
        min_up_frontend: 0,
        require_coordinator_current: false,
        deadline: Duration::from_secs(3_600),
        stall: Duration::from_secs(1_200),
        every: Duration::from_secs(EVERY_SECS),
    };
    let deployed = {
        let (time, tokens, farm_ca) = (Arc::clone(&time), Arc::clone(&tokens), farm_ca.clone());
        tokio::task::spawn_blocking(move || {
            let runner = [tokens.ca.clone()];
            let coordinator = [farm_ca];
            let roots = Roots {
                runner: &runner,
                coordinator: &coordinator,
            };
            let mut out = Vec::new();
            let result = deploy_on(&plan, &tokens.service(), &roots, &time.clock(), &mut out);
            (result, String::from_utf8_lossy(&out).into_owned())
        })
        .await
        .expect("the deploy ran")
    };
    let (result, log) = deployed;
    assert!(
        matches!(&result, Err(Error::Superseded(desired)) if desired == BEFORE),
        "{result:?}\n{log}"
    );
    assert_eq!(
        time.fleet.lock().expect("the fleet").ordered_ever(),
        2,
        "two hosts were ordered the release and failed it\n{log}"
    );
    let after_deploy = convergence(&farm).await;
    assert_eq!(after_deploy["desired"], BEFORE);
    let status = farm.status().await;
    assert_eq!(status["ops"]["services"]["kbf"]["bad"], json!([RELEASE]));
    assert_eq!(status["ops"]["services"]["kbf"]["last_good"], BEFORE);

    let asked_before = tokens.asked.load(Ordering::SeqCst);
    let restored = {
        let (time, tokens) = (Arc::clone(&time), Arc::clone(&tokens));
        tokio::task::spawn_blocking(move || {
            let runner = [tokens.ca.clone()];
            let coordinator = [farm_ca];
            let roots = Roots {
                runner: &runner,
                coordinator: &coordinator,
            };
            let args = restore::Args {
                url,
                version: RELEASE.to_owned(),
                service: "kbf".to_owned(),
                ca_pem: String::new(),
                audience: None,
                run: "4711-1".to_owned(),
                every_secs: 1,
            };
            let mut out = Vec::new();
            restore_on(&args, &tokens.service(), &roots, &time.clock(), &mut out)
        })
        .await
        .expect("the restore ran")
    };
    let line = restored.expect("nothing to set back is not an error");
    assert!(
        line.starts_with("nothing to set back: the desired version is ")
            && line.contains("the coordinator's own rollback"),
        "{line}"
    );
    assert_eq!(
        tokens.asked.load(Ordering::SeqCst) - asked_before,
        1,
        "the restore read once and posted nothing"
    );
    let after_restore = convergence(&farm).await;
    assert_eq!(after_restore["desired"], BEFORE);
    assert_eq!(after_restore["generation"], after_deploy["generation"]);
    assert_eq!(after_restore["write_id"], after_deploy["write_id"]);
}
