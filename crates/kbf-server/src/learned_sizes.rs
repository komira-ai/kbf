//! `--learned-sizes`: whether the farm learns action sizes from earlier runs.

use std::fmt;
use std::str::FromStr;

/// What the farm does with what earlier runs of an action taught it
/// (`--learned-sizes`).
///
/// Only [`LearnedSizes::On`] changes a booking, and only one: the key floor an own-limit
/// kill leaves ([`kbf_sched::Scheduler::key_floor`]). [`LearnedSizes::Shadow`] is for
/// observing an estimate beside the booking it would replace and changes none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LearnedSizes {
    /// The farm books as it always did. Requests carry no size key, and a size key on a
    /// row restored from the state store is ignored (an operation queued while the flag
    /// was `on` keeps the memory its key floor raised it to).
    #[default]
    Off,
    /// The estimate is computed and recorded beside the booking; the booking is the same
    /// as `off`'s.
    Shadow,
    /// Requests of an action whose memory the client did not name, whose platform pins
    /// the OS, carry a size key, and a kill at the action's own limit leaves a floor
    /// under it that later digests of the same key book at least.
    On,
}

/// A value of `--learned-sizes` that is not `off`, `shadow` or `on`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not one of off, shadow, on")]
pub struct BadLearnedSizes(String);

impl FromStr for LearnedSizes {
    type Err = BadLearnedSizes;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "on" => Ok(Self::On),
            other => Err(BadLearnedSizes(other.to_owned())),
        }
    }
}

impl fmt::Display for LearnedSizes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::On => "on",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spelling that does not round-trip, one accepted that should not be, and a
    /// default other than `off`.
    #[test]
    fn the_modes_parse_print_and_default_to_off() {
        for mode in [LearnedSizes::Off, LearnedSizes::Shadow, LearnedSizes::On] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
        assert_eq!(LearnedSizes::default(), LearnedSizes::Off);
        let bad = "yes".parse::<LearnedSizes>().expect_err("not a mode");
        assert_eq!(bad.to_string(), "`yes` is not one of off, shadow, on");
    }
}
