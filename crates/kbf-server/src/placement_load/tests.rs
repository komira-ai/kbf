//! Unit tests of `--placement-load` (`super`).

use clap::error::ErrorKind;
use clap::{Parser, ValueEnum};
use kbf_sched::LoadConfig;

use super::PlacementLoad;
use crate::Args;

/// Catches a default other than `off` (a server started with no flag would place by load
/// while its daemons send none), a value not parsed, a value other than the two accepted
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
/// booked), and a configuration that changes a setting other than the mode and the profile
/// switches kbf writes out: kbf takes the scheduler's own values for the rest.
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
                cpu_floor_permille: 1000,
                profile: true,
                profile_memory: true,
                profile_floor_permille: 750,
                expected_life_delta_minutes: Some(2),
                flat_memory_slack_permille: Some(0),
                ..LoadConfig::default()
            }
        );
    }
}

/// Pins the profile switches the farm gives every scheduler, as numbers and not as the
/// scheduler library's defaults: a raise of the library's revision that changes one of its
/// defaults leaves the farm's placement as it is, and a change of these values is a change
/// of kbf's. The values are the library's today and have not been measured on a farm.
#[test]
fn the_profile_switches_are_written_out_and_pinned() {
    for mode in PlacementLoad::value_variants() {
        let config = mode.config();
        assert_eq!(config.cpu_floor_permille, 1000, "{mode:?}");
        assert!(config.profile, "{mode:?}");
        assert!(config.profile_memory, "{mode:?}");
        assert_eq!(config.profile_floor_permille, 750, "{mode:?}");
        assert_eq!(config.expected_life_delta_minutes, Some(2), "{mode:?}");
        assert_eq!(config.flat_memory_slack_permille, Some(0), "{mode:?}");
    }
    assert_eq!(super::CPU_FLOOR_PERMILLE, 1000);
    assert_eq!(super::PROFILE_FLOOR_PERMILLE, 750);
    assert_eq!(super::EXPECTED_LIFE_DELTA_MINUTES, 2);
    assert_eq!(super::FLAT_MEMORY_SLACK_PERMILLE, 0);
}

/// Catches `off` carried as `Some(false)` (a farm that does not place by load would send a
/// field it never sent), `on` carried as absent (nodes would never lift their limit), and a
/// mode added without deciding what `Welcome` says of it.
#[test]
fn welcome_carries_true_for_on_and_nothing_for_off() {
    assert_eq!(PlacementLoad::On.welcome(), Some(true));
    assert_eq!(PlacementLoad::Off.welcome(), None);
}
