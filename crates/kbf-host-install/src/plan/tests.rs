use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    ACTIVE_WAIT, APT_GET, DPKG_QUERY, KBF, RUNNING_BUILD_TIMEOUT_SECS, SYSTEMCTL, SYSTEMD_RUN,
    UNIT_ENV, Wait, apt_install, apt_update, daemon_reload, installed_version, restart_unit,
    restart_unit_no_block, running_build, unit_active, unit_install, unit_name, units_running,
};
use crate::config::ChannelSource;
use crate::request::{Channel, Unit, Version};
use crate::runner::Cmd;

const SELF_EXE: &str = "/usr/libexec/kbf/kbf-host-install";

fn source() -> ChannelSource {
    ChannelSource {
        suite: "candidate".into(),
        source_list: PathBuf::from("/etc/kbf/host-install/candidate.sources"),
    }
}

fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

fn channel() -> Channel {
    Channel::new("candidate").unwrap()
}

fn unit(text: &str) -> Cmd {
    unit_install(Path::new(SELF_EXE), &version(text), &channel())
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

fn frontend(name: &str) -> Unit {
    Unit::frontend(name).unwrap()
}

/// Every command a plan builds, for the scans below.
fn every_command() -> Vec<Cmd> {
    let v = version("1.2.3-1");
    vec![
        installed_version(),
        running_build(),
        units_running(),
        apt_update(&source()),
        apt_install(&source(), &v),
        unit(v.as_str()),
        restart_unit(&frontend("-")),
        restart_unit(&frontend("a1")),
        unit_active(&frontend("-")),
        unit_active(&frontend("a1")),
        daemon_reload(),
        restart_unit_no_block(&frontend("a1")),
    ]
}

/// Asserts that a restart is `systemctl restart -- <unit>` with a time limit, for the plain
/// frontend unit and for an instance, and that the unit is the only argument that varies.
#[test]
fn the_restart_argv() {
    for (name, want) in [
        ("-", "kbf-frontend.service"),
        ("a1", "kbf-frontend@a1.service"),
    ] {
        assert_eq!(
            restart_unit(&frontend(name)),
            Cmd {
                program: SYSTEMCTL.into(),
                args: strings(&["restart", "--", want]),
                timeout: Some(Duration::from_secs(120)),
                ..Cmd::default()
            }
        );
    }
}

/// Asserts that a restart that does not wait is `systemctl restart --no-block -- <unit>` with a
/// short time limit (it only queues a job), and that a reload is `systemctl daemon-reload`.
#[test]
fn the_no_block_restart_and_reload_argv() {
    assert_eq!(
        restart_unit_no_block(&Unit::service("kbf-updater-agent.service").unwrap()),
        Cmd {
            program: SYSTEMCTL.into(),
            args: strings(&["restart", "--no-block", "--", "kbf-updater-agent.service"]),
            timeout: Some(Duration::from_secs(10)),
            ..Cmd::default()
        }
    );
    assert_eq!(
        daemon_reload(),
        Cmd {
            program: SYSTEMCTL.into(),
            args: strings(&["daemon-reload"]),
            timeout: Some(Duration::from_secs(120)),
            ..Cmd::default()
        }
    );
}

/// Asserts that the look at whether a unit is active is `systemctl is-active -- <unit>` with
/// a time limit shorter than the restart's, for the plain frontend unit and for an instance.
#[test]
fn the_is_active_argv() {
    for (name, want) in [
        ("-", "kbf-frontend.service"),
        ("a1", "kbf-frontend@a1.service"),
    ] {
        assert_eq!(
            unit_active(&frontend(name)),
            Cmd {
                program: SYSTEMCTL.into(),
                args: strings(&["is-active", "--", want]),
                timeout: Some(Duration::from_secs(10)),
                ..Cmd::default()
            }
        );
    }
}

/// Asserts that a real restart waits two minutes for the unit to be active and looks every
/// half second.
#[test]
fn the_wait_of_a_real_restart() {
    assert_eq!(
        ACTIVE_WAIT,
        Wait {
            timeout: Duration::from_secs(120),
            interval: Duration::from_millis(500),
        }
    );
}

/// The exact vector of the update step: `apt-get update` of the one source, with the other
/// lists left alone. It runs inside the install unit, so it is not wrapped in one.
#[test]
fn the_update_step_argv() {
    let cmd = apt_update(&source());
    assert_eq!(cmd.program, APT_GET);
    assert_eq!(cmd.timeout, None);
    assert_eq!(
        cmd.args,
        strings(&[
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
    assert_eq!(cmd.program, APT_GET);
    assert_eq!(cmd.timeout, None);
    assert_eq!(
        cmd.args,
        strings(&[
            "install",
            "-y",
            "--no-install-recommends",
            "--allow-downgrades",
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

/// The one unit of an install: this program again, with the request in the one variable
/// only the unit has, in a unit that is waited for, collected and piped back. Both apt
/// steps run inside it, so what holds the install lock is what runs them.
#[test]
fn the_install_unit_argv() {
    let cmd = unit("1.2.3-1");
    assert_eq!(cmd.program, SYSTEMD_RUN);
    assert_eq!(cmd.timeout, None);
    assert_eq!(
        cmd.args,
        strings(&[
            "--wait",
            "--collect",
            "--pipe",
            "--quiet",
            "--unit=kbf-host-install-1.2.3-1.service",
            "--property=RuntimeMaxSec=1800",
            "--setenv=KBF_HOST_INSTALL_UNIT=install kbf=1.2.3-1 from candidate",
            "--",
            SELF_EXE,
        ])
    );
    assert_eq!(UNIT_ENV, "KBF_HOST_INSTALL_UNIT");
}

/// The read-only commands.
#[test]
fn the_query_commands() {
    assert_eq!(
        installed_version(),
        Cmd {
            program: DPKG_QUERY.into(),
            args: strings(&[
                "--show",
                "--showformat=${db:Status-Abbrev} ${Version}\\n",
                "kbf"
            ]),
            ..Cmd::default()
        }
    );
    // The binary runs as root and may hang: it gets a limit; nothing else here does.
    assert_eq!(
        running_build(),
        Cmd {
            program: KBF.into(),
            args: strings(&["--version", "--json"]),
            timeout: Some(Duration::from_secs(10)),
            ..Cmd::default()
        }
    );
    assert_eq!(RUNNING_BUILD_TIMEOUT_SECS, 10);
    // `is-active` is true only for `active`: a unit that is starting is `activating`, so the
    // listing names every state that is not over.
    assert_eq!(
        units_running(),
        Cmd {
            program: SYSTEMCTL.into(),
            args: strings(&[
                "list-units",
                "--type=service",
                "--state=active,activating,deactivating,reloading",
                "--plain",
                "--no-legend",
                "--no-pager",
                "--",
                "kbf-host-install-*"
            ]),
            ..Cmd::default()
        }
    );
}

/// The unit name is made of the version and a fixed prefix; `+` and `~` are not unit
/// characters and are spelled out.
#[test]
fn unit_names_are_sanitized() {
    assert_eq!(
        unit_name(&version("1.2.3-1")),
        "kbf-host-install-1.2.3-1.service"
    );
    assert_eq!(
        unit_name(&version("1.0~rc1+b1")),
        "kbf-host-install-1.0_trc1_pb1.service"
    );
    assert_ne!(unit_name(&version("1.0~1")), unit_name(&version("1.0+1")));
    for text in [
        "1.0~rc1+b1",
        "9",
        "1.2.3-1",
        &format!("1{}", "9".repeat(127)),
    ] {
        let name = unit_name(&version(text));
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "{name}"
        );
    }
}

/// Catches an interpolation into a shell: every program is one of the five this module
/// names, the program a unit runs is this program and nothing else, no argument is the flag
/// that makes a program run a string, and no argument holds a shell metacharacter. (The
/// flag is built at run time, so this file does not itself name one.)
#[test]
fn no_shell_anywhere() {
    let programs = [DPKG_QUERY, KBF, SYSTEMCTL, SYSTEMD_RUN, APT_GET];
    let run_string = format!("-{}", 'c');
    for cmd in every_command() {
        assert!(programs.contains(&cmd.program.as_str()), "{cmd:?}");
        if cmd.program == SYSTEMD_RUN {
            let after: Vec<&String> = cmd.args.iter().skip_while(|a| *a != "--").collect();
            assert_eq!(after, ["--", SELF_EXE], "{cmd:?}");
        }
        assert!(!cmd.args.contains(&run_string), "{cmd:?}");
        for arg in &cmd.args {
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

/// Catches a version that reaches an argument other than its own: in the install step it
/// is `kbf=<version>` and nowhere else; in the unit it is the unit name and the request.
#[test]
fn the_version_is_only_where_it_belongs() {
    let cmd = apt_install(&source(), &version("7.8.9"));
    let holders: Vec<&String> = cmd.args.iter().filter(|a| a.contains("7.8.9")).collect();
    assert_eq!(holders, ["kbf=7.8.9"]);
    assert!(
        !apt_update(&source())
            .args
            .iter()
            .any(|a| a.contains("7.8.9")),
        "the update names no version"
    );
    let cmd = unit("7.8.9");
    let holders: Vec<&String> = cmd.args.iter().filter(|a| a.contains("7.8.9")).collect();
    assert_eq!(
        holders,
        [
            "--unit=kbf-host-install-7.8.9.service",
            "--setenv=KBF_HOST_INSTALL_UNIT=install kbf=7.8.9 from candidate"
        ]
    );
}

/// A version ending in a unit type suffix (`.timer`, `.mount`) must not change the unit's
/// type: the name always ends in `.service`.
#[test]
fn a_suffix_in_the_version_is_not_a_unit_type() {
    for text in ["1.0.timer", "1.0.mount", "1.0.service", "1.0.slice"] {
        let name = unit_name(&version(text));
        assert_eq!(name, format!("kbf-host-install-{text}.service"));
    }
}
