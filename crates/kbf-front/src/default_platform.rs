//! Where an action that names no OS and no architecture runs.
//!
//! An REAPI client that sends no `OSFamily` and no `ISA` (a Buck2 platform with only a
//! `pool` property, or none at all) asked for nothing, and [`kbf_caps::Request`] then
//! matches every node: the scheduler's first fit by name gave such an action to a Mac
//! whenever a Mac sorted first. [`DefaultPlatform`] names the OS and architecture
//! (`--default-platform`, `linux/x86_64` unless the operator says `none`) those actions
//! get. It is placement only: the action's digest and cache key are the client's.

use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use kbf_caps::{Arch, DAEMON_OSES, Request};

/// The OS and architecture given to an action that names neither, or none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultPlatform(Option<(&'static str, Arch)>);

/// A `--default-platform` value that is neither `none` nor `<os>/<arch>`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not `none` or `<os>/<arch>` with os one of {DAEMON_OSES:?} and arch x86_64 or arm64"
)]
pub struct BadDefaultPlatform(String);

impl DefaultPlatform {
    /// No default: an action that names no OS runs on any node that fits.
    pub const NONE: Self = Self(None);

    /// `linux/x86_64`, the flag's default.
    pub const LINUX_X86_64: Self = Self(Some(("linux", Arch::X86_64)));

    /// The OS and architecture given, if there is a default.
    #[must_use]
    pub const fn get(self) -> Option<(&'static str, Arch)> {
        self.0
    }

    /// `needs` with the default's `os` and `arch` added, or `None` if the default does
    /// not apply: there is none, or the request already says where it runs (an `os`, an
    /// `arch` or an `isa_level`), or it asks for something only a Mac reports (`xcode`,
    /// an iOS device, a VM image), which a Linux default would make unservable.
    #[must_use]
    pub fn apply(self, needs: &Request) -> Option<Request> {
        let (os, arch) = self.0?;
        let properties = needs.properties();
        let places = |key: &str| {
            matches!(key, "os" | "arch" | "isa_level" | "xcode" | "vm.image")
                || key.starts_with("ios.device")
        };
        if properties.iter().any(|(key, _)| places(key)) {
            return None;
        }
        let arch = arch.name();
        Request::parse(
            properties
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .chain([("os", os), ("arch", arch)]),
        )
        .ok()
    }
}

impl FromStr for DefaultPlatform {
    type Err = BadDefaultPlatform;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || BadDefaultPlatform(s.to_owned());
        if s == "none" {
            return Ok(Self::NONE);
        }
        let (os, arch) = s.split_once('/').ok_or_else(bad)?;
        let os = DAEMON_OSES.into_iter().find(|o| *o == os).ok_or_else(bad)?;
        let arch = arch.parse().map_err(|_| bad())?;
        Ok(Self(Some((os, arch))))
    }
}

impl fmt::Display for DefaultPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some((os, arch)) => write!(f, "{os}/{arch}"),
            None => f.write_str("none"),
        }
    }
}

/// How often at most the application of the default is logged.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Lets one log line through per [`LOG_EVERY`], and counts the ones it held back.
#[derive(Debug, Default)]
pub(crate) struct Quiet(Mutex<(Option<Instant>, u64)>);

impl Quiet {
    /// `Some(held back since the last line)` if a line may be logged now.
    pub(crate) fn turn(&self) -> Option<u64> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if state
            .0
            .is_some_and(|last| now.duration_since(last) < LOG_EVERY)
        {
            state.1 += 1;
            return None;
        }
        state.0 = Some(now);
        Some(std::mem::take(&mut state.1))
    }
}
