//! The slot profile and the GPU, on every platform: the profile a pool user's lease runs
//! under never denies what Metal needs. Two denies take the GPU away from a sandboxed
//! process (seen on a hosted macOS runner, whose GPU is paravirtual):
//! `(deny iokit-open)` leaves no Metal device, and a `mach-lookup` deny of
//! `com.apple.MTLCompilerService` leaves the device and breaks pipeline creation.
//! `tests/slot_gpu.rs` runs a compute kernel as a pool user on a Mac, under the real
//! profile and under each of these denies; this file needs no Mac.
//!
//! [`breaks_the_gpu`] reads a profile line and says whether it is one of those denies,
//! or a broader one that includes them: a blanket `(deny default)`, `(deny iokit*)`, a
//! `mach-lookup` deny with no filter, one that names the service or a prefix of its name,
//! or one by regular expression (which is not evaluated, so never allowed).

use std::path::{Path, PathBuf};

use kbf_driver_native::network::{
    Isolation, NO_DEVICE_RULES, Network, SANDBOX_EXEC, SLOT_NO_NETWORK_PROFILE, SLOT_PROFILE,
    WeakenedProfile, slot_profile, slot_write_rules,
};

/// The Mach service the Metal compiler runs in.
const COMPILER_SERVICE: &str = "com.apple.MTLCompilerService";

/// The two denies that took the GPU away, as the profile text spells them.
const PLANTED: [&str; 2] = [
    "(deny iokit-open)",
    "(deny mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
];

/// Whether profile line `line` denies something Metal needs: one of the operations listed
/// in the match below, alone or among several in one deny. This is a list of known denies,
/// not a proof that Metal works; `tests/slot_gpu.rs` is that proof, on a Mac.
fn breaks_the_gpu(line: &str) -> bool {
    let Some(rule) = line.trim().strip_prefix("(deny ") else {
        return false;
    };
    // A deny names one or more operations before its filters: `(deny a b (filter ...))`.
    let head = rule.split('(').next().unwrap_or("");
    head.split(|c: char| c.is_whitespace() || c == ')')
        .filter(|op| !op.is_empty())
        .any(|operation| match operation {
            "default"
            | "iokit*"
            | "iokit-open"
            | "iokit-get-properties"
            | "mach*"
            | "ipc-posix-shm*"
            | "ipc-posix-shm-read*"
            | "ipc-posix-shm-read-data" => true,
            "mach-lookup" => denies_service(rule),
            _ => false,
        })
}

/// Whether the filters of a `mach-lookup` deny (`rule` is the text after `(deny `)
/// include [`COMPILER_SERVICE`]. No filter is every service; a regular expression is
/// taken to include it.
fn denies_service(rule: &str) -> bool {
    let filters = rule.trim_start_matches("mach-lookup").trim();
    if filters == ")" {
        return true;
    }
    let quoted = |after: &str| -> Option<String> {
        let at = filters.find(after)? + after.len();
        let text = &filters[at..];
        let start = text.find('"')? + 1;
        let len = text[start..].find('"')?;
        Some(text[start..start + len].to_owned())
    };
    if filters.contains("global-name-regex") || filters.contains("regex") {
        return true;
    }
    if let Some(name) = quoted("(global-name-prefix") {
        return COMPILER_SERVICE.starts_with(&name);
    }
    quoted("(global-name").is_some_and(|name| name == COMPILER_SERVICE)
}

/// The profile a slot user's lease runs under for `network`, as the driver generates
/// it (the text after `-p` of the sandbox command line), from `base`, the profile text
/// before the device and write rules are added.
fn generated(base: &str) -> String {
    let rules = slot_write_rules(Path::new("/h/slot"), Path::new("/v/f/xx/yyyy")).expect("rules");
    let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
    let (_, args) = sandbox.wrap_in(base, Path::new("/lease"), &rules, "p".into(), &[]);
    let at = args.iter().position(|a| a == "-p").expect("-p");
    args[at + 1].clone()
}

fn breaking(profile: &str) -> Vec<&str> {
    profile.lines().filter(|l| breaks_the_gpu(l)).collect()
}

/// Catches a slot profile, with or without the network, that denies what a GPU needs:
/// the generated profile of each network setting has no line that is, or includes,
/// either planted deny; it still starts from `(allow default)`, which is what leaves the
/// Metal services open; and the device rules appended to every profile, which name
/// other services, are not taken for such a deny.
#[test]
fn the_slot_profile_has_none_of_the_denies_known_to_take_the_gpu() {
    for network in [Network::On, Network::Off] {
        let profile = generated(slot_profile(network));
        assert_eq!(
            breaking(&profile),
            Vec::<&str>::new(),
            "{network:?}:\n{profile}"
        );
        assert!(
            profile.lines().any(|l| l == "(allow default)"),
            "{network:?}:\n{profile}"
        );
    }
    for rule in NO_DEVICE_RULES.lines() {
        assert!(!breaks_the_gpu(rule), "{rule}");
    }
}

/// Catches the check above passing because it cannot fail: each planted deny, appended to
/// the shipped profile text (`SLOT_PROFILE` and `SLOT_NO_NETWORK_PROFILE`), is the one
/// line the check then finds in what the profile builder emits, and so is each wider
/// form of the same denies; and the denies the profile does carry (`mach-register`,
/// `lsopen`, `file-mount`, `file-write*`, a `mach-lookup` of another service, an
/// `iokit-open` allow) are not found.
#[test]
fn a_planted_deny_is_found_in_the_profile_the_builder_emits() {
    for base in [SLOT_PROFILE, SLOT_NO_NETWORK_PROFILE] {
        for deny in PLANTED {
            let profile = generated(&WeakenedProfile::Add(deny).apply(base));
            assert_eq!(breaking(&profile), [deny], "{profile}");
        }
    }
    for deny in [
        "(deny default)",
        "(deny iokit*)",
        "(deny iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))",
        "(deny mach*)",
        "(deny file-write* iokit-open)",
        "(deny iokit-get-properties)",
        "(deny ipc-posix-shm*)",
        "(deny network* mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup)",
        "(deny mach-lookup (global-name-prefix \"com.apple.\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.MTL\"))",
        "(deny mach-lookup (global-name-regex #\"[Mm][Tt][Ll]\"))",
    ] {
        assert!(breaks_the_gpu(deny), "{deny}");
        let profile = generated(&WeakenedProfile::Add(deny).apply(SLOT_PROFILE));
        assert_eq!(breaking(&profile), [deny], "{profile}");
    }
    for fine in [
        "(deny mach-register)",
        "(deny lsopen)",
        "(deny file-mount)",
        "(deny file-write*)",
        "(deny network*)",
        "(deny file-write* network*)",
        "(deny file-write* mach-lookup (global-name \"com.apple.other\"))",
        "(allow iokit-open)",
        "(allow mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
        "(deny mach-lookup (global-name \"com.apple.CoreDevice.CoreDeviceService\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))",
        "(deny mach-lookup (global-name-prefix \"com.apple.MTLCompilerService.x\"))",
        "; (deny iokit-open)",
        "",
    ] {
        assert!(!breaks_the_gpu(fine), "{fine}");
    }
}
