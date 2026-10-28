//! The scripted sessions of `tests/farm_core_trace`, run on `js-server-core`'s core with
//! kbf's adapter, and compared with the golden files of the farm's own driver.
//!
//! `session.rs` here is the farm's `session.rs` over `js_server_core::Core<KbfAdapter>`: it
//! offers the same calls and writes the same words (`text.rs`, `transcript.rs`, `tape.rs`
//! are the farm's, included as they are), so the scripts of `scenarios.rs` run unchanged. A
//! script whose text is the golden file's is a core that did what the farm does: the same
//! rows in the same order, the same messages to the nodes, the same stages to the callers,
//! the same action-cache entries.
//!
//! The golden files are the farm's: they are read from `tests/farm_core_trace/golden` and
//! never written from here.
//!
//! What these scripts do not reach: the `ResultAck` the adapter's `settle` sends to a node when
//! a result was held until its record was written (no script makes the state store refuse).
//! `farm_js::dispatch_tests::a_held_result_is_acknowledged_and_answered_once_its_record_is_written`
//! pins it. Nor do they read a store written by one driver with the other.

#[path = "../farm_core_trace/scenarios.rs"]
mod scenarios;
mod session;
#[path = "../farm_core_trace/tape.rs"]
mod tape;
#[path = "../farm_core_trace/text.rs"]
mod text;
#[path = "../farm_core_trace/transcript.rs"]
mod transcript;

use std::future::Future;
use std::path::PathBuf;

fn golden_path(name: &str) -> PathBuf {
    let dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(dir)
        .join("tests/farm_core_trace/golden")
        .join(format!("{name}.txt"))
}

/// Runs `script` on a runtime of its own with a paused clock, as `#[tokio::test(start_paused
/// = true)]` does, so every script starts at the same moment of its own clock.
fn run(script: impl Future<Output = String>) -> String {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime")
        .block_on(script)
}

