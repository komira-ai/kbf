//! `PutDirRun::took`: how long the Action read, the `Directory` walk and the presence check of a
//! put-directory preparation took, by the clock of `tokio::time`.
//!
//! The tests run on a paused clock over a store whose reads and asks take the virtual time a
//! test gives them, so every duration is exact and nothing waits on the real clock.

use super::*;

/// A store that answers as [`Watched`] does after the virtual time its tests give each blob read
/// and each FindMissing call.
struct Slow {
    inner: Arc<Watched>,
    reads: Vec<(Digest, Duration)>,
    ask: Duration,
}

impl Slow {
    fn read_time(&self, digest: &Digest) -> Duration {
        let named = self.reads.iter().find(|(blob, _)| blob == digest);
        named.map_or(Duration::ZERO, |(_, time)| *time)
    }
}

impl Storage for Slow {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            tokio::time::sleep(self.ask).await;
            self.inner.find_missing(digests).await
        })
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            tokio::time::sleep(self.read_time(digest)).await;
            self.inner.read_blob(digest).await
        })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.inner.read_range(digest, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.inner.action_result(action)
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Asserts that each part is reported by the virtual time it took: the Action read (5 ms); the
/// walk, whose levels are read one after another and whose Directories within a level are read
/// together, so it takes the root (10 ms), then the slower of `lib` and `lib2` (20 and 30 ms),
/// then `deep` (40 ms), 80 ms; and the presence check (7 ms). A part that takes no time is zero.
#[tokio::test(start_paused = true)]
async fn the_parts_of_a_preparation_are_timed_by_the_clock_they_ran_on() {
    let parts = Parts::new();
    let tree = &parts.tree;
    let inner = Watched::holding(parts.blobs.clone()).await;
    let slow = Slow {
        inner,
        reads: vec![
            (parts.action, millis(5)),
            (tree.root, millis(10)),
            (tree.lib, millis(20)),
            (tree.lib2, millis(30)),
            (tree.deep, millis(40)),
        ],
        ask: millis(7),
    };
    let storage: Arc<dyn Storage> = Arc::new(slow);
    let prep = RunPrep::new(Backend::remote(storage), config()).expect("a preparer");

    let run = prep
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("prepared");

    assert_eq!(run.took.action, millis(5));
    assert_eq!(run.took.walk, millis(80));
    assert_eq!(run.took.held, millis(7));
}

/// Asserts that a store that answers at once reports every part as zero: the clock is the
/// paused one, which does not move while nothing waits.
#[tokio::test(start_paused = true)]
async fn a_store_that_answers_at_once_takes_no_time() {
    let (parts, _, prep) = standard().await;

    let run = prep
        .prepare_put_dir(LEASE, &parts.action, now(), WALL)
        .await
        .expect("prepared");

    assert_eq!(run.took, kbf_front::prepare_run::PrepareTook::default());
}
