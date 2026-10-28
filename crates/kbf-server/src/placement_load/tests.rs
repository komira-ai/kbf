//! Unit tests of `--placement-load` (`super`).

use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};
use kbf_sched::LoadConfig;

use super::PlacementLoad;
use crate::Args;

/// Catches a default other than `off` (a server started with no flag would place by load
/// while its daemons send none), a value not parsed, a value other than the three accepted
/// (a typo, a capital, an empty value), a flag value that does not reach the listeners that
/// build the farm, and a variant whose flag word differs from its name in the config file.
#[test]
fn the_flag_defaults_to_off_and_takes_off_and_on() {
    let parse = |args: &[&str]| {
        Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
    };
    let mode = |args: &[&str]| parse(args).expect("flags parse").placement_load;
    assert_eq!(mode(&[]), PlacementLoad::Off);
    assert_eq!(mode(&["--placement-load", "off"]), PlacementLoad::Off);
    assert_eq!(mode(&["--placement-load=on"]), PlacementLoad::On);
    for bad in ["", "ON", "true", "load", "shadow", "off,on"] {
        let refused = parse(&["--placement-load", bad]).expect_err(bad);
        assert_eq!(refused.kind(), ErrorKind::InvalidValue, "{bad:?}");
    }
    for (flag, want) in [(None, PlacementLoad::Off), (Some("on"), PlacementLoad::On)] {
        let args: Vec<String> = flag
            .map(|v| format!("--placement-load={v}"))
            .into_iter()
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let listeners = parse(&args).expect("flags parse").listeners();
        assert_eq!(listeners.expect("listeners").placement_load, want);
    }
    assert_eq!(PlacementLoad::default(), PlacementLoad::Off);
    let words: Vec<_> = PlacementLoad::value_variants()
        .iter()
        .map(|v| {
            v.to_possible_value()
                .expect("a value")
                .get_name()
                .to_owned()
        })
        .collect();
    assert_eq!(words, ["off", "on"]);
}

/// Catches a mode mapped to another of the scheduler's (`off` placing by load, `on`
/// booked), and a configuration that changes a setting other than the mode: kbf takes
/// the scheduler's own values for the rest.
#[test]
fn each_mode_maps_to_the_schedulers_and_changes_nothing_else() {
    let theirs = |ours: PlacementLoad| ours.mode();
    assert_eq!(theirs(PlacementLoad::Off), kbf_sched::PlacementLoad::Off);
    assert_eq!(theirs(PlacementLoad::On), kbf_sched::PlacementLoad::On);
    for ours in PlacementLoad::value_variants() {
        assert_eq!(
            ours.config(),
            LoadConfig {
                mode: ours.mode(),
                ..LoadConfig::default()
            }
        );
    }
}
