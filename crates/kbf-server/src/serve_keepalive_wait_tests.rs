//! A WaitExecution stream (the same stream as an Execute's, `kbf_front`'s) whose client's path
//! goes silent, served as the REAPI listener serves it: the server PINGs, the progress tick
//! writes into the stalled connection, and the stream, with the receiver of its operation's
//! stage it holds, is dropped once the listener's watch closes the connection, within the PING
//! interval and the keepalive timeout after the client was last heard. That receiver is the
//! caller the executor counts before it cancels an operation no client waits on
//! ([`crate::executor::callers`]), and this bound is what the grace of that cancel is set above.

use kbf_auth::Authorizers;
use kbf_front::{Cache, Dispatch, ExecutionService, MemoryMetaLog, Stage, Submission, Ticket};
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::WaitExecutionRequest;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::execution_server::ExecutionServer;
use kbf_types::{Digest, DigestFunction};

use super::*;

/// The one operation's name.
const NAME: &str = "operations/1";

/// A dispatcher that knows one running operation, whose stage is `stage`.
struct One {
    stage: watch::Sender<Stage>,
}

impl Dispatch for One {
    fn submit(&self, _: Submission) -> Result<Ticket, Status> {
        Err(Status::unavailable("only WaitExecution is served here"))
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        (name == NAME).then(|| Ticket {
            name: NAME.to_owned(),
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
            stage: self.stage.subscribe(),
        })
    }

    fn cancel(&self, _: &str) -> bool {
        false
    }
}

/// A server over `timing` with the REAPI Execution service over [`One`], its progress tick
/// running, a client connected to it over a [`Path`], and the client's WaitExecution stream of
/// the operation, its first message read.
struct Waiting {
    path: Path,
    stage: watch::Sender<Stage>,
    _stream: Streaming<Operation>,
    _closer: kbf_front::Closer,
}

impl Waiting {
    async fn open(timing: StreamTiming) -> Self {
        let (client_end, path_client) = tokio::io::duplex(PIPE_BYTES);
        let (path_server, server_end) = tokio::io::duplex(PIPE_BYTES);
        let path = Path::between(path_client, path_server);

        let (stage, _) = watch::channel(Stage::Executing);
        let log = MemoryMetaLog::new(Retention::default());
        let objects = MemoryStore::new(Capabilities::default());
        let cache = Cache::open(log, objects, KeyPrefix::default())
            .await
            .expect("an action cache in memory");
        let (closer, closing) = kbf_front::closing();
        let (ticker, progress) = kbf_front::progress(timing.progress_interval);
        tokio::spawn(ticker.run());
        let dispatch = Arc::new(One {
            stage: stage.clone(),
        });
        let service = ExecutionService::with_authorizers(
            Arc::new(cache),
            dispatch,
            closing,
            Arc::new(Authorizers::allow_all()),
        )
        .with_progress(progress);
        let server =
            keepalive(Server::builder(), timing).add_service(ExecutionServer::new(service));
        tokio::spawn(server.serve_with_incoming(one_connection(server_end, limit(timing))));

        let channel = Endpoint::from_static("http://kbf.test")
            .connect_with_connector(OnePipe(Some(client_end)))
            .await
            .expect("connected over the pipe");
        let mut stream = ExecutionClient::new(channel)
            .wait_execution(WaitExecutionRequest {
                name: NAME.to_owned(),
            })
            .await
            .expect("the wait opened")
            .into_inner();
        let first = tokio::time::timeout(HANG, stream.message())
            .await
            .expect("the first operation came")
            .expect("the stream is open");
        assert_eq!(first.map(|operation| operation.name).as_deref(), Some(NAME));
        Self {
            path,
            stage,
            _stream: stream,
            _closer: closer,
        }
    }

    /// How many streams hold the operation's stage, once the server's tasks have run with the
    /// clock where it is.
    async fn callers(&self) -> usize {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        self.stage.receiver_count()
    }
}

/// Catches a dead client's stream that outlives the listener's bound (a keepalive turned off,
/// its timeout ignored, or a progress write into the dead connection taken for the client),
/// and one dropped before it: with the path silent, the stream still holds its caller a second
/// before the PING interval and the keepalive timeout have passed, and has dropped it a second
/// after, the path still silent.
#[tokio::test(start_paused = true)]
async fn a_silent_clients_wait_stops_counting_as_a_caller_at_the_keepalives_bound() {
    let timing = default_timing();
    let waiting = Waiting::open(timing).await;
    assert_eq!(
        waiting.callers().await,
        1,
        "the open stream holds the stage"
    );
    let bound = timing.keepalive_interval + timing.keepalive_timeout;

    waiting.path.stall();
    tokio::time::sleep(bound - Duration::from_secs(1)).await;
    assert_eq!(
        waiting.callers().await,
        1,
        "the stream was dropped before the bound"
    );

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        waiting.callers().await,
        0,
        "the server kept the dead client's stream past the bound"
    );
}

/// The control for the rig: a path that carries on keeps the stream, and its caller, past the
/// same bound, so the drop above is the silence's.
#[tokio::test(start_paused = true)]
async fn a_client_that_answers_keeps_its_wait_past_the_bound() {
    let timing = default_timing();
    let waiting = Waiting::open(timing).await;
    let bound = timing.keepalive_interval + timing.keepalive_timeout;

    tokio::time::sleep(bound + Duration::from_secs(1)).await;
    assert_eq!(
        waiting.callers().await,
        1,
        "a live client's stream was dropped"
    );
}
