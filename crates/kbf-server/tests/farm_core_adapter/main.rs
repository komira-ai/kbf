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

/// Whether `line` is the write of a lease row.
fn is_lease_row(line: &str) -> bool {
    line.trim_start().starts_with("put lease/")
}

/// The length a written row's line gives, in bytes.
fn row_len(line: &str) -> Option<usize> {
    let (_, rest) = line.split_once(" len=")?;
    rest.split_once(' ')?.0.parse().ok()
}

/// `text` with the length and hash of every lease row left out.
///
/// The one difference between the rows of the two drivers: a lease row the core writes
/// carries the cost admission charged the lease (a field the log's lease record has since the
/// scheduler library was raised, which an older binary skips), and one the farm's own driver
/// writes does not. Every other row, and every line that is not a row, is compared as it
/// is.
fn without_lease_costs(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(
            |line| match (is_lease_row(line), line.split_once(" len=")) {
                (true, Some((head, rest))) => {
                    let ttl = rest.split_once(" ttl=").map_or("", |(_, ttl)| ttl);
                    format!("{head} ttl={ttl}")
                }
                _ => line.to_owned(),
            },
        )
        .collect();
    lines.join("\n") + "\n"
}

/// The lines of `got` that are not the golden file's `want`, each with its golden line.
fn changed_lines<'a>(want: &'a str, got: &'a str) -> Vec<(&'a str, &'a str)> {
    let pairs = want.lines().zip(got.lines());
    pairs.filter(|(want, got)| want != got).collect()
}

/// Compares the text the core wrote for a script with the golden file of the farm: the same
/// text but for the cost in each lease row, which only grows the row.
fn check(name: &str, text: &str) {
    let path = golden_path(name);
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no golden file {}: {e}", path.display()));
    let (masked_want, masked_text) = (without_lease_costs(&want), without_lease_costs(text));
    if let Some(difference) = first_difference(&masked_want, &masked_text) {
        panic!(
            "the core did not do what the farm does in {}\n{difference}",
            path.display()
        );
    }
    assert_eq!(want.lines().count(), text.lines().count());
    for (want, got) in changed_lines(&want, text) {
        assert!(
            is_lease_row(want) && is_lease_row(got),
            "{want:?} became {got:?}"
        );
        let (want, got) = (row_len(want), row_len(got));
        assert!(
            got > want,
            "a lease row with a cost is longer: {want:?} became {got:?}"
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

/// Catches a mask that sets aside more than the length and hash of a lease row: the key, the
/// condition and the time to live of the row, and every other row, are still compared.
#[test]
fn only_the_length_and_hash_of_a_lease_row_are_set_aside() {
    let lease = |key: &str, len: usize, ttl: &str| {
        format!("    put lease/{key} cond=absent len={len} sha=ab ttl={ttl}\n")
    };
    let masked = |text: String| without_lease_costs(&text);

    assert_eq!(masked(lease("7.0", 20, "-")), masked(lease("7.0", 42, "-")));
    assert_ne!(masked(lease("7.0", 20, "-")), masked(lease("7.1", 20, "-")));
    assert_ne!(
        masked(lease("7.0", 20, "-")),
        masked(lease("7.0", 20, "60ms"))
    );
    let op = |len| format!("    put op/0 cond=absent len={len} sha=ab ttl=-\n");
    assert_ne!(masked(op(75)), masked(op(76)));
    assert_eq!(
        row_len("    put lease/7.0 cond=absent len=42 sha=ab ttl=-"),
        Some(42)
    );
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
