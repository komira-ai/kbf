//! `kbf-durable: only` on `FindMissingBlobs`: the durable question, over the wire, in
//! process and under catch-up, and the plain question unchanged beside it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use kbf_proto::reapi::FindMissingBlobsRequest;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use tonic::metadata::MetadataValue;
use tonic::{Code, Request};

use super::fake::{Mem, connect, pair, serve_routes, sha};
use crate::service::wire::to_proto;
use crate::service::{
    Admission, CatchUp, Config, DURABLE_HEADER, DURABLE_ONLY, RemoteStorage, StorageService,
};
use crate::{Storage, StorageError};

/// A node over a store holding `disk` on disk and `ram` in RAM only.
fn held_in(mem: &Mem, disk: &[u8], ram: &[u8]) -> (kbf_types::Digest, kbf_types::Digest) {
    let (disk, ram) = (sha(disk), sha(ram));
    let mut blobs = mem.blobs.lock().expect("lock");
    blobs.insert(disk, Bytes::from_static(b"d"));
    blobs.insert(ram, Bytes::from_static(b"r"));
    mem.ram_only.lock().expect("lock").insert(ram);
    (disk, ram)
}

/// Catches a client that does not send the header (the node would answer the plain
/// question, and a blob held in RAM would be reported durable), a node that ignores it,
/// and one that answers the plain question for a request without it: over the wire,
/// `find_durable` names the RAM-held blob and the absent one missing and reaches the
/// store's `find_durable` once; `find_missing` names only the absent one and reaches
/// `find_missing`; neither touches the other's counter.
#[tokio::test]
async fn the_header_selects_the_durable_question_over_the_wire() {
    let (mem, remote, _) = pair().await;
    let (disk, ram) = held_in(&mem, b"on disk", b"in ram");
    let absent = sha(b"nowhere");

    assert_eq!(
        remote
            .find_durable(&[disk, ram, absent])
            .await
            .expect("answer"),
        [ram, absent]
    );
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 1);
    assert_eq!(mem.knobs.asked.load(Ordering::SeqCst), 0);

    assert_eq!(
        remote
            .find_missing(&[disk, ram, absent])
            .await
            .expect("answer"),
        [absent]
    );
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 1);
    assert_eq!(mem.knobs.asked.load(Ordering::SeqCst), 1);
}

/// Catches a header value other than `only` being taken as the plain or the durable
/// question: `wait` (a later mode), an empty value and a value that is not text are each
/// INVALID_ARGUMENT, and the store is asked nothing. The value `only` is answered.
#[tokio::test]
async fn any_other_value_of_the_header_is_refused() {
    let (mem, _, channel) = pair().await;
    let mut cas = ContentAddressableStorageClient::new(channel);
    let digest = sha(b"x");
    let ask = |value: Option<MetadataValue<_>>| {
        let mut request = Request::new(FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![to_proto(&digest)],
            digest_function: 0,
        });
        if let Some(value) = value {
            request.metadata_mut().insert(DURABLE_HEADER, value);
        }
        request
    };
    let not_text = MetadataValue::try_from(&b"on\xffly"[..]).expect("a value");
    for value in [
        MetadataValue::from_static("wait"),
        MetadataValue::from_static(""),
        not_text,
    ] {
        let status = cas
            .find_missing_blobs(ask(Some(value.clone())))
            .await
            .expect_err("refused");
        assert_eq!(status.code(), Code::InvalidArgument, "{value:?}");
        assert!(status.message().contains(DURABLE_HEADER), "{status}");
    }
    assert_eq!(mem.knobs.asked.load(Ordering::SeqCst), 0);
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 0);

    let answered = cas
        .find_missing_blobs(ask(Some(MetadataValue::from_static(DURABLE_ONLY))))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(answered.missing_blob_digests.len(), 1);
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 1);
}

