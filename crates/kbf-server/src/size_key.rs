//! The key a kill-driven memory floor is kept under (`kbf_sched::Request::size_key`).
//!
//! `hash(instance, platform class, basis, shape)`, never the shape alone: a shape is
//! the first 8 bytes of a hash over argv and output patterns, which the client controls,
//! and excludes the instance and the platform. A floor keyed by shape would let a kill
//! in one tenant raise another tenant's floor, on purpose; a Linux kill would floor the
//! same shape on a Mac, whose memory is measured by another instrument and whose
//! actions are sized by it.
//!
//! A request is keyed only when the floor can be trusted to describe it:
//!
//! - the OS is pinned by `needs` (`Platform::of` is `-` for a request that names none,
//!   which can land on a cgroup node or a Mac);
//! - the client named no memory (a hinted action follows the ladder and its digest floor;
//!   `Evidence::memory_source` is [`BookSource::Default`]);
//! - it is an `action` without GPUs (the scheduler checks this too).

use kbf_invocation::BookSource;
use kbf_types::LeaseKind;
use sha2::{Digest as _, Sha256};

use crate::action_usage::ShapeKey;
use crate::machine::Platform;

/// The tag hashed ahead of every key: a new one for any change to what a key is.
const VERSION: &[u8] = b"kbf-size-key-v1";

/// What measures a lease's memory on a platform: the instrument the key's samples come
/// from, which is part of the key (the two never feed each other).
fn basis(platform: &Platform) -> Option<&'static str> {
    match platform.os_family {
        "linux" => Some("cgroup"),
        "macos" => Some("mac_footprint"),
        _ => None,
    }
}

/// The size key of an action of `shape` run under `instance` on `platform`, or `None`
/// when `platform` pins no OS.
#[must_use]
pub fn size_key(instance: &str, platform: &Platform, shape: ShapeKey) -> Option<u64> {
    let basis = basis(platform)?;
    let mut hash = Sha256::new();
    hash.update(VERSION);
    for part in [instance, platform.os_family, platform.isa.as_str(), basis] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.update(shape.bytes());
    let digest = hash.finalize();
    Some(u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("a SHA-256 digest is 32 bytes"),
    ))
}

/// What the front door knew about a request when it keyed it.
#[derive(Clone, Copy, Debug)]
pub struct Keyable<'a> {
    /// The instance the request was made under.
    pub instance: &'a str,
    /// The platform its `needs` name.
    pub platform: &'a Platform,
    /// Its shape.
    pub shape: ShapeKey,
    /// The lease kind it books.
    pub kind: LeaseKind,
    /// The GPUs it books.
    pub gpus: u64,
    /// Where its memory booking came from.
    pub memory: BookSource,
}

impl Keyable<'_> {
    /// The request's size key, or `None` when it must not be keyed.
    #[must_use]
    pub fn key(&self) -> Option<u64> {
        let keyed =
            self.kind == LeaseKind::Action && self.gpus == 0 && self.memory == BookSource::Default;
        if keyed {
            size_key(self.instance, self.platform, self.shape)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> Platform {
        Platform {
            os_family: "linux",
            isa: "x86_64".to_owned(),
        }
    }

    const SHAPE: ShapeKey = ShapeKey::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]);

    /// Catches a key that changes under a committed store: the hash pinned for one
    /// input. A new value means every key floor in a store is forgotten, and must be a
    /// new tag.
    #[test]
    fn the_key_of_an_input_is_pinned() {
        assert_eq!(
            size_key("main", &linux(), SHAPE),
            Some(9_002_056_458_043_364_908)
        );
    }

    /// Catches a key blind to one of its parts: each part changed alone moves it; a
    /// part that moves into the next (`ab` + `c` against `a` + `bc`) does not alias.
    ///
    #[test]
    fn every_part_of_the_key_counts() {
        let base = size_key("main", &linux(), SHAPE);
        let arm = Platform {
            isa: "arm64".to_owned(),
            ..linux()
        };
        let mac = Platform {
            os_family: "macos",
            ..linux()
        };
        let other = ShapeKey::from_bytes([9; 8]);
        for changed in [
            size_key("other", &linux(), SHAPE),
            size_key("main", &arm, SHAPE),
            size_key("main", &mac, SHAPE),
            size_key("main", &linux(), other),
        ] {
            assert!(changed.is_some());
            assert_ne!(changed, base);
        }
        let glued = |instance: &str, isa: &str| {
            let platform = Platform {
                os_family: "linux",
                isa: isa.to_owned(),
            };
            size_key(instance, &platform, SHAPE)
        };
        assert_ne!(glued("ab", "c"), glued("a", "bc"));
    }

    /// Catches a key for a platform that pins no OS (`-`, or a name no daemon runs).
    #[test]
    fn a_platform_with_no_os_has_no_key() {
        for os_family in ["-", "windows"] {
            let platform = Platform {
                os_family,
                isa: "x86_64".to_owned(),
            };
            assert_eq!(size_key("main", &platform, SHAPE), None, "{os_family}");
        }
    }

    /// Catches each condition of `Keyable::key` dropped: only an `action` without GPUs
    /// whose memory the client did not name is keyed.
    #[test]
    fn only_a_plain_unnamed_action_is_keyable() {
        let platform = linux();
        let keyable = Keyable {
            instance: "main",
            platform: &platform,
            shape: SHAPE,
            kind: LeaseKind::Action,
            gpus: 0,
            memory: BookSource::Default,
        };
        assert!(keyable.key().is_some());
        for refused in [
            Keyable {
                kind: LeaseKind::WholeMachine,
                ..keyable
            },
            Keyable {
                kind: LeaseKind::Vm,
                ..keyable
            },
            Keyable { gpus: 1, ..keyable },
            Keyable {
                memory: BookSource::Hint,
                ..keyable
            },
        ] {
            assert_eq!(refused.key(), None);
        }
    }
}
