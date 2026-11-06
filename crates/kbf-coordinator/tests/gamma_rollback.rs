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

/// The hosts' heartbeat interval, and the deploy's reads in the first test: three rounds
/// between two reads, so both failures land between two of them.
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

/// How a host's update to [`RELEASE`] goes: whether it reaches the version, and on which beat
/// after the order it says so (the beats before that say `updating`).
#[derive(Clone, Copy)]
struct Course {
    reaches: bool,
    beats: u32,
}

/// One host as its agent reports it: what it runs, its last update, the update in flight
/// (target and beats left), and how its update to [`RELEASE`] goes. An update to any other
/// version reaches it on the next beat.
struct Host {
    name: &'static str,
    running: String,
    last_update: Option<Value>,
    in_flight: Option<(String, u32)>,
    release: Course,
}

/// What a round of beats saw: the orders the coordinator gave, as `(host, version)`, and the
/// hosts that beat `updating`.
#[derive(Default)]
struct Round {
    orders: Vec<(&'static str, String)>,
    updating: Vec<&'static str>,
}

/// Six daemon hosts at [`BEFORE`]. A host whose update to [`RELEASE`] fails says so after its
/// commit (its helper put [`BEFORE`] back) on the beat that ends the update and every beat
/// after.
struct Fleet {
    hosts: Vec<Host>,
}

impl Fleet {
    /// Every host fails [`RELEASE`], on the beat after its order.
    fn failing() -> Self {
        Self::with(&[])
    }

    /// The hosts named in `plans` go to [`RELEASE`] as their plan says; the others fail it on
    /// the beat after their order.
    fn with(plans: &[(&str, Course)]) -> Self {
        let hosts = ["d1", "d2", "d3", "d4", "d5", "d6"]
            .into_iter()
            .map(|name| Host {
                name,
                running: BEFORE.to_owned(),
                last_update: None,
                in_flight: None,
                release: plans.iter().find(|(n, _)| *n == name).map_or(
                    Course {
                        reaches: false,
                        beats: 1,
                    },
                    |(_, plan)| *plan,
                ),
            })
            .collect();
        Self { hosts }
    }

    /// One heartbeat of every host, in order, as the coordinator's clock now stands.
    fn round(&mut self, coordinator: &Coordinator) -> Round {
        let mut seen = Round::default();
        for host in &mut self.hosts {
            let mut updating = None;
            if let Some((target, left)) = host.in_flight.take() {
                if left > 1 {
                    updating = Some(target.clone());
                    host.in_flight = Some((target, left - 1));
                } else if target != RELEASE || host.release.reaches {
                    host.running = target.clone();
                    host.last_update = Some(json!({"target": target, "outcome": "succeeded"}));
                } else {
                    host.last_update = Some(json!({"target": target, "outcome": "failed",
                           "phase": "after_commit", "class": "release"}));
                }
            }
            let mut beat = json!({"platform": "linux-x86_64", "version": host.running,
                                  "health": "healthy"});
            if let Some(last) = &host.last_update {
                beat["last_update"] = last.clone();
            }
            if let Some(target) = &updating {
                beat["updating"] = json!(target);
                seen.updating.push(host.name);
            }
            let beat: Heartbeat = serde_json::from_value(beat).expect("a heartbeat");
            let who = Identity {
                node_id: host.name.parse().expect("a node id"),
                role: HostRole::DAEMON,
            };
            if let ServiceReply::Update { version, .. } = coordinator.heartbeat(who, beat).reply {
                let version = version.to_string();
                assert!(
                    host.in_flight.is_none(),
                    "{} was ordered {version} while its update was in flight",
                    host.name
                );
                let beats = if version == RELEASE {
                    host.release.beats
                } else {
                    1
                };
                host.in_flight = Some((version.clone(), beats));
                seen.orders.push((host.name, version));
            }
        }
        seen
    }

