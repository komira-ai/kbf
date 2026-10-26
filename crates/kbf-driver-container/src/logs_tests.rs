//! The logs stored with the outputs: one `put_records` call, or the intake.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_daemon::cas::{Cas, CasError, Chunks, Intake, Record, digest_of, label, proto_digest};
use kbf_daemon::cpu_pool::CpuPool;
use kbf_proto::reapi::{ActionResult, Digest};

use crate::cas::MemoryCas;
use crate::intake_tests::{Intaking, Recording};
use crate::logs::{Logs, STDERR, STDOUT, store_both};
use crate::outputs::{OutputLimits, collect};
use crate::tree::{Exceeded, TreeError};

/// A fresh scratch directory for one unit test, beside the test binary.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::unit_dir(&format!("logs-{name}"))
}

/// A store with no intake (like the transfer engine and a daemon's packed store), or with
/// the recording intake of [`Plain::with_intake`], that records the digest labels of every
/// `put_records` call and counts the blobs stored one at a time. It answers `missing` from
/// what it holds. Both kinds are one type, so each function generic over the store runs
/// every branch under one instantiation (the coverage gate counts a generic by its best).
struct Plain {
    cas: Arc<MemoryCas>,
    groups: Mutex<Vec<Vec<String>>>,
    singles: AtomicUsize,
    /// Fails every `put_records`.
    fail: bool,
    intake: Option<Arc<Recording>>,
}

impl Plain {
    fn new(fail: bool) -> Self {
        Self {
            cas: Arc::new(MemoryCas::new()),
            groups: Mutex::default(),
            singles: AtomicUsize::new(0),
            fail,
            intake: None,
        }
    }

    /// A store whose intake takes every file into its memory store, and the intake's
    /// record of what it took.
    fn with_intake() -> (Self, Arc<Recording>) {
        let mut plain = Self::new(false);
        let recording = Recording::new(Arc::clone(&plain.cas), false);
        plain.intake = Some(Arc::clone(&recording));
        (plain, recording)
    }

