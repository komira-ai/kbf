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

/// One series of the steps family on `metrics`' page: `count` or `sum` of `step` for a tree of
/// `form` (`manifests` or `provider`).
fn series(metrics: &Metrics, part: &str, (step, form): (&str, &str)) -> String {
    let mut page = String::new();
    metrics.render(&mut page);
    let series = format!("kbf_executor_step_seconds_{part}{{step=\"{step}\",form=\"{form}\"}} ");
    page.lines()
        .find_map(|line| line.strip_prefix(&series))
        .unwrap_or_else(|| panic!("no {step} of {form} in {page}"))
        .to_owned()
}

/// The count of `step` for a tree of manifests.
fn counted(metrics: &Metrics, step: &str) -> String {
    series(metrics, "count", (step, "manifests"))
}

/// The sum of `step` for a tree of manifests.
fn summed(metrics: &Metrics, step: &str) -> String {
    series(metrics, "sum", (step, "manifests"))
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
        .mint(run(1), &action, WALL, InputTree::Manifests, None)
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
            .mint(run(n), &action, WALL, InputTree::Manifests, None)
            .await
            .expect("a dir");
    }
    assert_eq!((prep.prepared(), prep.minted()), (2, 0));
    rig.store.released.add_permits(1_000);
    rig.front.stored(run(1)).await.expect("stored");
    rig.front.stored(run(2)).await.expect("stored");
    rig.front
        .mint(run(3), &action, WALL, InputTree::Manifests, None)
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
            .mint(run(1), &action, WALL, form, None)
            .await
            .expect("a dir");
        let failed = rig.front.stored(run(1)).await;
        assert!(
            matches!(&failed, Err(MintError::Unavailable(why)) if why.starts_with("no action tree")),
            "{form:?}: {failed:?}"
        );
        rig.store.fail_puts.store(false, Ordering::SeqCst);
        rig.front
            .mint(run(2), &action, WALL, form, None)
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
        .mint(run(1), &action, WALL, InputTree::Manifests, None)
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
        .mint(run(1), &action, WALL, InputTree::Manifests, None)
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
        .mint(run(2), &action, WALL, InputTree::Manifests, None)
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
        .mint(run(3), &reserved, WALL, InputTree::Manifests, None)
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

/// Catches the stages of a tree's writes not timed into the executor's steps, timed twice, timed
/// as the other form's, or counted under the other form: a tree written as manifests, whose
/// blobs the rig keeps only in its CAS, counts one `tree_blobs`, one `tree_manifests` and one
/// `tree_root` once it is stored, all as `manifests`; a tree list counts one `tree_blobs`, one
/// `tree_list` and one `tree_sidecar`, all as `provider`; and neither counts anything under the
/// other form.
#[tokio::test]
async fn the_stages_of_a_trees_writes_are_timed_by_form_into_the_executor_metrics() {
    let stages = [
        "tree",
        "tree_blobs",
        "tree_manifests",
        "tree_root",
        "tree_list",
        "tree_sidecar",
    ];
    for (form, label, base, timed) in [
        (
            InputTree::Manifests,
            "manifests",
            super::put_dir::base(),
            &["tree", "tree_blobs", "tree_manifests", "tree_root"][..],
        ),
        (
            InputTree::Provider,
            "provider",
            super::put_dir::listed_base(),
            &["tree", "tree_blobs", "tree_list", "tree_sidecar"][..],
        ),
    ] {
        let prep = DirPrep::new(None, 7_200);
        let rig = super::put_dir::rig_over(prep, Some(base));
        let action = action_with_input(&rig, "input").await;
        let metrics = Arc::new(Metrics::default());
        let front = rig.front.with_metrics(Arc::clone(&metrics));
        front
            .mint(run(1), &action, WALL, form, None)
            .await
            .expect("a dir");
        front.stored(run(1)).await.expect("stored");
        for other in ["manifests", "provider"] {
            for stage in stages {
                let want = match other == label && timed.contains(&stage) {
                    true => "1",
                    false => "0",
                };
                let got = series(&metrics, "count", (stage, other));
                assert_eq!(got, want, "{form:?}: {stage} as {other}");
            }
        }
    }
}

/// One line of `family` on `metrics`' page, `labels` its braces' contents (empty: none).
fn line(metrics: &Metrics, family: &str, labels: &str) -> String {
    let mut page = String::new();
    metrics.render(&mut page);
    let series = match labels {
        "" => format!("{family} "),
        labels => format!("{family}{{{labels}}} "),
    };
    page.lines()
        .find_map(|line| line.strip_prefix(&series))
        .unwrap_or_else(|| panic!("no {series} in {page}"))
        .to_owned()
}

/// Catches a tree's writes not timed by part into the executor's metrics, timed under another
/// stage, or its manifests not counted once a tree: a tree written as manifests, through the
/// frontend's packs, counts each part of its two manifest writes (the input root's and `.kbf`'s)
/// under `tree_manifests` and of its root's under `tree_root`, nothing under `tree_list`, and one
/// tree of two manifests; a tree list counts each part but the manifest slot of its one write
/// under `tree_list`, nothing under the manifests' stages, and no tree of manifests.
#[tokio::test]
async fn a_trees_writes_are_counted_by_part_and_its_manifests_once_a_tree() {
    let parts = ["manifest_slot", "store_slot", "store_503", "request"];
    for (form, base, writes, trees) in [
        (
            InputTree::Manifests,
            super::put_dir::base(),
            [
                ("tree_manifests", "2"),
                ("tree_root", "1"),
                ("tree_list", "0"),
            ],
            ("1", "2"),
        ),
        (
            InputTree::Provider,
            super::put_dir::listed_base(),
            [
                ("tree_manifests", "0"),
                ("tree_root", "0"),
                ("tree_list", "1"),
            ],
            ("0", "0"),
        ),
    ] {
        let prep = DirPrep::new(None, 7_200);
        let rig = super::put_dir::rig_over(prep, Some(base));
        let action = action_with_input(&rig, "input").await;
        let metrics = Arc::new(Metrics::default());
        let front = rig.front.with_metrics(Arc::clone(&metrics));
        front
            .mint(run(1), &action, WALL, form, None)
            .await
            .expect("a dir");
        front.stored(run(1)).await.expect("stored");
        let family = "kbf_executor_tree_write_seconds_count";
        for (stage, want) in writes {
            for part in parts {
                if stage == "tree_list" && part == "manifest_slot" {
                    continue;
                }
                let labels = format!(r#"stage="{stage}",part="{part}""#);
                let got = line(&metrics, family, &labels);
                assert_eq!(got, want, "{form:?}: {labels}");
            }
        }
        let family = "kbf_executor_tree_manifests_written";
        let got = (
            line(&metrics, &format!("{family}_count"), ""),
            line(&metrics, &format!("{family}_sum"), ""),
        );
        let want = (trees.0.to_owned(), trees.1.to_owned());
        assert_eq!(got, want, "{form:?}");
    }
}
