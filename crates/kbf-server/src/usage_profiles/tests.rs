//! Unit tests of `--usage-profiles` (`super`): the flag, what a server refuses to start with,
//! the start line and the gauge.

use std::sync::Arc;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser, ValueEnum};
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

/// Catches `record` or `on` accepted on the legacy core (default or explicit), which has
/// nothing that records a run, and `off` refused on either core.
#[test]
fn usage_profiles_needs_scheduler_core_js() {
    for mode in ["record", "on"] {
        let flag = format!("--usage-profiles={mode}");
        let default: &[&str] = &[];
        let explicit: &[&str] = &["--scheduler-core=legacy"];
        for core in [default, explicit] {
            let mut args = vec![STORE, flag.as_str()];
            args.extend_from_slice(core);
            let error = refused(&args);
            assert!(
                matches!(&error, UsageProfilesError::NeedsJsCore(m) if *m == mode),
                "{mode} {core:?}: {error:?}"
            );
            let said = error.to_string();
            assert!(said.contains(&flag.replace('=', " ")), "{said}");
            assert!(said.contains("--scheduler-core js"), "{said}");
        }
    }
    for core in ["legacy", "js"] {
        let core = format!("--scheduler-core={core}");
        checked(&[core.as_str(), "--usage-profiles=off"]).expect("off is accepted");
    }
}

/// Catches `record` or `on` accepted with no `--state-store`, where the profiles would be lost
/// at every restart, and the js-core refusal coming after it for a legacy server.
#[test]
fn usage_profiles_needs_a_state_store() {
    for mode in ["record", "on"] {
        let flag = format!("--usage-profiles={mode}");
        let error = refused(&["--scheduler-core=js", flag.as_str()]);
        assert!(
            matches!(&error, UsageProfilesError::NeedsStateStore(m) if *m == mode),
            "{mode}: {error:?}"
        );
        let said = error.to_string();
        assert!(said.contains(&flag.replace('=', " ")), "{said}");
        assert!(said.contains("--state-store"), "{said}");
    }
    let both = refused(&["--usage-profiles=record"]);
    assert!(matches!(both, UsageProfilesError::NeedsJsCore("record")));
}

/// Catches `on` accepted while the scheduler books by cores (it would read no curve), or
/// without a prediction (a curve is read only for a predicted action), and a combination that
/// leaves the estimator out of it: the js core takes predictions only over
/// `--learned-sizes shadow`, so a farm that is not on it never reaches `on`.
#[test]
fn usage_profiles_on_needs_placement_load_on_and_predictions_on() {
    let base = [
        "--scheduler-core=js",
        STORE,
        "--usage-profiles=on",
        "--learned-sizes=shadow",
    ];
    let with = |more: &[&'static str]| {
        let mut args = base.to_vec();
        args.extend_from_slice(more);
        args
    };
    let no_load = refused(&with(&["--placement-predictions=on"]));
    assert!(matches!(no_load, UsageProfilesError::OnNeedsPlacementLoad));
    let said = no_load.to_string();
    assert!(said.contains("--usage-profiles on"), "{said}");
    assert!(said.contains("--placement-load on"), "{said}");
    let off_load = refused(&with(&[
        "--placement-load=off",
        "--placement-predictions=on",
    ]));
    assert!(matches!(off_load, UsageProfilesError::OnNeedsPlacementLoad));

    let no_predictions = refused(&with(&["--placement-load=on"]));
    assert!(matches!(
        no_predictions,
        UsageProfilesError::OnNeedsPredictions
    ));
    let said = no_predictions.to_string();
    assert!(said.contains("--placement-predictions on"), "{said}");
    let shadow = refused(&with(&[
        "--placement-load=on",
        "--placement-predictions=shadow",
    ]));
    assert!(matches!(shadow, UsageProfilesError::OnNeedsPredictions));

    let sound = with(&["--placement-load=on", "--placement-predictions=on"]);
    checked(&sound).expect("js, a store, load and predictions");
    let blind = [
        "--scheduler-core=js",
        STORE,
        "--usage-profiles=on",
        "--placement-load=on",
        "--placement-predictions=on",
    ];
    let error = checked(&blind).expect_err("no estimator");
    assert!(matches!(
        error,
        ConfigError::PredictionsNeedLearnedSizes("on")
    ));
    let sizes_on = [
        "--scheduler-core=js",
        STORE,
        "--usage-profiles=on",
        "--learned-sizes=on",
        "--placement-load=on",
        "--placement-predictions=on",
    ];
    let error = checked(&sizes_on).expect_err("sizes on under js");
    assert!(matches!(error, ConfigError::LearnedSizesOnUnderJs));
}

/// Catches `record` asking for more than the js core and a store (it learns and stores, and
/// reads no curve, so placement and prediction flags are none of its business), and a refusal
/// that is not exit status 2.
#[test]
fn record_needs_nothing_more_than_js_and_a_store() {
    let flags = ["--scheduler-core=js", STORE, "--usage-profiles=record"];
    checked(&flags).expect("record over js and a store");
    let mut loaded = flags.to_vec();
    loaded.push("--placement-load=on");
    checked(&loaded).expect("record with placement by load");
    let mut predicted = flags.to_vec();
    predicted.extend(["--learned-sizes=shadow", "--placement-predictions=shadow"]);
    checked(&predicted).expect("record with shadow predictions");
    let error = checked(&["--usage-profiles=record"]).expect_err("legacy");
    assert_eq!(StartFailure::of(&error).status(), REFUSED);
    let error = checked(&["--scheduler-core=js", "--usage-profiles=on"]).expect_err("no store");
    assert_eq!(StartFailure::of(&error).status(), REFUSED);
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

/// Catches the flag's help naming a staleness bound other than the one the curves use: the
/// help says how long a key may go unrun before it is charged flat, and that number is
/// `js_usage::STALE_DAYS`, read from the clap command so the two cannot drift.
#[test]
fn the_help_names_the_staleness_bound_the_curves_use() {
    let command = Args::command();
    let argument = command
        .get_arguments()
        .find(|argument| argument.get_id() == "usage_profiles")
        .expect("the flag exists");
    let help = argument
        .get_long_help()
        .or_else(|| argument.get_help())
        .expect("the flag has help")
        .to_string();
    assert!(
        help.contains(&format!("over {} days", js_usage::STALE_DAYS)),
        "{help}"
    );
}
