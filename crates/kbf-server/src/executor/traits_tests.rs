use super::RejectReason;

/// Catches a reject reason with no words (it is what an operator reads in the log when a
/// task's pack is refused), or two reasons with the same.
#[test]
fn every_reject_reason_says_something_different() {
    let all = [
        RejectReason::Missing,
        RejectReason::WrongKey,
        RejectReason::TooLarge,
        RejectReason::Malformed,
        RejectReason::HashMismatch,
        RejectReason::BadManifest,
        RejectReason::Incomplete,
        RejectReason::Expired,
        RejectReason::Claimed,
    ];
    let words: std::collections::BTreeSet<String> = all.iter().map(ToString::to_string).collect();
    assert_eq!(words.len(), all.len());
    assert!(words.iter().all(|w| !w.is_empty()));
}

/// An ingest that keeps none of the defaults' state: every method it must have refuses, and is
/// counted.
#[derive(Default)]
struct Bare(std::sync::atomic::AtomicUsize);

impl Bare {
    fn calls(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn called(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tonic::async_trait]
impl super::Ingest for Bare {
    async fn mint(
        &self,
        _run: crate::executor::RunId,
        _action: &kbf_proto::reapi::Digest,
        _wall: std::time::Duration,
        _tree: super::InputTree,
        _decoded: Option<&kbf_front::DecodedAction>,
    ) -> Result<crate::executor::PackKey, super::MintError> {
        self.called();
        Err(super::MintError::Refused("no mint here".to_owned()))
    }

    async fn ingest(
        &self,
        _run: crate::executor::RunId,
        _key: &crate::executor::PackKey,
        _action: &kbf_proto::reapi::Digest,
        _limits: &super::UntrustedLimits,
    ) -> super::Ingested {
        self.called();
        super::Ingested::Reject(RejectReason::Missing)
    }
}

/// Asserts that an ingest that starts no tree ahead of its run (the default) does nothing with
/// the start: it calls neither its mint nor its ingest (each counted, and each still answering
/// when called after), and the Execute's side of the finding it was handed still says held
/// without failing. Catches a default that mints or ingests on a start.
#[tokio::test]
async fn by_default_no_tree_is_started_ahead_of_its_run() {
    use super::Ingest as _;
    let bare = Bare::default();
    let (held, finding) = kbf_front::inputs_held();
    let decoded = std::sync::Arc::new(kbf_front::DecodedAction {
        action: kbf_proto::reapi::Action::default(),
        command: kbf_proto::reapi::Command::default(),
    });
    let digest = kbf_proto::reapi::Digest::default();
    let tree = super::InputTree::Manifests;

    bare.start_tree(&digest, decoded, tree, finding);

    held.held();
    assert_eq!(bare.calls(), 0, "the start called nothing");
    let run = crate::executor::RunId::new(kbf_types::OperationId(1), 0);
    let wall = std::time::Duration::from_secs(1);
    assert_eq!(
        bare.mint(run, &digest, wall, tree, None).await,
        Err(super::MintError::Refused("no mint here".to_owned()))
    );
    let key = crate::executor::PackKey::new("cas/b/x/y".to_owned());
    let limits = super::UntrustedLimits {
        max_pack_bytes: 1,
        max_blob_bytes: 1,
        max_object_entries: 1,
        max_object_manifest_bytes: 1,
        object_bytes_per_entry: 1,
    };
    assert!(matches!(
        bare.ingest(run, &key, &digest, &limits).await,
        super::Ingested::Reject(RejectReason::Missing)
    ));
    assert_eq!(bare.calls(), 2);
}