/// Catches an in-process call that goes around the durable question (it would answer the
/// plain one, which counts a RAM-held blob present) and one that skips the catching-up
/// rule: in-process, `find_durable` answers as the wire does, and while the node is
/// catching up a durable answer with a digest missing is UNAVAILABLE, as the plain one is
/// (an answer that left out only the missing ones would tell the caller they are held),
/// over the wire and in process; a durable answer with nothing missing is served.
#[tokio::test]
async fn the_durable_question_is_judged_as_the_plain_one_in_process_and_while_catching_up() {
    let mem = Arc::new(Mem::default());
    let (disk, ram) = held_in(&mem, b"on disk", b"in ram");
    let catch_up = CatchUp::rebuilding();
    let service = Arc::new(
        StorageService::new(
            Arc::clone(&mem) as Arc<dyn Storage>,
            Admission::Open,
            Config {
                write_bytes_in_flight: 1 << 30,
            },
        )
        .with_catch_up(catch_up.clone()),
    );
    let local = service.in_process().at(0);
    let wire: Arc<dyn Storage> = Arc::new(RemoteStorage::new(
        connect(serve_routes(service.shared_routes(), None).await).await,
        0,
    ));
    let both = [local, wire];

    for storage in &both {
        let missing = storage.find_durable(&[ram]).await;
        assert!(
            matches!(&missing, Err(StorageError::Unavailable(why)) if why.to_string().contains("catching up")),
            "{missing:?}"
        );
        assert_eq!(storage.find_durable(&[disk]).await.expect("served"), []);
    }

    catch_up.finish();
    for storage in &both {
        assert_eq!(
            storage.find_durable(&[disk, ram]).await.expect("served"),
            [ram]
        );
    }
}

/// A storage that holds nothing in RAM, so its `find_durable` is its `find_missing`.
struct Plain(Arc<Mem>);

impl Storage for Plain {
    fn find_missing<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> crate::Answer<'a, Vec<kbf_types::Digest>> {
        self.0.find_missing(digests)
    }

    /// Holds nothing in RAM: durable is present.
    fn find_durable<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> crate::Answer<'a, Vec<kbf_types::Digest>> {
        self.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a kbf_types::Digest) -> crate::Answer<'a, Bytes> {
        self.0.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a kbf_types::Digest,
        offset: u64,
        limit: u64,
    ) -> crate::Answer<'a, crate::ReadStream> {
        self.0.read_range(digest, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> crate::Answer<'_, kbf_types::Digest> {
        self.0.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn crate::PutStream>> {
        self.0.stream(size)
    }

    fn segments(&self) -> Arc<dyn kbf_proto::worker::segments_server::Segments> {
        self.0.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a kbf_types::Digest,
    ) -> crate::Answer<'a, Option<(kbf_types::Digest, kbf_proto::reapi::ActionResult)>> {
        self.0.action_result(action)
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a kbf_proto::reapi::ActionResult,
    ) -> crate::Answer<'a, kbf_meta::ActionRecord> {
        self.0.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: kbf_meta::Role,
        action: kbf_types::Digest,
        record: kbf_meta::ActionRecord,
    ) -> crate::Answer<'_, ()> {
        self.0.commit_action_record(role, action, record)
    }
}

/// Catches a `find_durable` of such a storage that answers something other than the storage's own
/// `find_missing` (a storage with nothing in RAM is durable wherever it is present), or
/// reaches the store another way: it names the absent digest missing and
/// the present one not, through `find_missing` alone.
#[tokio::test]
async fn a_storage_with_nothing_in_ram_answers_the_durable_question_as_the_plain_one() {
    let mem = Arc::new(Mem::default());
    let (disk, _) = held_in(&mem, b"on disk", b"also held");
    mem.ram_only.lock().expect("lock").clear();
    let absent = sha(b"nowhere");
    let plain = Plain(Arc::clone(&mem));
    assert_eq!(
        plain.find_durable(&[disk, absent]).await.expect("answer"),
        [absent]
    );
    assert_eq!(mem.knobs.asked.load(Ordering::SeqCst), 1);
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 0);
}
