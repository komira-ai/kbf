//! The gate table of the bake (`docs/design/kbf-cicd.md`, section 7): every threshold the
//! evaluator holds a measurement to, and the words of the page that state it.
//!
//! The page says that a change to a `[P]` threshold is one reviewed change to the page and to
//! the evaluator's table. [`rows`] builds, from the same constants the evaluator uses, the
//! text each row of the page's table must carry, and `tests/doc_table.rs` reads the page and
//! fails when the two differ. Change a constant and not the page, or the page and not the
//! constant, and that test is red.
//!
//! The post-deploy settle window of the under-replicated row is carried by the ten minute
//! rule: the bake starts after the deploy, so a series that is above zero for longer than
//! [`UNDER_REPLICATED_SECS`] outside a node step ends the bake from its first minutes.

/// How long the bake lasts: 24 hours.
pub const BAKE_SECS: u64 = 24 * 3600;
/// The minimum load, section 7: executed actions across the window.
pub const MIN_ACTIONS: u64 = 1000;
/// The minimum load: builds across the window.
pub const MIN_BUILDS: u64 = 10;
/// The minimum load: distinct hours with load.
pub const MIN_HOURS: u64 = 3;
/// Under-replicated above zero for longer than this, outside a node step, is a failure.
pub const UNDER_REPLICATED_SECS: u64 = 600;
/// The error-rate limit is this many times the baseline ratio ...
pub const ERROR_BASELINE_FACTOR: f64 = 2.0;
/// ... or the baseline plus this many percentage points, whichever is larger.
pub const ERROR_ADD_POINTS: f64 = 0.5;
/// Queue-to-start p99 above this many times the baseline is reported (a warning).
pub const LATENCY_WARN_FACTOR: f64 = 1.5;
/// Queue-to-start p99 above this many times the baseline ends the bake red.
pub const LATENCY_RED_FACTOR: f64 = 3.0;

/// The series the evaluator reads.
pub mod metric {
    /// Farm-class failed actions, labelled by `class`. **Provisional:** M10 (#951) has not
    /// landed; until it does the series is absent and its gate is red.
    pub const FAILED_BY_CLASS: &str = "kbf_failed_actions_total";
    /// The label of [`FAILED_BY_CLASS`] that names the failure class.
    pub const CLASS_LABEL: &str = "class";
    /// Blobs the cache has lost, now.
    pub const LOST_BLOBS: &str = "kbf_cache_lost_blobs";
    /// Blobs the cache has lost, ever.
    pub const BLOB_LOSSES: &str = "kbf_cache_blob_losses_total";
    /// Blobs with fewer copies than they should have; present only while known.
    pub const UNDER_REPLICATED: &str = "kbf_under_replicated_blobs";
    /// Handovers that lost a call.
    pub const RELAY_UNTRACED: &str = "kbf_frontend_relay_untraced";
    /// Execute requests.
    pub const EXECUTE_REQUESTS: &str = "kbf_execute_requests_total";
    /// Execute errors.
    pub const EXECUTE_ERRORS: &str = "kbf_execute_errors_total";
    /// Queue-to-start seconds (a histogram).
    pub const QUEUED_SECONDS: &str = "kbf_execute_queued_seconds";
    /// Write-tier flushes that failed.
    pub const FLUSH_FAILURES: &str = "kbf_write_tier_flush_failures_total";
    /// Flushed bytes whose digest was not the one expected.
    pub const DIGEST_MISMATCHES: &str = "kbf_tier_flush_digest_mismatch_total";
}

/// Whether a failure class is one the farm is answerable for: `infra`, and every `lost_*`
/// (the classes of `invocation.proto` that are not the user's action failing).
pub fn is_farm_class(class: &str) -> bool {
    class == "infra" || class.starts_with("lost_")
}

/// What one row of the page's table must say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The first cell: the gate.
    pub gate: &'static str,
    /// The fourth cell, exactly.
    pub hard: String,
    /// Series names the measure cell must name.
    pub measure_has: Vec<&'static str>,
    /// Text the threshold cell must contain.
    pub threshold_has: Vec<String>,
}

/// How the page words a factor: twice, or `N times`.
fn times(factor: f64) -> String {
    let n = format!("{factor}");
    if n == "2" {
        "twice".to_owned()
    } else {
        format!("{n} times")
    }
}

/// The rows of the page's table, in its order.
pub fn rows() -> Vec<Row> {
    let yes = || "yes".to_owned();
    vec![
        Row {
            gate: "no infra-attributed failures",
            hard: yes(),
            measure_has: vec![],
            threshold_has: vec!["0 over the bake".into()],
        },
        Row {
            gate: "lost blobs",
            hard: yes(),
            measure_has: vec![metric::LOST_BLOBS, metric::BLOB_LOSSES],
            threshold_has: vec!["gauge 0; delta 0".into()],
        },
        Row {
            gate: "under-replicated",
            hard: yes(),
            // The page names the series without the `kbf_` prefix.
            measure_has: vec![metric::UNDER_REPLICATED.trim_start_matches("kbf_")],
            threshold_has: vec![
                "0 at the end".into(),
                format!(
                    "above 0 for more than **[P]** {} minutes outside a node step",
                    UNDER_REPLICATED_SECS / 60
                ),
            ],
        },
        Row {
            gate: "handover losses",
            hard: yes(),
            measure_has: vec![metric::RELAY_UNTRACED],
            threshold_has: vec!["0".into()],
        },
        Row {
            gate: "error rate",
            hard: yes(),
            measure_has: vec![metric::EXECUTE_ERRORS, metric::EXECUTE_REQUESTS],
            threshold_has: vec![format!(
                "at most the larger of {} the baseline and the baseline plus **[P]** {ERROR_ADD_POINTS} percentage points",
                times(ERROR_BASELINE_FACTOR)
            )],
        },
        Row {
            gate: "latency",
            hard: format!("no, reported; yes if above {LATENCY_RED_FACTOR} times"),
            measure_has: vec![metric::QUEUED_SECONDS],
            threshold_has: vec![format!(
                "not above **[P]** {LATENCY_WARN_FACTOR} times the baseline"
            )],
        },
        Row {
            gate: "write tier",
            hard: yes(),
            measure_has: vec![metric::DIGEST_MISMATCHES],
            threshold_has: vec!["no new failures or mismatches".into()],
        },
        Row {
            gate: "restarts",
            hard: yes(),
            measure_has: vec![],
            threshold_has: vec!["no process restarted that the deploy did not restart".into()],
        },
    ]
}

/// Sentences of the page's prose that state the load floors and the length of the bake:
/// whitespace-collapsed text the section must contain.
pub fn prose() -> Vec<String> {
    vec![
        format!("## 7. The {} hour bake", BAKE_SECS / 3600),
        format!(
            "at least **[P]** {MIN_ACTIONS} executed actions and {MIN_BUILDS} builds across the window, and at least {MIN_HOURS} distinct hours with load"
        ),
    ]
}
