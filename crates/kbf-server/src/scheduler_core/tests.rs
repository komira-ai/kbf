//! Unit tests of `--scheduler-core` (`super`).

use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};
use kbf_meta::Retention;
use kbf_objstore::{Floor, FsStore};

use super::{SchedulerCore, render};
use crate::start_exit::{REFUSED, StartFailure};
use crate::test_log::capture;
use crate::{Args, ConfigError, RaftMetaLog, serve_with, start};

/// Catches a default other than `legacy` (a server started with no flag would change
/// core), `legacy` or `js` not parsed, a value other than the two accepted (a typo, a
/// capital, an empty value), and a `name` that is not what the flag takes (the gauge's
/// label would differ from the unit file's word).
#[test]
fn the_flag_defaults_to_legacy_and_takes_only_its_two_values() {
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
    assert_eq!(
        core(&["--scheduler-core", "js"]).expect("js"),
        SchedulerCore::Js
    );
    for bad in ["rust", "JS", "", "legacy,js"] {
        let refused = core(&["--scheduler-core", bad]).expect_err(bad);
        assert_eq!(refused.kind(), ErrorKind::InvalidValue, "{bad:?}");
    }
    for value in SchedulerCore::value_variants() {
        let possible = value.to_possible_value().expect("a value");
        assert_eq!(possible.get_name(), value.name());
    }
    assert_eq!(SchedulerCore::default(), SchedulerCore::Legacy);
}

/// Catches `js` refused by the check in a build that has it (every build does: a unit file
/// naming the js core would not start), and `legacy` refused.
#[test]
fn both_cores_pass_the_check_in_every_build() {
    for flags in [
        &[][..],
        &["--scheduler-core=legacy"],
        &["--scheduler-core=js"],
    ] {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv)
            .check_scheduler_core()
            .unwrap_or_else(|e| panic!("{flags:?}: {e}"));
    }
}

/// Catches the flag not reaching `Listeners` (a field left at a fixed value whatever the
/// flag says), and `under_test()` naming a core other than the one this build runs.
#[test]
fn the_parsed_flag_reaches_the_listeners() {
    let core = |args: &[&str]| {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(args);
        Args::parse_from(argv)
            .listeners()
            .expect("listeners")
            .scheduler_core
    };
    assert_eq!(core(&[]), SchedulerCore::Legacy);
    assert_eq!(core(&["--scheduler-core=legacy"]), SchedulerCore::Legacy);
    assert_eq!(core(&["--scheduler-core=js"]), SchedulerCore::Js);
    assert_eq!(
        SchedulerCore::under_test() == SchedulerCore::Js,
        cfg!(feature = "farm-core-js")
    );
}

/// Catches the start line missing the core, or naming another one.
#[test]
fn the_start_line_names_the_core() {
    assert_eq!(SchedulerCore::Legacy.start_line(), "scheduler core: legacy");
    assert_eq!(SchedulerCore::Js.start_line(), "scheduler core: js");
}

/// Catches a gauge that is not one series per core, 1 for the running core and 0 for the
/// other (a dashboard could not tell a core that is off from a server that reports none),
/// and a label other than the flag's value.
#[test]
fn the_gauge_is_one_for_the_running_core_and_zero_for_the_other() {
    let text = |running| {
        let mut out = String::new();
        render(&mut out, running);
        out
    };
    let head = "# HELP kbf_scheduler_core The scheduler core the server runs: 1 for it, 0 for the others\n\
                # TYPE kbf_scheduler_core gauge\n";
    assert_eq!(
        text(SchedulerCore::Legacy),
        format!(
            "{head}kbf_scheduler_core{{core=\"legacy\"}} 1\nkbf_scheduler_core{{core=\"js\"}} 0\n"
        )
    );
    assert_eq!(
        text(SchedulerCore::Js),
        format!(
            "{head}kbf_scheduler_core{{core=\"legacy\"}} 0\nkbf_scheduler_core{{core=\"js\"}} 1\n"
        )
    );
}

