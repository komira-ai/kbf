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

/// The machine version that admits `Request::size_key` (`kbf_log::MachineVersion::V6`).
const SIZE_KEY_VERSION: u32 = 6;

/// Whether requests may be keyed under `mode` over the metadata log `meta`: `on`, and the
/// log is at machine version 6 (raised here when this process is its only voter). Says so
/// at start when `on` cannot key.
pub(crate) fn admitted<M: kbf_front::MetaLog>(mode: LearnedSizes, meta: &M) -> bool {
    if mode != LearnedSizes::On {
        return false;
    }
    let admitted = meta.admit_machine_version(SIZE_KEY_VERSION);
    if admitted {
        tracing::info!("learned sizes: on; requests of unnamed memory are keyed");
    } else {
        tracing::warn!(
            "learned sizes: on, but the metadata log is below machine version 6 and this \
             process may not raise it: no request is keyed (as off) until it is raised"
        );
    }
    admitted
}

/// [`admitted`] for a frontend with no metadata log of its own (it reaches storage
/// nodes): there is no group version to read, so `on` keys nothing there.
pub(crate) fn admitted_without_log(mode: LearnedSizes) -> bool {
    if mode == LearnedSizes::On {
        tracing::warn!(
            "learned sizes: on, but this frontend has no metadata log to read a machine \
             version from: no request is keyed (as off)"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a spelling that does not round-trip, one accepted that should not be,
    /// and a default other than `off`.
    #[test]
    fn the_modes_parse_print_and_default_to_off() {
        for mode in [LearnedSizes::Off, LearnedSizes::Shadow, LearnedSizes::On] {
            assert_eq!(mode.to_string().parse(), Ok(mode));
        }
        assert_eq!(LearnedSizes::default(), LearnedSizes::Off);
        let bad = "yes".parse::<LearnedSizes>().expect_err("not a mode");
        assert_eq!(bad.to_string(), "`yes` is not one of off, shadow, on");
    }

    /// Catches `on` keying over a log with no group version (a frontend, or an
    /// in-process log): it must not, since no binary can be told to read the field.
    /// And a mode other than `on` never raises anything.
    #[test]
    fn without_a_machine_version_nothing_is_admitted() {
        let memory = kbf_front::MemoryMetaLog::new(Default::default());
        for mode in [LearnedSizes::Off, LearnedSizes::Shadow, LearnedSizes::On] {
            assert!(!admitted(mode, &memory), "{mode:?}");
            assert!(!admitted_without_log(mode), "{mode:?}");
        }
    }
}
