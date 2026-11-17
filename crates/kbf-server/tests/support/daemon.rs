//! A fake daemon: a node registered with the server's farm in-process, as the executor
//! registers its workers.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use kbf_proto::worker::{
    DaemonMessage, Heartbeat, Hello, LeaseId, LeaseOffer, NodeStatus, ResultAck, ServerMessage,
    Start, Welcome, daemon_message, server_message,
};
use kbf_sched::DaemonInstance;
use kbf_server::HostFloor;
use kbf_types::WorkerId;
use tokio::time::timeout;

use super::{INTERVAL, PROMPT, QUIET};

/// A node registered with the farm in-process, as the executor registers its workers: it
/// sends what a worker session carries (`Hello`, `Heartbeat`, `Result`, `NodeStatus`,
/// `UpdateStatus`) and receives what the farm sends it. Messages a test is not waiting for
/// are kept, in order, for a later `expect`.
pub struct FakeDaemon {
    tx: tokio::sync::mpsc::UnboundedSender<DaemonMessage>,
    inbound: tokio::sync::mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>,
    kept: VecDeque<server_message::Message>,
    seq: u64,
    /// The lease epoch the Welcome named (0 before it).
    pub epoch: u64,
    /// The farm, whose end ends the stream.
    farm: std::sync::Weak<kbf_server::farm::Farm>,
}

/// The stream of one registered node: what a message from it means to the farm.
struct Feed {
    farm: std::sync::Weak<kbf_server::farm::Farm>,
    worker: WorkerId,
    stream: kbf_server::farm::StreamId,
    /// The node's stream, which the farm also holds while the stream is its current one: an
    /// acknowledgement goes out on it even after the farm has replaced the stream.
    outbound: kbf_server::farm::Outbound,
}

impl Feed {
    /// Hands the farm one message of the node, as the executor's workers do. Returns whether
    /// the farm is still there.
    async fn handle(&self, received: Option<daemon_message::Message>) -> bool {
        let Some(farm) = self.farm.upgrade() else {
            return false;
        };
        let (worker, stream) = (&self.worker, self.stream);
        match received {
            Some(daemon_message::Message::Hello(hello)) => {
                if let Ok(reported) = kbf_server::worker::reported(&hello, HostFloor::default()) {
                    farm.resize(worker, stream, reported);
                }
            }
            Some(daemon_message::Message::Heartbeat(beat)) => {
                let running = beat
                    .running
                    .iter()
                    .map(|l| kbf_types::LeaseId::new(l.term, l.seq))
                    .collect();
                let acked = farm.heartbeat(worker, stream, beat.seq, running).await;
                if acked {
                    self.reply(server_message::Message::HeartbeatAck(
                        kbf_proto::worker::HeartbeatAck { seq: beat.seq },
                    ));
                }
            }
            Some(daemon_message::Message::Result(result)) => {
                // Beside the loop: settling a result is slow, and heartbeats behind it are
                // still taken meanwhile.
                let (farm, worker, outbound) =
                    (Arc::clone(&farm), worker.clone(), self.outbound.clone());
                tokio::spawn(async move {
                    if let Some(ack) = farm.report(&worker, result).await {
                        let _ = outbound.send(Ok(ServerMessage {
                            message: Some(server_message::Message::ResultAck(ack)),
                        }));
                    }
                });
            }
            Some(daemon_message::Message::NodeStatus(status)) => {
                let _ = farm.node_status(worker, stream, status);
            }
            Some(daemon_message::Message::UpdateStatus(status)) => {
                let _ = farm.update_status(worker, stream, status);
            }
            // Offers go from the farm to the node; the others are not a worker's here.
            Some(daemon_message::Message::Offer(_) | daemon_message::Message::MapRequest(_))
            | None => {}
        }
        true
    }

    fn reply(&self, message: server_message::Message) {
        let _ = self.outbound.send(Ok(ServerMessage {
            message: Some(message),
        }));
    }
}

