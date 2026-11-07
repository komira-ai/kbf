//! Unit tests of `--scheduler-core` (`super`).

use clap::Parser;
use clap::error::ErrorKind;
use kbf_meta::Retention;
use kbf_objstore::{Floor, FsStore};

use super::{SchedulerCore, parse, render};
use crate::test_log::capture;
use crate::{Args, RaftMetaLog, serve_with};

/// Catches a default other than `legacy` (a server started with no flag would change core),
/// `legacy` not parsed, `js` accepted, a value other than `legacy` accepted (a typo, a
/// capital, an empty value), and a `name` that is not what the flag takes (the gauge's label
/// would differ from the unit file's word).
#[test]
fn the_flag_defaults_to_legacy_and_takes_only_legacy() {
    let core = |args: &[&str]| {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(args);
        Args::try_parse_from(argv).map(|a| a.scheduler_core)
    };
    assert_eq!(core(&[]).expect("default"), SchedulerCore::Legacy);
    assert_eq!(
        core(&["--scheduler-core=legacy"]).expect("legacy"),
        SchedulerCore::Legacy
    );
    for bad in ["js", "rust", "JS", "", "legacy,js"] {
        let refused = core(&["--scheduler-core", bad]).expect_err(bad);
        assert_eq!(refused.kind(), ErrorKind::ValueValidation, "{bad:?}");
    }
    assert_eq!(SchedulerCore::Legacy.name(), "legacy");
    assert_eq!(SchedulerCore::default(), SchedulerCore::Legacy);
}

/// Catches `js` refused with any text but the one that says the core was removed and where
/// scheduling runs, and an unknown word refused with the same text as `js` (a typo would be
/// told the core was removed).
#[test]
fn the_parser_refuses_js_and_unknown_words_with_their_own_messages() {
    assert_eq!(parse("legacy").expect("legacy"), SchedulerCore::Legacy);
    assert_eq!(
        parse("js").expect_err("js").to_string(),
        "the js scheduler core was removed: scheduling runs in the job service; use \
         --scheduler-core legacy or leave the flag out"
    );
    for word in ["rust", "JS", "", "Legacy"] {
        assert_eq!(
            parse(word).expect_err(word).to_string(),
            "the one scheduler core is legacy",
            "{word:?}"
        );
    }
}

/// Catches `--scheduler-core js` reaching a running server: clap refuses it, naming the flag
/// and the value and carrying the removal text, so the start ends with exit status 2 before
/// anything opens.
#[test]
fn a_start_given_js_is_refused_by_the_parser() {
    let refused = Args::try_parse_from(["kbf-server", "--scheduler-core=js"]).expect_err("js");
    assert_eq!(refused.kind(), ErrorKind::ValueValidation);
    assert_eq!(refused.exit_code(), 2);
    let said = refused.to_string();
    assert!(
        said.contains(
            "invalid value 'js' for '--scheduler-core <SCHEDULER_CORE>': the js scheduler core \
             was removed: scheduling runs in the job service; use --scheduler-core legacy or \
             leave the flag out"
        ),
        "{said}"
    );
}

/// Catches a check that refuses a server given `legacy` or no flag.
#[test]
fn the_legacy_core_passes_the_check() {
    for flags in [&[][..], &["--scheduler-core=legacy"]] {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
            .check_scheduler_core()
            .unwrap_or_else(|e| panic!("{flags:?}: {e}"));
    }
}

/// Catches `--learned-sizes on` and `--no-memory-climb` refused with the legacy core, where
/// both are features.
#[test]
fn learned_sizes_on_and_no_memory_climb_are_accepted() {
    for flags in [
        &["--learned-sizes=on"][..],
        &["--no-memory-climb"],
        &[
            "--scheduler-core=legacy",
            "--learned-sizes=on",
            "--no-memory-climb",
        ],
    ] {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
            .check_scheduler_core()
            .unwrap_or_else(|e| panic!("{flags:?}: {e}"));
    }
}

/// Catches the start line missing the core, or naming another one.
#[test]
fn the_start_line_names_the_core() {
    assert_eq!(SchedulerCore::Legacy.start_line(), "scheduler core: legacy");
}

