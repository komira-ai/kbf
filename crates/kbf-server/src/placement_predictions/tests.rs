//! Unit tests of `--placement-predictions` (`super`): the flag, and what a server refuses to
//! start with.

use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};

use super::PlacementPredictions;
use crate::{Args, ConfigError, Listeners};

fn parse(args: &[&str]) -> Result<Args, clap::Error> {
    Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
}

fn listeners(args: &[&str]) -> Result<Listeners, ConfigError> {
    parse(args).expect("flags parse").listeners()
}

fn refused(args: &[&str]) -> ConfigError {
    listeners(args).expect_err("the start is refused")
}

/// Catches a default other than `off` (a server started with no flag would predict), a value
/// not parsed, a value other than the three accepted (a typo, a capital, an empty value), a
/// flag value that does not reach the listeners that build the farm, a word the flag spells
/// differently from [`PlacementPredictions::name`], and `predicts` wrong for a mode.
#[test]
fn the_flag_defaults_to_off_and_takes_off_shadow_and_on() {
    let mode = |args: &[&str]| parse(args).expect("flags parse").placement_predictions;
    assert_eq!(mode(&[]), PlacementPredictions::Off);
    assert_eq!(
        mode(&["--placement-predictions", "off"]),
        PlacementPredictions::Off
    );
    assert_eq!(
        mode(&["--placement-predictions=shadow"]),
        PlacementPredictions::Shadow
    );
    assert_eq!(
        mode(&["--placement-predictions", "on"]),
        PlacementPredictions::On
    );
    for bad in ["", "ON", "true", "learned", "off,on"] {
        let error = parse(&["--placement-predictions", bad]).expect_err(bad);
        assert_eq!(error.kind(), ErrorKind::InvalidValue, "{bad:?}");
    }
    assert_eq!(PlacementPredictions::default(), PlacementPredictions::Off);
    let words: Vec<_> = PlacementPredictions::value_variants()
        .iter()
        .map(|v| {
            let word = v.to_possible_value().expect("a value");
            assert_eq!(word.get_name(), v.name());
            v.name()
        })
        .collect();
    assert_eq!(words, ["off", "shadow", "on"]);
    let predicts: Vec<_> = PlacementPredictions::value_variants()
        .iter()
        .map(|v| v.predicts())
        .collect();
    assert_eq!(predicts, [false, true, true]);
}

/// Catches a mode the listeners do not carry to the farm, and an accepted combination that is
/// refused: off with nothing else, shadow over a learning farm (with placement by load or
/// not), and on over a learning farm that places by load, with either learning mode.
#[test]
fn the_listeners_carry_the_mode_and_every_sound_combination_is_accepted() {
    let carried = |args: &[&str]| listeners(args).expect("accepted").placement_predictions;
    assert_eq!(carried(&[]), PlacementPredictions::Off);
    for sizes in ["--learned-sizes=shadow", "--learned-sizes=on"] {
        for load in ["--placement-load=off", "--placement-load=on"] {
            let args = [sizes, load, "--placement-predictions=shadow"];
            assert_eq!(carried(&args), PlacementPredictions::Shadow, "{args:?}");
        }
        let args = [sizes, "--placement-load=on", "--placement-predictions=on"];
        assert_eq!(carried(&args), PlacementPredictions::On, "{args:?}");
    }
    let off = ["--placement-load=off", "--placement-predictions=off"];
    assert_eq!(carried(&off), PlacementPredictions::Off);
}

/// Catches `shadow` or `on` accepted with `--learned-sizes off` (explicit or by default),
/// where the estimator is never asked and every operation would go without a prediction.
#[test]
fn shadow_and_on_are_refused_without_learned_sizes() {
    for mode in ["shadow", "on"] {
        let flag = format!("--placement-predictions={mode}");
        let default: &[&str] = &[];
        let explicit: &[&str] = &["--learned-sizes=off"];
        for sizes in [default, explicit] {
            let mut args = vec!["--placement-load=on", flag.as_str()];
            args.extend_from_slice(sizes);
            let error = refused(&args);
            assert!(
                matches!(&error, ConfigError::PredictionsNeedLearnedSizes(m) if *m == mode),
                "{mode} {sizes:?}: {error:?}"
            );
            let said = error.to_string();
            assert!(
                said.contains(&flag.replace('=', " ")) && said.contains("--learned-sizes"),
                "{said}"
            );
        }
    }
}

/// Catches `on` accepted while the scheduler books by cores (it would ignore every
/// prediction), and `shadow` refused for it (shadow sets nothing, so placement does not
/// matter to it).
#[test]
fn on_is_refused_without_placement_load_on_and_shadow_is_not() {
    let flags = ["--learned-sizes=shadow", "--placement-predictions=on"];
    let error = refused(&flags);
    assert!(
        matches!(error, ConfigError::PredictionsNeedPlacementLoad),
        "{error:?}"
    );
    let said = error.to_string();
    assert!(
        said.contains("--placement-predictions on") && said.contains("--placement-load on"),
        "{said}"
    );
    let explicit = [flags[0], flags[1], "--placement-load=off"];
    assert!(matches!(
        refused(&explicit),
        ConfigError::PredictionsNeedPlacementLoad
    ));
    let shadow = ["--learned-sizes=shadow", "--placement-predictions=shadow"];
    assert!(listeners(&shadow).is_ok());
}

/// Catches the js core refused or the flag's other rules dropped under it: `shadow` and `on`
/// are accepted with `--scheduler-core js` (its adapter reads the flag), and still refused
/// there without the estimator (`on` also without the load).
#[test]
fn shadow_and_on_are_accepted_on_the_js_core_under_the_same_rules() {
    for mode in ["shadow", "on"] {
        let flag = format!("--placement-predictions={mode}");
        let args = [
            "--learned-sizes=shadow",
            "--placement-load=on",
            "--scheduler-core=js",
            flag.as_str(),
        ];
        assert!(listeners(&args).is_ok(), "{mode}");
        let blind = ["--scheduler-core=js", "--placement-load=on", flag.as_str()];
        assert!(
            matches!(
                refused(&blind),
                ConfigError::PredictionsNeedLearnedSizes(m) if m == mode
            ),
            "{mode}"
        );
    }
    let dark = [
        "--learned-sizes=shadow",
        "--scheduler-core=js",
        "--placement-predictions=on",
    ];
    assert!(matches!(
        refused(&dark),
        ConfigError::PredictionsNeedPlacementLoad
    ));
    let off = ["--scheduler-core=js", "--placement-predictions=off"];
    assert!(listeners(&off).is_ok(), "off asks nothing of the core");
}