impl FakeDaemon {
    /// Registers the node `hello` describes with `farm` and sends a `NodeStatus`, as a
    /// worker does after its `Welcome`.
    pub async fn connect(
        farm: std::sync::Weak<kbf_server::farm::Farm>,
        hello: Hello,
    ) -> Result<Self, tonic::Status> {
        let d = Self::connect_without_status(farm, hello).await?;
        d.send(daemon_message::Message::NodeStatus(NodeStatus {
            os_name: "Ubuntu".to_owned(),
            daemon_version: "test".to_owned(),
            ..NodeStatus::default()
        }));
        Ok(d)
    }

    /// Registers the node `hello` describes with `farm` and waits for its `Welcome`: a
    /// node that sends no `NodeStatus`.
    pub async fn connect_without_status(
        farm: std::sync::Weak<kbf_server::farm::Farm>,
        hello: Hello,
    ) -> Result<Self, tonic::Status> {
        let Some(live) = farm.upgrade() else {
            return Err(tonic::Status::unavailable("the farm is gone"));
        };
        if hello.node_id.is_empty() {
            return Err(tonic::Status::invalid_argument("Hello has no node_id"));
        }
        let reported = kbf_server::worker::reported(&hello, HostFloor::default())
            .map_err(tonic::Status::invalid_argument)?;
        let worker = WorkerId::new(hello.node_id);
        let welcome = ServerMessage {
            message: Some(server_message::Message::Welcome(Welcome {
                protocol_version: 1,
                heartbeat_interval_ms: INTERVAL.as_millis() as u64,
                epoch: live.lease_epoch(),
                ..Welcome::default()
            })),
        };
        let (outbound, responses) = tokio::sync::mpsc::unbounded_channel();
        let stream = live.register(
            &worker,
            DaemonInstance::new(hello.instance_id),
            DaemonInstance::new(hello.predecessor_instance_id),
            reported,
            outbound.clone(),
            welcome,
        );
        let feed = Feed {
            farm: farm.clone(),
            worker,
            stream,
            outbound,
        };
        drop(live);
        let (tx, mut messages) = tokio::sync::mpsc::unbounded_channel::<DaemonMessage>();
        tokio::spawn(async move {
            while let Some(received) = messages.recv().await {
                if !feed.handle(received.message).await {
                    break;
                }
            }
        });
        let mut d = Self {
            tx,
            inbound: responses,
            kept: VecDeque::new(),
            seq: 0,
            epoch: 0,
            farm,
        };
        match d.next().await {
            Some(server_message::Message::Welcome(w)) => {
                d.epoch = w.epoch;
                Ok(d)
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    pub fn send(&self, message: daemon_message::Message) {
        self.tx
            .send(DaemonMessage {
                message: Some(message),
            })
            .expect("the stream is open");
    }

    pub fn send_empty(&self) {
        self.tx
            .send(DaemonMessage { message: None })
            .expect("the stream is open");
    }

    /// Ends the stream from the daemon side.
    pub fn close(self) {}

    /// The next message of the stream, or `None` once it has ended: the farm dropped the
    /// stream's sender after the messages it sent, or the farm itself is gone.
    async fn recv(&mut self) -> Option<Result<ServerMessage, tonic::Status>> {
        let gone = async {
            while self.farm.strong_count() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            biased;
            received = self.inbound.recv() => received,
            () = gone => None,
        }
    }

    async fn next(&mut self) -> Option<server_message::Message> {
        if let Some(m) = self.kept.pop_front() {
            return Some(m);
        }
        let msg = timeout(PROMPT, self.recv())
            .await
            .expect("a server message in time")?
            .expect("the stream is healthy");
        Some(msg.message.expect("a non-empty server message"))
    }

    /// The first message `pick` accepts, within `within`, keeping the others.
    pub async fn expect_within<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> Option<T> {
        if let Some(at) = self.kept.iter().position(|m| pick(m).is_some()) {
            let m = self.kept.remove(at).expect("present");
            return pick(&m);
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let msg = timeout(deadline - tokio::time::Instant::now(), self.recv())
                .await
                .ok()??
                .expect("the stream is healthy");
            let m = msg.message.expect("a non-empty server message");
            if let Some(t) = pick(&m) {
                return Some(t);
            }
            self.kept.push_back(m);
        }
    }

    pub async fn expect<T>(
        &mut self,
        what: &str,
        pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> T {
        self.expect_within(PROMPT, pick)
            .await
            .unwrap_or_else(|| panic!("no {what} within {PROMPT:?}"))
    }

    pub async fn offer(&mut self) -> LeaseOffer {
        self.expect("LeaseOffer", |m| match m {
            server_message::Message::LeaseOffer(o) => Some(o.clone()),
            _ => None,
        })
        .await
    }

    /// The next Start. The offer of the same lease, if kept, is dropped with it.
    pub async fn start(&mut self) -> Start {
        let start = self
            .expect("Start", |m| match m {
                server_message::Message::Start(s) => Some(s.clone()),
                _ => None,
            })
            .await;
        self.kept.retain(|m| {
            !matches!(m, server_message::Message::LeaseOffer(o) if o.lease_id == start.lease_id)
        });
        start
    }

    /// The status the server ends the stream with, within [`PROMPT`]; messages before
    /// it are dropped. Panics if the stream stays open or ends without an error.
    pub async fn ended(&mut self) -> tonic::Status {
        let ending = async {
            loop {
                match self.recv().await {
                    Some(Ok(_)) => {}
                    None => panic!("the stream ended without an error"),
                    Some(Err(status)) => return status,
                }
            }
        };
        timeout(PROMPT, ending)
            .await
            .expect("the server ended the stream in time")
    }

    /// Waits, within [`PROMPT`], until the stream ends, by the server's status or by
    /// the connection closing; messages before that are dropped.
    pub async fn closed(&mut self) {
        let ending = async { while let Some(Ok(_)) = self.recv().await {} };
        timeout(PROMPT, ending)
            .await
            .expect("the stream ended in time");
    }

    /// The lease of the next `Cancel` within [`QUIET`], if one arrives.
    pub async fn cancelled(&mut self) -> Option<LeaseId> {
        self.expect_within(QUIET, |m| match m {
            server_message::Message::Cancel(c) => Some(c.lease_id.expect("a lease id")),
            _ => None,
        })
        .await
    }

    /// The leases of every `Cancel` already received and kept, in order, without waiting.
    /// The server queues a heartbeat's cancels before its acknowledgement, so after
    /// `beat_*_acked` these are all that heartbeat caused.
    pub fn cancels_received(&mut self) -> Vec<LeaseId> {
        let mut leases = Vec::new();
        self.kept.retain(|m| match m {
            server_message::Message::Cancel(c) => {
                leases.push(c.lease_id.expect("a lease id"));
                false
            }
            _ => true,
        });
        leases
    }

    /// Asserts no Offer or Start arrives within [`QUIET`].
    pub async fn no_work(&mut self) {
        let work = self
            .expect_within(QUIET, |m| match m {
                server_message::Message::LeaseOffer(_) | server_message::Message::Start(_) => {
                    Some(m.clone())
                }
                _ => None,
            })
            .await;
        assert_eq!(work, None, "work arrived");
    }

    /// Sends a heartbeat listing `running` and returns whether it was acknowledged
    /// within [`QUIET`].
    pub async fn heartbeat(&mut self, running: &[LeaseId]) -> bool {
        let seq = self.beat(running);
        self.expect_within(QUIET, ack_of(seq)).await.is_some()
    }

    /// Sends a heartbeat with the next sequence number; the number.
    fn beat(&mut self, running: &[LeaseId]) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        self.send(daemon_message::Message::Heartbeat(Heartbeat {
            seq,
            report_hash: Vec::new(),
            running: running.to_vec(),
            pack_reads: None,
            pack_writes: None,
            node_load: None,
            input_cache: None,
        }));
        seq
    }

    /// Sends `result` and returns the server's acknowledgement.
    pub async fn report(&mut self, result: kbf_proto::worker::Result) -> ResultAck {
        let lease = result.lease_id;
        self.send(daemon_message::Message::Result(result));
        self.expect("ResultAck", |m| match m {
            server_message::Message::ResultAck(a) if a.lease_id == lease => Some(*a),
            _ => None,
        })
        .await
    }
}

/// Picks the `HeartbeatAck` of the heartbeat numbered `seq`.
fn ack_of(seq: u64) -> impl FnMut(&server_message::Message) -> Option<()> {
    move |m| match m {
        server_message::Message::HeartbeatAck(a) if a.seq == seq => Some(()),
        _ => None,
    }
}