/// The first line at which `got` is not `want`, with its neighbours, or `None` if they
/// are the same text.
fn first_difference(want: &str, got: &str) -> Option<String> {
    if want == got {
        return None;
    }
    let (want, got): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let at = (0..want.len().max(got.len()))
        .find(|i| want.get(*i) != got.get(*i))
        .unwrap_or(0);
    let from = at.saturating_sub(3);
    let show = |lines: &[&str]| {
        lines
            .iter()
            .enumerate()
            .skip(from)
            .take(7)
            .map(|(i, l)| format!("{:>5}  {l}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Some(format!(
        "first difference at line {}\n--- the farm's golden\n{}\n--- the core\n{}",
        at + 1,
        show(&want),
        show(&got)
    ))
}

/// Compares the text the core wrote for a script with the golden file of the farm, line for
/// line. The lease rows the core writes are written down without the cost admission charged
/// the lease (`session::without_lease_cost`); nothing else is set aside.
fn check(name: &str, text: &str) {
    let path = golden_path(name);
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no golden file {}: {e}", path.display()));
    if let Some(difference) = first_difference(&want, text) {
        panic!(
            "the core did not do what the farm does in {}\n{difference}",
            path.display()
        );
    }
}

macro_rules! scripts {
    ($($name:ident),* $(,)?) => {
        $(
            /// Catches a core that does anything of this script differently from the
            /// farm: a row, a message, a stage, a cache entry.
            #[test]
            fn $name() {
                check(stringify!($name), &run(scenarios::$name()));
            }
        )*
    };
}

scripts!(
    answer_and_cache,
    qos_order,
    whole_machine,
    silent_node_requeue,
    stream_ends,
    lost_runs,
    memory_kills,
    unservable_work,
    restart_over_one_store,
    epoch_change,
    handover,
    cordon_and_drain,
);

/// Catches a normalisation that takes out more than the cost: a lease row with a cost is
/// written down as the same row without one, and as no other; a lease row that differs in
/// its lease, its operation or its node stays different; every other row is written as it is.
#[test]
fn the_lease_row_is_written_down_without_its_cost_and_with_everything_else() {
    use js_store::RowCodec as _;
    use kbf_server::farm_js::codec::KbfRowCodec;
    use kbf_types::sched::usage::LeaseCost;
    use kbf_types::{LeaseGrant, LeaseId, OperationId, WorkerId};

    let cost = LeaseCost {
        cpu_millis: 4,
        memory_bytes: 5,
        limit_bytes: 6,
        cold: true,
        retry: true,
        key_hash: 7,
    };
    let grant = |seq, operation, node: &str, cost| LeaseGrant {
        lease: LeaseId::new(7, seq),
        operation: OperationId(operation),
        worker: WorkerId::new(node),
        cost,
    };
    let row = |grant: &LeaseGrant| KbfRowCodec.lease(grant).expect("a lease row");
    let key = "lease/0000000000000007.0000000000000000";
    let written = |grant: &LeaseGrant| session::without_lease_cost(key, &row(grant));
    let plain = grant(0, 0, "node-a", LeaseCost::default());

    assert_eq!(written(&grant(0, 0, "node-a", cost)), row(&plain));
    assert_ne!(
        row(&grant(0, 0, "node-a", cost)),
        row(&plain),
        "the cost is in the row"
    );
    for other in [
        grant(1, 0, "node-a", cost),
        grant(0, 1, "node-a", cost),
        grant(0, 0, "node-b", cost),
    ] {
        assert_ne!(written(&other), row(&plain), "{other:?}");
    }
    let bytes = b"not a lease row".to_vec();
    assert_eq!(session::without_lease_cost("session/node-a", &bytes), bytes);
    assert_eq!(
        session::without_lease_cost(key, &bytes),
        bytes,
        "unreadable: as it is"
    );
    assert_eq!(
        session::without_lease_cost(
            "lease-by-op/0000000000000000",
            &row(&grant(0, 0, "node-a", cost))
        ),
        row(&grant(0, 0, "node-a", cost)),
        "only the rows of leases"
    );
}

/// The bytes of a lease row for node-a wrapped in a batch of farm time `now` and wall time
/// `wall`, as a codec that wrote other times would write it.
fn lease_row_in_batch(now: kbf_types::FarmTime, wall: std::time::SystemTime) -> Vec<u8> {
    use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
    use kbf_types::{ControlRecord, LeaseGrant, LeaseId, OperationId, WorkerId};

    let grant = LeaseGrant {
        lease: LeaseId::new(7, 0),
        operation: OperationId(0),
        worker: WorkerId::new("node-a"),
        cost: Default::default(),
    };
    let inputs = vec![FarmInput::Committed(ControlRecord::Lease(grant))];
    let batch = FarmBatch { now, wall, inputs };
    kbf_log::encode(Format::V1, &LogCommand::Farm(batch)).expect("a batch")
}

/// Catches a normalisation that hides the times a lease row's batch is written with: writing
/// the row again resets them, so a row with another wall time or farm time is reported, naming
/// the row, and not normalised; a row with the default times is normalised.
#[test]
fn a_lease_row_with_other_batch_times_is_reported_not_normalised() {
    use std::time::{Duration, UNIX_EPOCH};

    let key = "lease/0000000000000007.0000000000000000";
    let plain = lease_row_in_batch(kbf_types::FarmTime::default(), UNIX_EPOCH);
    assert_eq!(session::without_lease_cost(key, &plain), plain);

    let other_wall = lease_row_in_batch(
        kbf_types::FarmTime::default(),
        UNIX_EPOCH + Duration::from_secs(1),
    );
    let other_now = lease_row_in_batch(kbf_types::FarmTime::from_millis(5), UNIX_EPOCH);
    for row in [other_wall, other_now] {
        let reported = std::panic::catch_unwind(|| session::without_lease_cost(key, &row));
        let message = reported.expect_err("reported");
        let text = message
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(text.contains(key), "{text}");
    }
}

/// Catches a comparison that cannot fail: equal text is no difference, and a changed or
/// missing line is found where it is.
#[test]
fn the_comparison_finds_a_changed_line() {
    assert_eq!(first_difference("a\nb\nc\n", "a\nb\nc\n"), None);
    let changed = first_difference("a\nb\nc\n", "a\nx\nc\n").expect("a difference");
    assert!(changed.contains("first difference at line 2"), "{changed}");
    let shorter = first_difference("a\nb\nc\n", "a\nb\n").expect("a difference");
    assert!(shorter.contains("first difference at line 3"), "{shorter}");
}