    fn ordered_ever(&self) -> usize {
        self.hosts
            .iter()
            .filter(|h| h.last_update.is_some() || h.in_flight.is_some())
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
                self.pass(d.as_secs());
            }),
        }
    }

    /// `secs` pass: the rounds of beats they hold, in order.
    fn pass(&self, secs: u64) -> Vec<Round> {
        let from = self.now.load(Ordering::SeqCst);
        let to = from + secs;
        let mut rounds = Vec::new();
        for at in (from + 1..=to).filter(|s| s % BEAT_SECS == 0) {
            self.clock.store(START + at, Ordering::SeqCst);
            rounds.push(
                self.fleet
                    .lock()
                    .expect("the fleet")
                    .round(&self.coordinator),
            );
        }
        self.now.store(to, Ordering::SeqCst);
        self.clock.store(START + to, Ordering::SeqCst);
        rounds
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

/// The marks of kbf in the operator's status: `(last_good, bad)`.
async fn kbf_marks(farm: &Farm) -> (Value, Value) {
    let status = farm.status().await;
    let kbf = &status["ops"]["services"]["kbf"];
    (kbf["last_good"].clone(), kbf["bad"].clone())
}

/// A job's view of the farm: the coordinator as shipped, the token service, and the clock that
/// moves the fleet.
struct Gamma {
    farm: Farm,
    tokens: Arc<Tokens>,
    time: Arc<Time>,
}

impl Gamma {
    /// A farm whose six hosts have converged on [`BEFORE`] (so it is the last good version),
    /// with `fleet` as the hosts.
    async fn start(name: &str, fleet: Fleet) -> Self {
        let farm = Farm::start(name).await;
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
            fleet: Mutex::new(fleet),
        });
        time.fleet
            .lock()
            .expect("the fleet")
            .round(&farm.coordinator);
        assert_eq!(kbf_marks(&farm).await.0, BEFORE, "BEFORE is the last good");
        Self {
            farm,
            tokens: Arc::new(Tokens::start()),
            time,
        }
    }

    fn url(&self) -> String {
        format!("https://127.0.0.1:{}", self.farm.bound.control.port())
    }

    fn roots(&self) -> (Vec<CertificateDer<'static>>, Vec<CertificateDer<'static>>) {
        let farm_ca = CertificateDer::from(self.farm.pki.ca.root_der().to_vec());
        (vec![self.tokens.ca.clone()], vec![farm_ca])
    }

    /// The job's deploy of [`RELEASE`], reading every `every_secs`: its result and its log.
    async fn deploy(&self, every_secs: u64) -> (Result<(), Error>, String) {
        let plan = Plan {
            url: self.url(),
            service: "kbf".to_owned(),
            audience: self.url(),
            version: RELEASE.to_owned(),
            min_up_storage: 0,
            min_up_frontend: 0,
            require_coordinator_current: false,
            deadline: Duration::from_secs(3_600),
            stall: Duration::from_secs(1_200),
            every: Duration::from_secs(every_secs),
        };
        let (time, tokens, (runner, coordinator)) = (
            Arc::clone(&self.time),
            Arc::clone(&self.tokens),
            self.roots(),
        );
        tokio::task::spawn_blocking(move || {
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
    }

    /// The job's restore after a failed deploy, as run `4711-1`.
    async fn restore(&self) -> Result<String, restore::Error> {
        let args = restore::Args {
            url: self.url(),
            version: RELEASE.to_owned(),
            service: "kbf".to_owned(),
            ca_pem: String::new(),
            audience: None,
            run: "4711-1".to_owned(),
            every_secs: 1,
        };
        let (time, tokens, (runner, coordinator)) = (
            Arc::clone(&self.time),
            Arc::clone(&self.tokens),
            self.roots(),
        );
        tokio::task::spawn_blocking(move || {
            let roots = Roots {
                runner: &runner,
                coordinator: &coordinator,
            };
            let mut out = Vec::new();
            restore_on(&args, &tokens.service(), &roots, &time.clock(), &mut out)
        })
        .await
        .expect("the restore ran")
    }

    fn asked(&self) -> usize {
        self.tokens.asked.load(Ordering::SeqCst)
    }
}

/// Catches: a release that two hosts fail after the commit left desired while the job waits,
/// and a restore that writes over the coordinator's own rollback. The farm converged on
/// BEFORE, so it is the last good version. The job sets RELEASE and reads every 45 s; between
/// two of its reads the first host ordered RELEASE fails it, the next host is ordered it and
/// fails too, and the coordinator marks RELEASE bad and sets BEFORE back. The deploy then reads
/// BEFORE desired and ends `Superseded`; the restore that follows reads the same, sets nothing
/// and says why: it asks for one token, for its read, and the generation the rollback left is
/// the one after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_rollback_by_the_coordinator_ends_the_deploy_superseded_and_the_restore_posts_nothing() {
    let gamma = Gamma::start("gamma-rollback", Fleet::failing()).await;
    let (result, log) = gamma.deploy(EVERY_SECS).await;
    assert!(
        matches!(&result, Err(Error::Superseded(desired)) if desired == BEFORE),
        "{result:?}\n{log}"
    );
    assert_eq!(
        gamma.time.fleet.lock().expect("the fleet").ordered_ever(),
        2,
        "two hosts were ordered the release and failed it\n{log}"
    );
    let after_deploy = convergence(&gamma.farm).await;
    assert_eq!(after_deploy["desired"], BEFORE);
    assert_eq!(
        kbf_marks(&gamma.farm).await,
        (json!(BEFORE), json!([RELEASE]))
    );

    let asked_before = gamma.asked();
    let line = gamma
        .restore()
        .await
        .expect("nothing to set back is not an error");
    assert!(
        line.starts_with("nothing to set back: the desired version is ")
            && line.contains("the coordinator's own rollback"),
        "{line}"
    );
    assert_eq!(
        gamma.asked() - asked_before,
        1,
        "the restore read once and posted nothing"
    );
    let after_restore = convergence(&gamma.farm).await;
    assert_eq!(after_restore["desired"], BEFORE);
    assert_eq!(after_restore["generation"], after_deploy["generation"]);
    assert_eq!(after_restore["write_id"], after_deploy["write_id"]);
}

