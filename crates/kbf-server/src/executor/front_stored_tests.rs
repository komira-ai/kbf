//! An action tree named before it is written: the mint answers with the tree's name while its
//! writes are held, [`Ingest::stored`] waits for them, the tree is remembered only once its
//! writes succeeded, and a failed write is the wait's answer.

use super::put_dir::{DirPrep, WALL, action_with_input, rig};
use super::*;
use crate::executor::metrics::Metrics;

fn run(n: u64) -> RunId {
    RunId::new(OperationId(n), 0)
}

/// The manifest object of the tree named `tree`.
fn manifest(tree: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/l/{tree}")).expect("a key")
}

/// One series of the steps family on `metrics`' page: `count` or `sum` of `step`.
fn series(metrics: &Metrics, part: &str, step: &str) -> String {
    let mut page = String::new();
    metrics.render(&mut page);
    let series = format!("kbf_executor_step_seconds_{part}{{step=\"{step}\"}} ");
    page.lines()
        .find_map(|line| line.strip_prefix(&series))
        .unwrap_or_else(|| panic!("no {step} in {page}"))
        .to_owned()
}

fn counted(metrics: &Metrics, step: &str) -> String {
    series(metrics, "count", step)
}

fn summed(metrics: &Metrics, step: &str) -> String {
    series(metrics, "sum", step)
}

/// Catches a mint that waits for the tree's writes, or a wait that answers before they end:
/// with every write held, the mint answers and names the tree, the tree's root is not in the
/// store, the wait does not end; once the writes are let through, the wait ends and the root
/// is stored.
#[tokio::test]
async fn a_mint_names_its_tree_while_the_writes_are_held_and_stored_waits_for_them() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    rig.store.hold_puts.store(true, Ordering::SeqCst);
    rig.front
        .mint(run(1), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    let tree = rig.front.tree(run(1)).await.expect("a name");
    assert_eq!(tree.len(), 64, "{tree}");
    assert_eq!(
        rig.store.puts.load(Ordering::SeqCst),
        0,
        "no write made yet"
    );
    assert_eq!(rig.store.len(&manifest(&tree)).await.expect("asked"), None);
    let stored = rig.front.stored(run(1));
    tokio::pin!(stored);
    for _ in 0..16 {
        tokio::task::yield_now().await;
        assert!(
            futures::poll!(stored.as_mut()).is_pending(),
            "the wait ends only with the writes"
        );
    }
    rig.store.released.add_permits(1_000);
    stored.await.expect("stored");
    assert!(
        rig.store
            .len(&manifest(&tree))
            .await
            .expect("asked")
            .is_some()
    );
}