/// Catches a gauge that is not the one `legacy` series at 1, and a series for a core that
/// does not exist.
#[test]
fn the_gauge_is_one_series_for_the_legacy_core() {
    let mut out = String::new();
    render(&mut out);
    assert_eq!(
        out,
        "# HELP kbf_scheduler_core The scheduler core the server runs: 1 for it\n\
         # TYPE kbf_scheduler_core gauge\n\
         kbf_scheduler_core{core=\"legacy\"} 1\n"
    );
    assert!(!out.contains("core=\"js\""), "{out}");
}

/// Catches a start that does not log the core it runs, and one that logs it more than
/// once. A server over a log and store the test opened logs `scheduler core: legacy` once and
/// `memory climb: on`; one given `--no-memory-climb` logs `memory climb: off`.
#[tokio::test(flavor = "current_thread")]
async fn a_start_logs_the_core_it_runs() {
    let root = crate::test_dir::scratch_dir("kbf-server-scheduler-core");
    let open = |name: &'static str| {
        let root = root.to_path_buf();
        async move {
            let log = RaftMetaLog::open_dir(&root.join(name), Retention::default(), "kbf/")
                .await
                .expect("open the log");
            let (prefix, id) = (log.store_prefix(), log.log_id());
            let store = FsStore::open(&root.join(format!("{name}-blobs")), Floor::Bytes(1))
                .expect("open the store");
            (log, store, prefix, id)
        }
    };
    let flags = ["--listen=127.0.0.1:0", "--worker-listen=127.0.0.1:0"];
    let log = capture();

    let (meta, store, prefix, id) = open("legacy").await;
    let args = Args::parse_from(["kbf-server"].into_iter().chain(flags));
    let server = serve_with(&args, None, meta, store, prefix, id, || {
        Ok(std::future::pending::<()>())
    })
    .await
    .expect("legacy starts");
    drop(server);
    let said = log.text();
    assert_eq!(said.matches("scheduler core: legacy").count(), 1, "{said}");
    assert_eq!(said.matches("memory climb: on").count(), 1, "{said}");
    assert_eq!(said.matches("memory climb: off").count(), 0, "{said}");

    let (meta, store, prefix, id) = open("no-climb").await;
    let args = Args::parse_from(["kbf-server", "--no-memory-climb"].into_iter().chain(flags));
    let server = serve_with(&args, None, meta, store, prefix, id, || {
        Ok(std::future::pending::<()>())
    })
    .await
    .expect("legacy without the climb starts");
    drop(server);
    let said = log.text();
    assert_eq!(said.matches("scheduler core: legacy").count(), 2, "{said}");
    assert_eq!(said.matches("memory climb: off").count(), 1, "{said}");
    assert_eq!(said.matches("memory climb: on").count(), 1, "{said}");
}

/// Catches a refusal of `js` that comes after anything is opened: clap refuses the flag
/// before `start` runs, so none of the store, metadata, state or log paths it was given
/// exists, and no core is logged.
#[tokio::test(flavor = "current_thread")]
async fn js_is_refused_before_anything_opens() {
    let root = crate::test_dir::scratch_dir("kbf-server-scheduler-core-refused");
    let log = capture();
    let (cold, hot, meta, state) = (
        root.join("start-cold"),
        root.join("start-hot"),
        root.join("start-meta"),
        root.join("start-state").join("state.db"),
    );
    let refused = Args::try_parse_from([
        "kbf-server".to_owned(),
        "--scheduler-core=js".to_owned(),
        format!("--cold-store=file://{}", cold.display()),
        format!("--hot-store=file://{}", hot.display()),
        format!("--meta-dir={}", meta.display()),
        format!("--state-store=sqlite://{}", state.display()),
        format!("--log-dir={}", root.join("start-log").display()),
    ])
    .expect_err("js is refused");
    assert_eq!(refused.exit_code(), 2);
    for path in [&cold, &hot, &meta, &state, &root.join("start-log")] {
        assert!(!path.exists(), "{} was created", path.display());
    }
    assert!(
        !log.text().contains("scheduler core:"),
        "a refused start logs no core"
    );
}