/// Catches: the job's restore and the coordinator's rollback fighting when the deploy reads at
/// its shipped interval (`deploy_farm::EVERY_SECS`, 30 s). The first host ordered RELEASE fails
/// it, and the deploy's next read finds the farm `blocked` before a second host has failed: it
/// ends `Blocked`. The restore then reads RELEASE desired and sets BEFORE back on the generation
/// it read, as its own write. The second host's failure arrives after that, for a version no
/// longer desired: the coordinator marks nothing bad, writes nothing more, and the farm
/// converges on BEFORE.
#[tokio::test(flavor = "multi_thread")]
async fn at_the_shipped_interval_the_deploy_ends_blocked_and_the_restore_sets_before_back() {
    let gamma = Gamma::start("gamma-blocked", Fleet::failing()).await;
    let (result, log) = gamma.deploy(xtask::deploy_farm::EVERY_SECS).await;
    assert!(
        matches!(&result, Err(Error::Blocked(release)) if release == RELEASE),
        "{result:?}\n{log}"
    );
    let after_deploy = convergence(&gamma.farm).await;
    assert_eq!(after_deploy["desired"], RELEASE);
    assert_eq!(after_deploy["state"], "blocked");
    let generation = after_deploy["generation"].as_u64().expect("a generation");

    let restored = gamma.restore().await;
    assert!(
        matches!(&restored, Err(restore::Error::Restored { version, release })
            if version == BEFORE && release == RELEASE),
        "{restored:?}"
    );
    let after_restore = convergence(&gamma.farm).await;
    assert_eq!(after_restore["desired"], BEFORE);
    assert_eq!(after_restore["generation"], generation + 1);
    assert_eq!(after_restore["write_id"], "restore-4711-1");

    // The host ordered RELEASE before the restore now fails it; nothing is marked or written.
    let rounds = gamma.time.pass(4 * BEAT_SECS);
    assert!(
        rounds.iter().all(|r| r.orders.is_empty()),
        "every host is at BEFORE: no order"
    );
    assert_eq!(
        gamma.time.fleet.lock().expect("the fleet").ordered_ever(),
        2
    );
    assert_eq!(kbf_marks(&gamma.farm).await, (json!(BEFORE), json!([])));
    let settled = convergence(&gamma.farm).await;
    assert_eq!(settled["generation"], generation + 1);
    assert_eq!(settled["state"], "converged");
}

