//! A scripted session over the real farm, and a golden file of everything it did.
//!
//! Each script in `scenarios.rs` drives [`kbf_server::Farm`] through its public calls
//! (workers register, callers submit, nodes heartbeat and report, time passes, a process
//! dies and starts again) over the in-memory state store, on tokio's paused clock, with
//! a farm built by `Farm::for_test` so that no term and no timestamp comes from the
//! process. It writes down every row the farm asked the store to write, every message it
//! sent to a node, every action-cache entry it made and every stage its callers saw
//! (`session.rs` says how the text is laid out). `golden/<script>.txt` holds that text.
//!
//! A change of the farm's behaviour that is meant (a scheduling decision, a row's bytes, the
//! order of a start and its row) shows as a difference in a golden file, and is
//! reviewed there. Another scheduling core behind the same `Farm` must write the same
//! text for the same script.
//!
//! The store is the in-memory fake: the durable path under the farm needs no database for
//! the rows to be the same, and a database would make the text depend on the server.
//!
//! To write the golden files again, on purpose:
//! `cargo test -p kbf-server --test farm_core_trace -- --ignored regenerate_goldens`.

mod scenarios;
mod session;
mod tape;
mod text;
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
        "first difference at line {}\n--- golden\n{}\n--- this run\n{}",
        at + 1,
        show(&want),
        show(&got)
    ))
}

/// Compares the text a script wrote with its golden file.
fn check(name: &str, text: &str) {
    let path = golden_path(name);
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "no golden file {}: {e}; write it with `cargo test -p kbf-server --test \
             farm_core_trace -- --ignored regenerate_goldens`",
            path.display()
        )
    });
    if let Some(difference) = first_difference(&want, text) {
        panic!(
            "the farm did not do what {} says\n{difference}\nIf the change is meant, write the \
             golden files again (`cargo test -p kbf-server --test farm_core_trace -- --ignored \
             regenerate_goldens`) and review the diff.",
            path.display()
        );
    }
}

macro_rules! scripts {
    ($($name:ident),* $(,)?) => {
        $(
            /// Catches a change in anything this script's golden file records.
            #[test]
            fn $name() {
                check(stringify!($name), &run(scenarios::$name()));
            }
        )*

        /// Writes every golden file again. Run on purpose, then read the diff.
        #[test]
        #[ignore = "writes the golden files; run it only on purpose"]
        fn regenerate_goldens() {
            let dir = golden_path("x").parent().expect("a directory").to_owned();
            std::fs::create_dir_all(&dir).expect("the golden directory");
            $(
                let path = golden_path(stringify!($name));
                std::fs::write(&path, run(scenarios::$name())).expect("write a golden file");
            )*
        }

        /// The names of the scripts, to check each has a golden file and the other way round.
        const SCRIPTS: &[&str] = &[$(stringify!($name)),*];
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

/// Catches a script that is not run: a golden file with no test would never be compared,
/// and a test with no golden file fails by itself.
#[test]
fn every_golden_file_belongs_to_a_script() {
    let dir = golden_path("x").parent().expect("a directory").to_owned();
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .expect("the golden directory")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    files.sort();
    let mut want: Vec<String> = SCRIPTS.iter().map(|n| format!("{n}.txt")).collect();
    want.sort();
    assert_eq!(files, want);
}

/// Catches a text that depends on anything but the script: a wall-clock time, a hash order,
/// a term from the process. The two longest scripts run twice must write the same text.
#[test]
fn a_script_run_twice_writes_the_same_text() {
    let restart = (
        run(scenarios::restart_over_one_store()),
        run(scenarios::restart_over_one_store()),
    );
    assert_eq!(first_difference(&restart.0, &restart.1), None);
    let silent = (
        run(scenarios::silent_node_requeue()),
        run(scenarios::silent_node_requeue()),
    );
    assert_eq!(first_difference(&silent.0, &silent.1), None);
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