/// Catches a start that does not log the core it runs, and one that logs the other core's
/// name. A server over a log and store the test opened logs `scheduler core: legacy` once
/// and no `js` line; a second one started with `--scheduler-core=js` logs `scheduler core: js`
/// once, whatever core the build's tests default to.
#[tokio::test(flavor = "current_thread")]
async fn a_start_logs_the_core_it_runs_for_each_core() {
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

    let (meta, store, prefix, id) = open("js").await;
    let args = Args::parse_from(
        ["kbf-server", "--scheduler-core=js"]
            .into_iter()
            .chain(flags),
    );
    let server = serve_with(&args, None, meta, store, prefix, id, || {
        Ok(std::future::pending::<()>())
    })
    .await
    .expect("js starts");
    drop(server);
    let said = log.text();
    assert_eq!(said.matches("scheduler core: js").count(), 1, "{said}");
    assert_eq!(said.matches("scheduler core: legacy").count(), 1, "{said}");
    assert_eq!(said.matches("memory climb: on").count(), 2, "{said}");
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
    assert_eq!(said.matches("memory climb: off").count(), 1, "{said}");
    assert_eq!(said.matches("memory climb: on").count(), 2, "{said}");
}

/// Catches a refusal of `--learned-sizes on` with the js core that comes after anything is
/// opened: `serve_with` refuses before it logs a core, and `start` before it creates any of
/// the store, metadata, state or log paths it is given.
#[tokio::test(flavor = "current_thread")]
async fn js_with_learned_sizes_on_is_refused_before_anything_opens() {
    let root = crate::test_dir::scratch_dir("kbf-server-scheduler-core-refused");
    let log = capture();
    let (cold, hot, meta, state) = (
        root.join("start-cold"),
        root.join("start-hot"),
        root.join("start-meta"),
        root.join("start-state").join("state.db"),
    );
    let args = Args::parse_from([
        "kbf-server".to_owned(),
        "--scheduler-core=js".to_owned(),
        "--learned-sizes=on".to_owned(),
        format!("--cold-store=file://{}", cold.display()),
        format!("--hot-store=file://{}", hot.display()),
        format!("--meta-dir={}", meta.display()),
        format!("--state-store=sqlite://{}", state.display()),
        format!("--log-dir={}", root.join("start-log").display()),
    ]);
    let refused = start(&args, || Ok(std::future::pending::<()>()))
        .await
        .err()
        .expect("js with learned sizes on is refused");
    assert!(refused.to_string().starts_with("--learned-sizes on"));
    assert_eq!(StartFailure::of(&*refused).status(), REFUSED);
    for path in [&cold, &hot, &meta, &state, &root.join("start-log")] {
        assert!(!path.exists(), "{} was created", path.display());
    }
    assert!(
        !log.text().contains("scheduler core:"),
        "a refused start logs no core"
    );
}

/// Catches `--learned-sizes on` accepted with the js core (which learns in shadow only, so
/// a farm told `on` would book as `off` does), and the refusal applied to the other
/// combinations: `shadow` and `off` on js, and `on` on the legacy core.
#[test]
fn learned_sizes_on_is_refused_with_the_js_core_only() {
    let check = |flags: &[&str]| {
        let mut argv = vec!["kbf-server"];
        argv.extend_from_slice(flags);
        Args::parse_from(argv).check_scheduler_core()
    };
    let refused = check(&["--scheduler-core=js", "--learned-sizes=on"]).expect_err("on");
    assert!(matches!(refused, ConfigError::LearnedSizesOnUnderJs));
    let said = refused.to_string();
    assert!(said.contains("--learned-sizes on"), "{said}");
    assert!(said.contains("--scheduler-core js"), "{said}");
    assert_eq!(StartFailure::of(&refused).status(), REFUSED);
    for mode in ["shadow", "off"] {
        let flag = format!("--learned-sizes={mode}");
        check(&["--scheduler-core=js", &flag]).unwrap_or_else(|e| panic!("{mode}: {e}"));
    }
    check(&["--scheduler-core=legacy", "--learned-sizes=on"]).expect("legacy on");
}