/// Catches a tree remembered before it is stored: a second run of the action while the first
/// one's writes are held collects its closure again, and a run after the writes ended mints
/// its directory alone.
#[tokio::test]
async fn a_tree_is_remembered_only_once_its_writes_succeed() {
    let prep = DirPrep::new(None, 7_200);
    let rig = super::put_dir::rig_with(Arc::clone(&prep), true);
    let action = action_with_input(&rig, "input").await;
    rig.store.hold_puts.store(true, Ordering::SeqCst);
    for n in [1, 2] {
        rig.front
            .mint(run(n), &action, WALL, InputTree::Manifests)
            .await
            .expect("a dir");
    }
    assert_eq!((prep.prepared(), prep.minted()), (2, 0));
    rig.store.released.add_permits(1_000);
    rig.front.stored(run(1)).await.expect("stored");
    rig.front.stored(run(2)).await.expect("stored");
    rig.front
        .mint(run(3), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    assert_eq!((prep.prepared(), prep.minted()), (2, 1));
    rig.front.stored(run(3)).await.expect("nothing to wait for");
}

/// Catches a failed write lost or remembered as a tree: the mint still names the tree, the wait
/// answers the store's failure as the farm's, and the next run of the action collects its
/// closure and writes the tree again.
#[tokio::test]
async fn a_failed_tree_write_is_the_wait_answer_and_is_not_remembered() {
    for form in [InputTree::Manifests, InputTree::Provider] {
        let prep = DirPrep::new(None, 7_200);
        let base = match form {
            InputTree::Manifests => super::put_dir::base(),
            InputTree::Provider => super::put_dir::listed_base(),
        };
        let rig = super::put_dir::rig_over(Arc::clone(&prep), Some(base));
        let action = action_with_input(&rig, "input").await;
        rig.store.fail_puts.store(true, Ordering::SeqCst);
        rig.front
            .mint(run(1), &action, WALL, form)
            .await
            .expect("a dir");
        let failed = rig.front.stored(run(1)).await;
        assert!(
            matches!(&failed, Err(MintError::Unavailable(why)) if why.starts_with("no action tree")),
            "{form:?}: {failed:?}"
        );
        rig.store.fail_puts.store(false, Ordering::SeqCst);
        rig.front
            .mint(run(2), &action, WALL, form)
            .await
            .expect("a dir");
        rig.front.stored(run(2)).await.expect("stored");
        assert_eq!((prep.prepared(), prep.minted()), (2, 0), "{form:?}");
    }
}

/// Catches a wait that hangs or fails for a run with nothing to write: a run never minted, and
/// a run whose wait was already answered, are answered at once.
#[tokio::test]
async fn stored_answers_at_once_for_a_run_with_no_writes_pending() {
    let rig = rig();
    rig.front.stored(run(9)).await.expect("never minted");
    let action = action_with_input(&rig, "input").await;
    rig.front
        .mint(run(1), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    rig.front.stored(run(1)).await.expect("stored");
    rig.front
        .stored(run(1))
        .await
        .expect("answered once, then nothing left");
}

/// Catches a mint whose steps are not timed, or timed twice: a run whose tree is written counts
/// one `put_dir`, one `tree` and the closure's `action`, `walk` and `held` with the durations
/// the preparation reported; a run whose tree is remembered counts one more `put_dir` and
/// nothing else; a tree the writer refuses counts its `put_dir` and its closure and no `tree`.
#[tokio::test]
async fn a_mint_times_its_put_dir_and_its_tree_into_the_executor_metrics() {
    let rig = rig();
    let action = action_with_input(&rig, "input").await;
    let reserved = action_with_input(&rig, ".kbf-out").await;
    let metrics = Arc::new(Metrics::default());
    let front = rig.front.with_metrics(Arc::clone(&metrics));
    front
        .mint(run(1), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    front.stored(run(1)).await.expect("stored");
    assert_eq!(
        (counted(&metrics, "put_dir"), counted(&metrics, "tree")),
        ("1".to_owned(), "1".to_owned())
    );
    for (step, sum) in [
        ("action", "0.001000"),
        ("walk", "0.002000"),
        ("held", "0.003000"),
    ] {
        assert_eq!(counted(&metrics, step), "1", "{step}");
        assert_eq!(summed(&metrics, step), sum, "{step}");
    }
    front
        .mint(run(2), &action, WALL, InputTree::Manifests)
        .await
        .expect("a dir");
    assert_eq!(
        (counted(&metrics, "put_dir"), counted(&metrics, "tree")),
        ("2".to_owned(), "1".to_owned())
    );
    assert_eq!(
        counted(&metrics, "walk"),
        "1",
        "a remembered tree walks nothing"
    );
    let refused = front
        .mint(run(3), &reserved, WALL, InputTree::Manifests)
        .await;
    assert!(matches!(refused, Err(MintError::Refused(_))), "{refused:?}");
    assert_eq!(
        (counted(&metrics, "put_dir"), counted(&metrics, "tree")),
        ("3".to_owned(), "1".to_owned())
    );
    front
        .stored(run(3))
        .await
        .expect("nothing pending for a refused tree");
}
