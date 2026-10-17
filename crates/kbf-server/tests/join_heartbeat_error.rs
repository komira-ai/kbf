//! How the joiner's heartbeat loop answers a heartbeat that fails: with a scripted
//! membership service and a paused clock, so which heartbeat fails is the script's choice
//! and not the timing of a run. Alone in its test binary: a log event is seen by a
//! subscriber only if no other thread has already made the callsite uninteresting.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_server::{Membership, MembershipServer};
use kbf_server::join::{Identity, Joiner};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tonic::transport::Endpoint;
use tonic::{Code, Request, Response, Status};
use tracing::instrument::WithSubscriber as _;

/// What the service tells the joiner to beat at.
const INTERVAL: Duration = Duration::from_millis(10);
/// The joiner's wait after its first failure.
const RETRY: Duration = Duration::from_millis(100);

/// A `MakeWriter` into a shared buffer, standing for the log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("UTF-8")
    }

    fn dispatch(&self) -> tracing::Dispatch {
        tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .finish(),
        )
    }
}

/// A membership service whose heartbeats answer as scripted (`None` is success, and so is
/// every heartbeat after the script's end). It counts registrations and reports the
/// (paused) time of each heartbeat it receives.
#[derive(Clone)]
struct Scripted {
    script: Arc<Mutex<VecDeque<Option<Status>>>>,
    registers: Arc<AtomicU32>,
    beats: mpsc::UnboundedSender<Instant>,
}

#[tonic::async_trait]
impl Membership for Scripted {
    async fn register(
        &self,
        _: Request<pb::RegisterRequest>,
    ) -> Result<Response<pb::RegisterResponse>, Status> {
        self.registers.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: 1,
            heartbeat_interval_ms: u64::try_from(INTERVAL.as_millis()).expect("small"),
            map: None,
        }))
    }

    async fn heartbeat(
        &self,
        _: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        let _ = self.beats.send(Instant::now());
        let next = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .flatten();
        match next {
            Some(status) => Err(status),
            None => Ok(Response::new(pb::HeartbeatResponse {
                map_generation: 0,
                map: None,
            })),
        }
    }
}

/// Runs a joiner against a service that answers its heartbeats as `script` says, until it
/// has made `beats` heartbeats; returns the times of those, the registrations made and the
/// joiner's log.
async fn run_joiner(script: Vec<Option<Status>>, beats: usize) -> (Vec<Instant>, u32, String) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let service = Scripted {
        script: Arc::new(Mutex::new(script.into())),
        registers: Arc::new(AtomicU32::new(0)),
        beats: tx,
    };
    let registers = Arc::clone(&service.registers);
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(MembershipServer::new(service))
            .serve_with_incoming(incoming),
    );
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let who = Identity {
        instance_id: "node-x".to_owned(),
        addr: "192.0.2.5:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let log = Log::default();
    let joiner = tokio::spawn(
        Joiner::new(channel, who, RETRY)
            .run()
            .with_subscriber(log.dispatch()),
    );
    let mut times = Vec::new();
    while times.len() < beats {
        times.push(rx.recv().await.expect("the service lives"));
    }
    joiner.abort();
    (times, registers.load(Ordering::SeqCst), log.text())
}

/// Catches: a joiner that treats a heartbeat failing in any way but NOT_FOUND as a lapsed
/// row, or does not log it, or backs off wrongly. Heartbeats 2 and 4 of five answer
/// UNAVAILABLE (the others succeed): each is logged once with the status message, the
/// joiner registers once (a second registration would be the NOT_FOUND path), waits
/// `RETRY` and then the interval before beating again, and the wait after the second
/// failure is again `RETRY`, not twice it: a success in between reset the failure count.
/// Then the NOT_FOUND arm: a lapsed row is logged as that, registers again and beats at
/// the interval, with no backoff. Mutants: the success arm not resetting the failure count
/// (the last wait doubles); the failure arm registering again (two registrations); its
/// sleep dropped (a gap of the interval only); NOT_FOUND taken as any other failure (no
/// second registration).
#[tokio::test(start_paused = true)]
async fn a_failed_heartbeat_is_retried_without_registering_again() {
    let down = || Some(Status::unavailable("frontend down"));
    let (times, registers, log) = run_joiner(vec![None, down(), None, down()], 5).await;
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(registers, 1, "registered again after UNAVAILABLE: {log}");
    assert_eq!(
        log.matches("heartbeat to the frontend: frontend down")
            .count(),
        2,
        "{log}"
    );
    assert!(!log.contains("lapsed"), "{log}");
    assert_eq!(
        gaps,
        [INTERVAL, RETRY + INTERVAL, INTERVAL, RETRY + INTERVAL],
        "the waits between heartbeats"
    );

    let lapsed = || Some(Status::new(Code::NotFound, "no such member"));
    let (times, registers, log) = run_joiner(vec![lapsed()], 2).await;
    assert_eq!(registers, 2, "did not register again: {log}");
    assert_eq!(
        log.matches("the membership row lapsed; registering again")
            .count(),
        1,
        "{log}"
    );
    assert!(!log.contains("heartbeat to the frontend"), "{log}");
    assert_eq!(times[1] - times[0], INTERVAL, "a lapse waited to register");
}
