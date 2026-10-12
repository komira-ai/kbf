//! `sign-darwin` and `darwin-asset`: the macOS binary signed ad hoc with the hardened
//! runtime and the entitlements the allow list gives it ([`crate::entitlements`]), and
//! the same checks made again on the binary taken back out of its release tarball
//! (docs/artifacts.md).
//!
//! `sign-darwin <binary> --minos <version> [--summary <file>]` signs `<binary>` in place
//! and fails unless
//!
//! 1. `codesign --verify --strict` accepts the signature;
//! 2. the signed binary runs: `<binary> --version` exits 0 with an environment of only
//!    `PATH` and `HOME`, so no `DYLD_*` variable is set and only the signature decides
//!    whether it starts. (On GitHub's hosted runners process code-signing enforcement
//!    is off, so a byte changed after signing does not stop the binary there;
//!    `darwin-asset` verifies the signature of the bytes that ship);
//! 3. the code directory's flags are exactly `adhoc,runtime`, and the signature carries
//!    exactly the entitlements the list gives the binary's file name: none for a binary
//!    it does not list, so no `get-task-allow` (a debugger cannot attach) on any binary;
//! 4. the binary's minimum macOS (`vtool -show-build`, `minos`) is `<version>`;
//! 5. where System Integrity Protection is enabled, the hardened runtime is in force,
//!    seen in behaviour: dyld refuses to start the binary as the linker signed it when
//!    `DYLD_INSERT_LIBRARIES` names a missing library (the control, which must hold on
//!    any host), and starts the signed binary, which ignores `DYLD_*` variables.
//!    GitHub's hosted macOS runners have SIP disabled, and there dyld loads the
//!    variable into the hardened binary too; so on them this check only prints a
//!    warning, and the runtime flag is checked by 3, not by behaviour.
//!
//! `darwin-asset <tarball> <name>` reads the binary `<name>` out of its tarball, which
//! must be exactly the one entry `package` writes ([`crate::package::check_tarball`]),
//! and fails unless 1, 3 and a run of `--version` as in 2 pass for it. 1 is the check
//! that catches a modified binary on the hosted runners: a binary with a byte flipped
//! in its executed code after signing still ran there.
//!
//! Both run `codesign`, `vtool` and `csrutil` from macOS, so on any other host they
//! fail at once. The parsers of those tools' output below are plain functions, tested
//! on every host.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::{asset, check_entitlements, minos_of, sign};

/// The error `sign-darwin` and `darwin-asset` give on a host that is not macOS.
pub fn needs_macos(verb: &str) -> crate::Error {
    crate::Error::Failed(format!(
        "{verb} runs on macOS only: it needs codesign, vtool and csrutil"
    ))
}

/// The code directory's flags in `codesign --display --verbose=4` output: the names in
/// the parentheses after `flags=0x<hex>` on the `CodeDirectory` line (`adhoc,runtime`).
pub fn code_directory_flags(info: &str) -> Option<&str> {
    info.lines().find_map(|line| {
        let rest = line.strip_prefix("CodeDirectory ")?;
        let hex = &rest[rest.find("flags=0x")? + "flags=0x".len()..];
        let names = hex.trim_start_matches(|c: char| c.is_ascii_hexdigit());
        let names = names.strip_prefix('(')?;
        Some(&names[..names.find(')')?])
    })
}

/// The `CDHash=` value in `codesign --display --verbose=4` output.
pub fn cdhash(info: &str) -> Option<&str> {
    info.lines().find_map(|line| line.strip_prefix("CDHash="))
}

/// The `minos` value in `vtool -show-build` output.
pub fn minos(build: &str) -> Option<&str> {
    build.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("minos"))
            .then(|| words.next())
            .flatten()
    })
}

/// Whether `csrutil status` output says System Integrity Protection is enabled.
pub fn sip_enabled(status: &str) -> bool {
    status.contains("status: enabled")
}

/// The entitlements a binary is signed with, for the summary line: `none` when there
/// are none.
pub fn granted(keys: &[String]) -> String {
    if keys.is_empty() {
        return "none".to_owned();
    }
    keys.join(" ")
}

#[cfg(test)]
mod tests;
