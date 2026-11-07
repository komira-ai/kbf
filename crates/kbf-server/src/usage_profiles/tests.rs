//! Unit tests of `--usage-profiles` (`super`): the flag, what a server refuses to start with,
//! the start line and the gauge.

use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};
use kbf_front::Cache;
use kbf_usage::UsageMode;

use super::{UsageProfiles, UsageProfilesError, render};
use crate::start_exit::{REFUSED, StartFailure};
use crate::test_log::capture;
use crate::{Args, ConfigError, Farm};

const STORE: &str = "--state-store=sqlite:///nowhere/state.db";

fn parse(args: &[&str]) -> Result<Args, clap::Error> {
    Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
}

/// What the start-time checks say about `args`: the one every start runs first, and then the
/// one that builds the listeners.
fn checked(args: &[&str]) -> Result<(), ConfigError> {
    let args = parse(args).expect("flags parse");
    args.check_scheduler_core()?;
    args.listeners().map(drop)
}

fn refused(args: &[&str]) -> UsageProfilesError {
    match checked(args).expect_err("the start is refused") {
        ConfigError::UsageProfiles(error) => error,
        other => panic!("{args:?}: another refusal: {other}"),
    }
}

/// Catches a default other than `off` (a server started with no flag would learn), a value
/// not parsed, a value other than the three accepted (a typo, a capital, an empty value), a
/// flag word that differs from `UsageMode`'s spelling, and a variant that maps to another mode.
#[test]
fn the_flag_defaults_to_off_and_takes_off_record_and_on() {
    let mode = |args: &[&str]| parse(args).expect("flags parse").usage_profiles;
    assert_eq!(mode(&[]), UsageProfiles::Off);
    assert_eq!(mode(&["--usage-profiles", "off"]), UsageProfiles::Off);
    assert_eq!(mode(&["--usage-profiles=record"]), UsageProfiles::Record);
    assert_eq!(mode(&["--usage-profiles", "on"]), UsageProfiles::On);
    for bad in ["", "ON", "true", "shadow", "off,on"] {
        let error = parse(&["--usage-profiles", bad]).expect_err(bad);
        assert_eq!(error.kind(), ErrorKind::InvalidValue, "{bad:?}");
    }
    assert_eq!(UsageProfiles::default(), UsageProfiles::Off);
    let modes: Vec<_> = UsageProfiles::value_variants()
        .iter()
        .map(|v| {
            let word = v.to_possible_value().expect("a value");
            assert_eq!(word.get_name(), v.name());
            assert_eq!(v.name().parse::<UsageMode>().expect("a mode"), v.mode());
            v.mode()
        })
        .collect();
    assert_eq!(modes, [UsageMode::Off, UsageMode::Record, UsageMode::On]);
}

/// Catches `record` or `on` accepted (with or without a state store, whatever the other
/// flags say), a refusal text other than the one that says the js core was removed and where
/// scheduling runs, a refusal that is not exit status 2, and `off` refused with or without a
/// state store.
#[test]
fn record_and_on_are_refused_because_the_js_core_was_removed() {
    for mode in ["record", "on"] {
        let flag = format!("--usage-profiles={mode}");
        let with_store = [STORE, flag.as_str()];
        let without_store = [flag.as_str()];
        let loaded = [
            STORE,
            flag.as_str(),
            "--placement-load=on",
            "--learned-sizes=shadow",
            "--placement-predictions=on",
        ];
        for args in [&with_store[..], &without_store[..], &loaded[..]] {
            let error = refused(args);
            assert!(
                matches!(&error, UsageProfilesError::JsCoreRemoved(m) if *m == mode),
                "{mode} {args:?}: {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "--usage-profiles {mode} needs the js scheduler core, which was removed: \
                     scheduling runs in the job service; leave the flag out or use \
                     --usage-profiles off"
                )
            );
            let refusal = checked(args).expect_err("refused");
            assert_eq!(StartFailure::of(&refusal).status(), REFUSED);
        }
    }
    checked(&["--usage-profiles=off"]).expect("off without a store");
    checked(&[STORE, "--usage-profiles=off"]).expect("off with a store");
}

/// The farm a server builds from `flags`, as `serve` and `frontend` both build it.
fn farm(flags: &[&str]) -> Farm {
    let args = parse(flags).expect("flags parse");
    let listeners = args.listeners().expect("listeners");
    assert_eq!(listeners.usage_profiles, args.usage_profiles);
    Farm::from_listeners(Arc::new(Cache::memory()), &listeners)
}

/// Catches a start line that is missing, empty, or names another mode, and a default that is
/// not `off`: a server started with no flag says `usage profiles: off` once, and a server
/// built from `record` says `record` and not `off`.
#[test]
fn default_is_off_and_startup_line_says_so() {
    assert_eq!(UsageProfiles::Off.start_line(), "usage profiles: off");
    assert_eq!(UsageProfiles::Record.start_line(), "usage profiles: record");
    assert_eq!(UsageProfiles::On.start_line(), "usage profiles: on");
    let log = capture();
    crate::serve::log_core(&farm(&[]));
    let said = log.text();
    assert_eq!(said.matches("usage profiles: off").count(), 1, "{said}");
    assert_eq!(said.matches("usage profiles: record").count(), 0, "{said}");
    let log = capture();
    crate::serve::log_core(&farm(&["--usage-profiles=record"]));
    let said = log.text();
    assert_eq!(said.matches("usage profiles: record").count(), 1, "{said}");
    assert_eq!(said.matches("usage profiles: off").count(), 0, "{said}");
}

/// Catches a gauge that is not one series per mode, 1 for the running mode and 0 for the
/// others, a label other than the flag's value, and a mode that does not reach the page from
/// the flags (`render_farm`, the one call both servers' `/metrics` make).
#[test]
fn metrics_mode_gauge_is_one_for_the_running_mode() {
    let text = |running| {
        let mut out = String::new();
        render(&mut out, running);
        out
    };
    let head = "# HELP kbf_usage_profiles_mode The usage profiles mode the server runs: 1 for it, 0 for the others\n\
                # TYPE kbf_usage_profiles_mode gauge\n";
    let series = |off, record, on| {
        format!(
            "{head}kbf_usage_profiles_mode{{mode=\"off\"}} {off}\n\
             kbf_usage_profiles_mode{{mode=\"record\"}} {record}\n\
             kbf_usage_profiles_mode{{mode=\"on\"}} {on}\n"
        )
    };
    assert_eq!(text(UsageProfiles::Off), series(1, 0, 0));
    assert_eq!(text(UsageProfiles::Record), series(0, 1, 0));
    assert_eq!(text(UsageProfiles::On), series(0, 0, 1));
    for (flags, expected) in [
        (&[][..], series(1, 0, 0)),
        (&["--usage-profiles=record"][..], series(0, 1, 0)),
        (&["--usage-profiles=on"][..], series(0, 0, 1)),
    ] {
        let mut out = String::new();
        crate::metrics::render_farm(&mut out, &farm(flags));
        assert!(out.contains(&expected), "{flags:?}: {out}");
    }
}
