//! The platforms kbf releases are built for (`docs/design/pull-updates.md`).

use std::fmt;

/// A platform a kbf release tarball is built for: the `<platform>` of
/// `<name>-<commit>-<platform>.tar.gz`. A host's platform comes from the `os` and
/// `arch` entries of its node report ([`ReleasePlatform::from_report`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReleasePlatform {
    /// `linux-x86_64`.
    LinuxX86_64,
    /// `linux-arm64`.
    LinuxArm64,
    /// `darwin-arm64`.
    DarwinArm64,
}

impl ReleasePlatform {
    /// Every platform, in the order [`Ord`] gives.
    pub const ALL: [Self; 3] = [Self::LinuxX86_64, Self::LinuxArm64, Self::DarwinArm64];

    /// The platform of a node that reports `os` (`linux` or `macos`) and `arch`
    /// (`x86_64` or `arm64`); `None` for any other pair, including `macos` on `x86_64`,
    /// for which no release is built.
    #[must_use]
    pub fn from_report(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxX86_64),
            ("linux", "arm64") => Some(Self::LinuxArm64),
            ("macos", "arm64") => Some(Self::DarwinArm64),
            _ => None,
        }
    }

    /// The name in file names and on the wire: `linux-x86_64`, `linux-arm64` or
    /// `darwin-arm64`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-arm64",
            Self::DarwinArm64 => "darwin-arm64",
        }
    }

    /// The tarballs a host of this platform needs for a release: `kbf` on every platform
    /// (`docs/design/pull-updates.md` section 5). A Mac runs `kbf --mode=daemon` from its
    /// release, and no darwin `kbf-daemon` is built (`docs/artifacts.md`). The release store
    /// calls a platform complete when it holds each of these with its attestation.
    #[must_use]
    pub const fn components(self) -> &'static [&'static str] {
        match self {
            Self::LinuxX86_64 | Self::LinuxArm64 | Self::DarwinArm64 => &["kbf"],
        }
    }

    /// The platform [`ReleasePlatform::as_str`] names `name` for; `None` for any other
    /// text (the match is exact: no case folding, no trimming).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

impl fmt::Display for ReleasePlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a pair mapped to another platform, `macos` on `x86_64` or an arch spelled
    /// `aarch64` accepted, an `os` of another case accepted, and any pair outside the table
    /// mapped to something. Every `os` x `arch` pair below is listed, so a new platform
    /// added to the match but not here changes a row.
    #[test]
    fn a_node_report_maps_to_its_platform() {
        let table = [
            ("linux", "x86_64", Some(ReleasePlatform::LinuxX86_64)),
            ("linux", "arm64", Some(ReleasePlatform::LinuxArm64)),
            ("linux", "aarch64", None),
            ("linux", "", None),
            ("macos", "arm64", Some(ReleasePlatform::DarwinArm64)),
            ("macos", "x86_64", None),
            ("macos", "aarch64", None),
            ("macos", "", None),
            ("darwin", "arm64", None),
            ("Linux", "x86_64", None),
            ("windows", "x86_64", None),
            ("", "x86_64", None),
            ("", "", None),
        ];
        for (os, arch, want) in table {
            assert_eq!(ReleasePlatform::from_report(os, arch), want, "{os}/{arch}");
        }
    }

    /// Catches: a name that does not read back as its platform, a platform listed twice or
    /// missing from `ALL`, text that is not exactly a name accepted, and a `Display` that
    /// differs from the name.
    #[test]
    fn names_round_trip() {
        for platform in ReleasePlatform::ALL {
            assert_eq!(ReleasePlatform::parse(platform.as_str()), Some(platform));
            assert_eq!(platform.to_string(), platform.as_str());
        }
        let names: Vec<_> = ReleasePlatform::ALL.map(ReleasePlatform::as_str).to_vec();
        assert_eq!(names, ["linux-x86_64", "linux-arm64", "darwin-arm64"]);
        for bad in [
            "",
            "linux",
            "Linux-x86_64",
            "linux-x86_64 ",
            "darwin-x86_64",
            "macos-arm64",
        ] {
            assert_eq!(ReleasePlatform::parse(bad), None, "{bad:?}");
        }
    }

    /// Catches: a host asked for a tarball it does not run (the Mac was asked for a
    /// `kbf-daemon` that is not built for darwin, so no Mac release was ever complete or
    /// taken), and a platform with no component at all. The cross-crate pins are
    /// `kbf-installer` (`the_components_of_each_platform_are_the_binary_its_host_runs`,
    /// `an_order_naming_the_components_the_store_needs_passes_the_name_check`) and `kbf-it`
    /// (`every_component_a_platform_needs_is_in_its_release_set`).
    #[test]
    fn each_platform_needs_the_tarballs_of_its_host() {
        assert_eq!(ReleasePlatform::LinuxX86_64.components(), ["kbf"]);
        assert_eq!(ReleasePlatform::LinuxArm64.components(), ["kbf"]);
        assert_eq!(ReleasePlatform::DarwinArm64.components(), ["kbf"]);
        for platform in ReleasePlatform::ALL {
            assert!(!platform.components().is_empty(), "{platform}");
        }
    }
}