/// Catches: a rollback that strands a host that took the release, or one that cuts an update
/// in flight. Updates take beats here: d1 reaches RELEASE after three, d2 and d3 fail it after
/// two. One update is live at a time, so while one host is updating no other is ordered, and
/// each update runs to its end. d1's success does not stop the roll; the second failure (d3's,
/// on the beat its update ends, when no update is in flight) rolls RELEASE back. The rollback
/// orders BEFORE to d1, the one host on RELEASE, and the farm converges on BEFORE. No job runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_mixed_fleet_past_the_threshold_is_ordered_back_and_converges_on_last_good() {
    let fleet = Fleet::with(&[
        (
            "d1",
            Course {
                reaches: true,
                beats: 3,
            },
        ),
        (
            "d2",
            Course {
                reaches: false,
                beats: 2,
            },
        ),
        (
            "d3",
            Course {
                reaches: false,
                beats: 2,
            },
        ),
    ]);
    let gamma = Gamma::start("gamma-mixed", fleet).await;
    let set_release = json!({"version": RELEASE, "min_up": {"storage": 0, "frontend": 0}});
    let (code, body) = gamma
        .farm
        .control(
            "POST",
            "/v1/desired",
            Some(&ci_token()),
            &set_release.to_string(),
        )
        .await
        .expect("answered");
    assert_eq!(code, 200, "{body}");

    let mut rounds = Vec::new();
    while convergence(&gamma.farm).await["desired"] == RELEASE {
        assert!(
            rounds.len() < 20,
            "no rollback after {} rounds",
            rounds.len()
        );
        rounds.extend(gamma.time.pass(BEAT_SECS));
    }
    let orders: Vec<(&str, String)> = rounds.iter().flat_map(|r| r.orders.clone()).collect();
    assert_eq!(
        orders,
        [
            ("d1", RELEASE.to_owned()),
            ("d2", RELEASE.to_owned()),
            ("d3", RELEASE.to_owned()),
        ]
    );
    for (i, round) in rounds.iter().enumerate() {
        assert!(
            round.updating.len() <= 1,
            "one update at a time: {:?}",
            round.updating
        );
        assert!(
            round.orders.is_empty() || round.updating.is_empty(),
            "round {i}: an order went out while {:?} was updating",
            round.updating
        );
    }
    let last = rounds.last().expect("rounds");
    assert!(
        last.updating.is_empty(),
        "nothing was in flight at the rollback"
    );
    assert_eq!(
        kbf_marks(&gamma.farm).await,
        (json!(BEFORE), json!([RELEASE]))
    );

    let mut back = Vec::new();
    while convergence(&gamma.farm).await["state"] != "converged" {
        assert!(back.len() < 10, "not converged after {} rounds", back.len());
        back.extend(gamma.time.pass(BEAT_SECS));
    }
    let orders: Vec<(&str, String)> = back.iter().flat_map(|r| r.orders.clone()).collect();
    assert_eq!(orders, [("d1", BEFORE.to_owned())]);
    assert_eq!(convergence(&gamma.farm).await["desired"], BEFORE);
    let fleet = gamma.time.fleet.lock().expect("the fleet");
    assert!(fleet.hosts.iter().all(|h| h.running == BEFORE));
}