    fn groups(&self) -> Vec<Vec<String>> {
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn singles(&self) -> usize {
        self.singles.load(Ordering::Relaxed)
    }
}

impl Cas for Plain {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.cas.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.singles.fetch_add(1, Ordering::Relaxed);
        self.cas.put(bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        self.singles.fetch_add(1, Ordering::Relaxed);
        self.cas.put_chunks(digest, chunks).await
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        let held = |digest: &&Digest| self.cas.blob(digest).is_none();
        digests.iter().filter(held).cloned().collect()
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        let labels = records.iter().map(|r| label(&proto_digest(&r.digest)));
        self.groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(labels.collect());
        if self.fail {
            return Err(CasError::Write("records".to_owned(), "refused".to_owned()));
        }
        self.cas.put_records(records).await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        let recording = Arc::clone(self.intake.as_ref()?);
        Some(Arc::new(Intaking(recording)))
    }
}

/// Writes the two logs and one output file in a fresh upper directory and lease
/// directory, and returns `(upper, lease)`.
fn action(
    name: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> (kbf_testkit::dir::TestDir, kbf_testkit::dir::TestDir) {
    let (upper, lease) = (
        scratch(&format!("{name}-upper")),
        scratch(&format!("{name}-lease")),
    );
    std::fs::create_dir_all(upper.join("out")).expect("mkdir");
    std::fs::write(upper.join("out/file"), b"an output").expect("write");
    std::fs::write(lease.join(STDOUT), stdout).expect("write");
    std::fs::write(lease.join(STDERR), stderr).expect("write");
    (upper, lease)
}

/// Collects the output `out/file` of `upper` and the logs of `lease`, each log within
/// `max`, into `cas`.
async fn run(
    cas: &impl Cas,
    upper: &std::path::Path,
    lease: &std::path::Path,
    max: u64,
) -> Result<ActionResult, TreeError> {
    let mut result = ActionResult::default();
    collect(
        cas,
        upper,
        &CpuPool::new(2),
        "",
        &["out/file".to_owned()],
        OutputLimits::DEFAULT,
        Some(&Logs { dir: lease, max }),
        &mut result,
    )
    .await?;
    Ok(result)
}

/// Asserts that a store with no intake gets an action's output and both its logs in one
/// `put_records` call, and none through `put` or `put_chunks`, and that the result's
/// digests are the logs' digests. A pack per call in a daemon is then one pack per action.
#[tokio::test]
async fn the_outputs_and_both_logs_are_stored_in_one_call() {
    let (upper, lease) = action("one-call", b"to stdout", b"to stderr");
    let cas = Plain::new(false);
    let result = run(&cas, &upper, &lease, 100).await.expect("collected");
    let (out, err) = (digest_of(b"to stdout"), digest_of(b"to stderr"));
    assert_eq!(result.stdout_digest, Some(out.clone()));
    assert_eq!(result.stderr_digest, Some(err.clone()));
    let file = result.output_files[0].digest.clone().expect("a digest");
    let groups = cas.groups();
    assert_eq!(groups.len(), 1, "{groups:?}");
    let mut sent = groups[0].clone();
    sent.sort();
    let mut expected = [label(&file), label(&out), label(&err)];
    expected.sort();
    assert_eq!(sent, expected);
    assert_eq!(cas.singles(), 0);
    assert_eq!(cas.cas.blob(&out), Some(b"to stdout".to_vec()));
    assert_eq!(cas.cas.blob(&err), Some(b"to stderr".to_vec()));
}

/// Asserts that a log the store already holds is not sent again, while the result still
/// names it, and that two empty logs are one blob with the empty digest, sent once.
#[tokio::test]
async fn a_log_the_store_holds_is_not_sent_and_equal_logs_are_sent_once() {
    let (upper, lease) = action("held", b"held already", b"new");
    let cas = Plain::new(false);
    let held = cas.cas.insert(b"held already".to_vec());
    let result = run(&cas, &upper, &lease, 100).await.expect("collected");
    assert_eq!(result.stdout_digest, Some(held.clone()));
    let sent = cas.groups().concat();
    assert!(!sent.contains(&label(&held)), "{sent:?}");
    assert!(sent.contains(&label(&digest_of(b"new"))), "{sent:?}");

    let (upper, lease) = action("empty", b"", b"");
    let cas = Plain::new(false);
    let result = run(&cas, &upper, &lease, 100).await.expect("collected");
    let empty = digest_of(b"");
    assert_eq!(result.stdout_digest, Some(empty.clone()));
    assert_eq!(result.stderr_digest, Some(empty.clone()));
    let sent = cas.groups().concat();
    assert_eq!(sent.iter().filter(|l| **l == label(&empty)).count(), 1);
}

/// Asserts that a log of more than the limit is refused with the stdio limit before
/// anything is sent, and that one of exactly the limit is stored.
#[tokio::test]
async fn a_log_past_the_limit_is_refused_and_nothing_is_sent() {
    let (upper, lease) = action("limit", b"0123456789", b"0123456789a");
    let cas = Plain::new(false);
    let why = run(&cas, &upper, &lease, 10).await.expect_err("too long");
    assert!(
        matches!(
            why,
            TreeError::Limit {
                what: Exceeded::Stdio,
                limit: 10,
                ..
            }
        ),
        "{why}"
    );
    assert!(cas.groups().is_empty());
    assert_eq!(cas.singles(), 0);
    std::fs::write(lease.join(STDERR), b"0123456789").expect("write");
    run(&cas, &upper, &lease, 10).await.expect("at the limit");
    assert_eq!(cas.groups().len(), 1);
}

/// Asserts that a store with an intake still takes the logs through it: both logs are
/// finished there, nothing reaches `put_chunks`, and the digests are the logs'.
#[tokio::test]
async fn with_an_intake_the_logs_go_through_it() {
    let (upper, lease) = action("intake", b"to stdout", b"to stderr");
    let (cas, recording) = Plain::with_intake();
    let result = run(&cas, &upper, &lease, 100).await.expect("collected");
    let (out, err) = (digest_of(b"to stdout"), digest_of(b"to stderr"));
    assert_eq!(result.stdout_digest, Some(out.clone()));
    assert_eq!(result.stderr_digest, Some(err.clone()));
    let finished: Vec<String> = recording.finished().into_iter().map(|f| f.0).collect();
    assert!(finished.contains(&label(&out)), "{finished:?}");
    assert!(finished.contains(&label(&err)), "{finished:?}");
    assert_eq!(
        cas.groups().concat(),
        Vec::<String>::new(),
        "no blob sent in a call"
    );
    assert_eq!(cas.singles(), 0);
}

/// Asserts that a timed-out action's two logs are stored in one `put_records` call, and
/// that one past the limit is left out with its reason while the other is stored.
#[tokio::test]
async fn the_timeout_path_stores_both_logs_in_one_call() {
    let lease = scratch("timeout");
    std::fs::write(lease.join(STDOUT), b"partial").expect("write");
    std::fs::write(lease.join(STDERR), b"more").expect("write");
    let cas = Plain::new(false);
    let logs = Logs {
        dir: &lease,
        max: 100,
    };
    let (found, problems) = store_both(&cas, &logs).await;
    assert_eq!(
        found,
        [Some(digest_of(b"partial")), Some(digest_of(b"more"))]
    );
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(cas.groups().len(), 1);
    assert_eq!(cas.groups()[0].len(), 2);
    assert_eq!(cas.singles(), 0);

    let cas = Plain::new(false);
    let logs = Logs {
        dir: &lease,
        max: 5,
    };
    let (found, problems) = store_both(&cas, &logs).await;
    assert_eq!(found, [None, Some(digest_of(b"more"))]);
    assert!(
        matches!(
            problems[..],
            [TreeError::Limit {
                what: Exceeded::Stdio,
                limit: 5,
                ..
            }]
        ),
        "{problems:?}"
    );
    assert_eq!(cas.groups().len(), 1);
}

/// Asserts that a timed-out action's logs are left out, with the store's reason, when the
/// one call that stores them fails; and that with an intake each goes through it.
#[tokio::test]
async fn the_timeout_path_leaves_logs_out_when_the_store_fails_and_uses_the_intake() {
    let lease = scratch("timeout-fail");
    std::fs::write(lease.join(STDOUT), b"partial").expect("write");
    std::fs::write(lease.join(STDERR), b"more").expect("write");
    let logs = Logs {
        dir: &lease,
        max: 100,
    };
    let (found, problems) = store_both(&Plain::new(true), &logs).await;
    assert_eq!(found, [None, None]);
    assert!(matches!(problems[..], [TreeError::Cas(_)]), "{problems:?}");

    let (cas, _) = Plain::with_intake();
    let (found, problems) = store_both(&cas, &logs).await;
    assert_eq!(
        found,
        [Some(digest_of(b"partial")), Some(digest_of(b"more"))]
    );
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(
        cas.groups().concat(),
        Vec::<String>::new(),
        "no blob sent in a call"
    );
    assert_eq!(cas.singles(), 0);
    let short = Logs {
        dir: &lease,
        max: 5,
    };
    let (found, problems) = store_both(&cas, &short).await;
    assert_eq!(found[0], None);
    assert_eq!(problems.len(), 1);
}

/// Asserts that a timed-out action whose two logs are equal (here both empty, then both the
/// same text) names the one blob for both and sends it once, as the completed path does.
#[tokio::test]
async fn the_timeout_path_sends_equal_logs_once() {
    for (name, bytes) in [("timeout-empty", &b""[..]), ("timeout-equal", &b"same"[..])] {
        let lease = scratch(name);
        std::fs::write(lease.join(STDOUT), bytes).expect("write");
        std::fs::write(lease.join(STDERR), bytes).expect("write");
        let cas = Plain::new(false);
        let logs = Logs {
            dir: &lease,
            max: 100,
        };
        let (found, problems) = store_both(&cas, &logs).await;
        let digest = digest_of(bytes);
        assert_eq!(found, [Some(digest.clone()), Some(digest.clone())]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(cas.groups(), vec![vec![label(&digest)]]);
    }
}
