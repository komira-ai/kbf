//! The writes a node has open ([`OpenWrites`]): what the node's registration reports and a
//! rollout reads before it stops the node. A write counts from its first message to the end
//! of its call, whichever way the call ends; an action-cache commit counts too.

use std::sync::Arc;
use std::time::Duration;

use kbf_meta::{ActionRecord, Role};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use tokio::sync::mpsc;
use tonic::transport::Channel;

use super::fake::{Mem, connect, serve_routes, sha};
use super::{upload_name, write_of};
use crate::Storage;
use crate::service::{Admission, Config, OpenWrites, RemoteStorage, StorageService};

/// A plain-text service over `mem` counting into `open`, its shared handle, and a client.
async fn node(mem: &Arc<Mem>, open: &OpenWrites) -> (Channel, RemoteStorage, Arc<StorageService>) {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = Arc::new(
        StorageService::new(
            storage,
            Admission::Open,
            Config {
                write_bytes_in_flight: 1 << 30,
            },
        )
        .with_open_writes(open.clone()),
    );
    let channel = connect(serve_routes(service.shared_routes(), None).await).await;
    let remote = RemoteStorage::new(channel.clone(), 0);
    (channel, remote, service)
}

/// Polls until `open` counts `want`; the call is on another task, so the count is reached,
/// not read at once.
async fn count_reaches(open: &OpenWrites, want: u64) {
    for _ in 0..1000 {
        if open.count() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the open writes are {}, not {want}", open.count());
}

/// A write whose messages the test sends, and its answer.
struct Held {
    send: mpsc::Sender<WriteRequest>,
    answer: tokio::task::JoinHandle<Result<tonic::Response<()>, tonic::Status>>,
}

fn hold_open(channel: &Channel) -> Held {
    let (send, receive) = mpsc::channel(8);
    let mut client = ByteStreamClient::new(channel.clone());
    let answer = tokio::spawn(async move {
        client
            .write(futures::stream::unfold(
                receive,
                |mut messages| async move { messages.recv().await.map(|m| (m, messages)) },
            ))
            .await
            .map(|r| r.map(drop))
    });
    Held { send, answer }
}

/// Catches a count that is not the writes open: it must be 1 from the write's first
/// message until its answer (a write held open by a caller that has sent part of a blob),
/// 2 with a second beside it, and 0 once both are answered. Mutants: nothing counted; a
/// write counted only from its last message; the count dropped at the first message.
#[tokio::test]
async fn a_write_is_open_from_its_first_message_to_its_answer() {
    let mem = Arc::new(Mem::default());
    let open = OpenWrites::new();
    let (channel, _, _) = node(&mem, &open).await;
    assert_eq!(open.count(), 0);

    let blob = b"twelve bytes";
    let first = hold_open(&channel);
    let parts = write_of(
        &upload_name(&sha(blob)),
        &[(&blob[..5], false), (&blob[5..], true)],
    );
    first
        .send
        .send(parts[0].clone())
        .await
        .expect("first message");
    count_reaches(&open, 1).await;

    let other = b"another one!";
    let second = hold_open(&channel);
    let rest = write_of(
        &upload_name(&sha(other)),
        &[(&other[..4], false), (&other[4..], true)],
    );
    second
        .send
        .send(rest[0].clone())
        .await
        .expect("first message");
    count_reaches(&open, 2).await;

    first
        .send
        .send(parts[1].clone())
        .await
        .expect("last message");
    first.answer.await.expect("joins").expect("answered");
    count_reaches(&open, 1).await;
    second
        .send
        .send(rest[1].clone())
        .await
        .expect("last message");
    second.answer.await.expect("joins").expect("answered");
    count_reaches(&open, 0).await;
}

/// Catches a count left up by a write that does not end in an answer: a stream that stops
/// before `finish_write` (the caller crashed or the connection broke), a blob that is not
/// the one named, and a call the caller drops while the node waits for its next message.
/// Each must bring the count back to 0, or a node that had one such write would never be
/// idle. Mutant: the decrement only on the success path.
#[tokio::test]
async fn a_write_that_fails_or_is_dropped_is_not_left_open() {
    let mem = Arc::new(Mem::default());
    let open = OpenWrites::new();
    let (channel, _, _) = node(&mem, &open).await;
    let blob = b"twelve bytes";
    let name = upload_name(&sha(blob));

    let mut client = ByteStreamClient::new(channel.clone());
    let early = futures::stream::iter(write_of(&name, &[(&blob[..5], false)]));
    client.write(early).await.expect_err("ends early");
    count_reaches(&open, 0).await;

    let wrong = write_of(&name, &[(b"other bytes!", true)]);
    client
        .write(futures::stream::iter(wrong))
        .await
        .expect_err("not the blob named");
    count_reaches(&open, 0).await;

    let held = hold_open(&channel);
    held.send
        .send(write_of(&name, &[(&blob[..5], false)])[0].clone())
        .await
        .expect("first message");
    count_reaches(&open, 1).await;
    held.answer.abort();
    count_reaches(&open, 0).await;
}

/// Catches an action-cache commit that is not counted: a commit held in the store is a
/// write the node is still taking, over the wire and in-process. Mutants: the guard
/// missing from the `ActionStore` commit; missing from the in-process commit.
#[tokio::test]
async fn a_commit_in_flight_is_an_open_write() {
    let mem = Arc::new(Mem::default());
    let open = OpenWrites::new();
    let (_, remote, service) = node(&mem, &open).await;
    let remote = Arc::new(remote);
    let record = ActionRecord {
        result: sha(b"result"),
        closure: Default::default(),
    };

    mem.knobs.hold_commits();
    let over_the_wire = {
        let (remote, record) = (Arc::clone(&remote), record.clone());
        tokio::spawn(async move {
            remote
                .commit_action_record(Role::Daemon, sha(b"action"), record)
                .await
        })
    };
    count_reaches(&open, 1).await;
    let in_process = {
        let view = service.in_process().at(0);
        let record = record.clone();
        tokio::spawn(async move {
            view.commit_action_record(Role::Daemon, sha(b"other action"), record)
                .await
        })
    };
    count_reaches(&open, 2).await;

    mem.knobs.release_commits();
    over_the_wire.await.expect("joins").expect("committed");
    in_process.await.expect("joins").expect("committed");
    count_reaches(&open, 0).await;
}
