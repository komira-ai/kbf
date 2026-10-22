use std::path::PathBuf;

use super::{
    apt_install, apt_update, installed_version, running_build, unit_name, units_active, APT_GET,
    DPKG_QUERY, KBF, SYSTEMCTL, SYSTEMD_RUN,
};
use crate::config::ChannelSource;
use crate::request::Version;
use crate::runner::{Cmd, LANG, PATH};

fn source() -> ChannelSource {
    ChannelSource {
        suite: "candidate".into(),
        source_list: PathBuf::from("/etc/kbf/host-install/candidate.sources"),
    }
}

fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Every command a plan builds, for the scans below.
fn every_command() -> Vec<Cmd> {
    let v = version("1.2.3-1");
    vec![
        installed_version(),
        running_build(),
        units_active(),
        apt_update(&source(), &v),
        apt_install(&source(), &v),
    ]
}

/// The exact vector of the update step: a unit around `apt-get update` that reads the one
/// source, with the other lists left alone.
#[test]
fn the_update_step_argv() {
    let cmd = apt_update(&source(), &version("1.2.3-1"));
    assert_eq!(cmd.program, SYSTEMD_RUN);
    assert_eq!(
        cmd.args,
        strings(&[
            "--wait",
            "--collect",
            "--pipe",
            "--quiet",
            "--unit=kbf-host-install-1.2.3-1",
            "--property=RuntimeMaxSec=1800",
            &format!("--setenv=PATH={PATH}"),
            &format!("--setenv=LANG={LANG}"),
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--",
            APT_GET,
            "-o",
            "Dir::Etc::SourceList=/etc/kbf/host-install/candidate.sources",
            "-o",
            "Dir::Etc::SourceParts=-",
            "-o",
            "APT::Get::List-Cleanup=0",
            "update",
        ])
    );
}

/// The exact vector of the install step: the package at the version, the suite, the same
/// source restriction, the dpkg lock wait, and the old configuration files kept.
#[test]
fn the_install_step_argv() {
    let cmd = apt_install(&source(), &version("1.2.3-1"));
    assert_eq!(cmd.program, SYSTEMD_RUN);
    assert_eq!(
        cmd.args,
        strings(&[
            "--wait",
            "--collect",
            "--pipe",
            "--quiet",
            "--unit=kbf-host-install-1.2.3-1",
            "--property=RuntimeMaxSec=1800",
            &format!("--setenv=PATH={PATH}"),
            &format!("--setenv=LANG={LANG}"),
            "--setenv=DEBIAN_FRONTEND=noninteractive",
            "--",
            APT_GET,
            "install",
            "-y",
            "--no-install-recommends",
            "-o",
            "Dpkg::Options::=--force-confold",
            "-o",
            "DPkg::Lock::Timeout=300",
            "-o",
            "Dir::Etc::SourceList=/etc/kbf/host-install/candidate.sources",
            "-o",
            "Dir::Etc::SourceParts=-",
            "-t",
            "candidate",
            "kbf=1.2.3-1",
        ])
    );
}

/// The read-only commands.
#[test]
fn the_query_commands() {
    assert_eq!(
        installed_version(),
        Cmd {
            program: DPKG_QUERY.into(),
            args: strings(&["--show", "--showformat=${db:Status-Abbrev} ${Version}\\n", "kbf"]),
        }
    );
    assert_eq!(running_build(), Cmd { program: KBF.into(), args: strings(&["--version", "--json"]) });
    assert_eq!(
        units_active(),
        Cmd {
            program: SYSTEMCTL.into(),
            args: strings(&["is-active", "--quiet", "--", "kbf-host-install-*"]),
        }
    );
}

/// The unit name is made of the version and a fixed prefix; `+` and `~` are not unit
/// characters and are spelled out.
#[test]
fn unit_names_are_sanitized() {
    assert_eq!(unit_name(&version("1.2.3-1")), "kbf-host-install-1.2.3-1");
    assert_eq!(unit_name(&version("1.0~rc1+b1")), "kbf-host-install-1.0_trc1_pb1");
    assert_ne!(unit_name(&version("1.0~1")), unit_name(&version("1.0+1")));
    for text in ["1.0~rc1+b1", "9", "1.2.3-1", &format!("1{}", "9".repeat(127))] {
        let name = unit_name(&version(text));
        assert!(
            name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "{name}"
        );
    }
}

/// Catches an interpolation into a shell: no command runs a shell, none passes a "run this
/// string" flag, and no argument holds the version inside a longer shell-looking string.
/// (The names are built at run time so this file is not itself a shell invocation.)
#[test]
fn no_shell_anywhere() {
    let shells: Vec<String> = ["s", "bas", "das", "zs", "ks", "fis"]
        .iter()
        .flat_map(|s| [format!("/bin/{s}h"), format!("/usr/bin/{s}h")])
        .collect();
    let run_string = format!("-{}", 'c');
    for cmd in every_command() {
        assert!(!shells.contains(&cmd.program), "{cmd:?}");
        assert!(!cmd.program.ends_with("sh"), "{cmd:?}");
        assert!(!cmd.args.contains(&run_string), "{cmd:?}");
        for arg in &cmd.args {
            assert!(!shells.contains(arg), "{cmd:?}");
            for meta in [';', '|', '&', '`', '>', '<', '\n', '\''] {
                assert!(!arg.contains(meta), "{arg:?} in {cmd:?}");
            }
        }
    }
}

/// Every program is an absolute path, so the fixed `PATH` is not what picks the binary.
#[test]
fn programs_are_absolute() {
    for cmd in every_command() {
        assert!(cmd.program.starts_with('/'), "{cmd:?}");
    }
}

/// Catches a version that reaches an argument other than its own: it is in the unit name
/// and in `kbf=<version>` and nowhere else.
#[test]
fn the_version_is_in_two_places() {
    let cmd = apt_install(&source(), &version("7.8.9"));
    let holders: Vec<&String> = cmd.args.iter().filter(|a| a.contains("7.8.9")).collect();
    assert_eq!(holders, ["--unit=kbf-host-install-7.8.9", "kbf=7.8.9"]);
}
