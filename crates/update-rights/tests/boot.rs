//! The boot entry of a declaration (docs/design/host-updates.md, 3.5): run through the same rights
//! path as a step, a transient unit with `AmbientCapabilities=` and `CapabilityBoundingSet=` set
//! to the hook-wide `capabilities` and `NoNewPrivileges=yes`. It is not one of the five steps and
//! has no `step_capabilities` key, so no step's list is ever its own.

mod support;

use std::cell::RefCell;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use support::{SAMPLE, Scratch, sample, with, with_in};
use update_hook::{Class, Outcome, Step};
use update_rights::launch::FIXED_PROPERTIES;
use update_rights::{
    Declaration, Declared, Finished, LaunchError, Refusal, RunError, Spawner, SystemdRun,
    boot_command, boot_properties, boot_unit_name, properties, run_boot,
};

const RELEASE: &str = "/var/lib/updater/releases/tierstore/abc";
/// The uid of a hook account that is not root.
const ORDINARY: u32 = 65534;

/// A spawner that answers every run the same way and keeps what it was asked to run.
struct Fake {
    seen: RefCell<Vec<SystemdRun>>,
    answer: Result<(Option<i32>, &'static str), io::ErrorKind>,
}

impl Fake {
    fn answering(code: Option<i32>, stdout: &'static str) -> Fake {
        Fake {
            seen: RefCell::default(),
            answer: Ok((code, stdout)),
        }
    }
}

impl Spawner for Fake {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished> {
        self.seen.borrow_mut().push(command.clone());
        match self.answer {
            Ok((code, stdout)) => Ok(Finished {
                code,
                stdout: stdout.as_bytes().to_vec(),
                stderr: b"to the log\n".to_vec(),
            }),
            Err(kind) => Err(io::Error::from(kind)),
        }
    }
}

fn declared(d: &Declaration) -> Declared {
    support::declared(d.clone(), ORDINARY, &[d.name.as_str()])
}

/// The design's tierstore shape: `CAP_NET_ADMIN` hook-wide and every step's entry an empty list,
/// so a boot entry that looked its capabilities up through any step would hold none.
fn hook_wide_net_admin_and_empty_steps() -> Declaration {
    let text = SAMPLE.replace("= [\"CAP_NET_ADMIN\"]", "= []");
    let text = with_in(
        &text,
        "capabilities = []",
        "capabilities = [\"CAP_NET_ADMIN\"]",
    );
    let d = Declaration::parse(&text).expect("a declaration");
    for step in Step::ALL {
        assert!(d.hook.capabilities_for(step).is_empty(), "{step}");
    }
    d
}

fn args(command: &SystemdRun) -> Vec<String> {
    command
        .args
        .iter()
        .map(|a| a.to_str().unwrap().to_owned())
        .collect()
}

fn has(properties: &[String], property: &str) -> bool {
    properties.iter().any(|p| p == property)
}

/// Catches a boot entry that drops the hook-wide set, or that takes its capabilities from a
/// step's entry: with `CAP_NET_ADMIN` hook-wide and every step's entry empty, the boot entry holds
/// exactly `CAP_NET_ADMIN` (ambient and bounding) under `NoNewPrivileges=yes`, and each step holds
/// none.
#[test]
fn the_boot_entry_holds_exactly_the_hook_wide_capabilities() {
    let d = hook_wide_net_admin_and_empty_steps();
    let boot = boot_properties(&d);
    assert!(has(&boot, "AmbientCapabilities=CAP_NET_ADMIN"), "{boot:?}");
    assert!(
        has(&boot, "CapabilityBoundingSet=CAP_NET_ADMIN"),
        "{boot:?}"
    );
    assert!(has(&boot, "NoNewPrivileges=yes"), "{boot:?}");
    for step in Step::ALL {
        let step_properties = properties(&d, step);
        assert!(has(&step_properties, "AmbientCapabilities="), "{step}");
        assert!(has(&step_properties, "CapabilityBoundingSet="), "{step}");
    }
}

/// Catches a boot entry that takes a step's list when the hook-wide one is empty: the sample holds
/// `CAP_NET_ADMIN` in `prepare`, `cutover` and `rollback` only, and its boot entry holds nothing.
#[test]
fn the_boot_entry_of_a_declaration_without_capabilities_holds_none() {
    let d = sample();
    assert!(d.hook.capabilities.is_empty());
    assert!(!d.hook.capabilities_for(Step::Prepare).is_empty());
    let boot = boot_properties(&d);
    assert!(has(&boot, "AmbientCapabilities="), "{boot:?}");
    assert!(has(&boot, "CapabilityBoundingSet="), "{boot:?}");
    assert!(has(&boot, "NoNewPrivileges=yes"), "{boot:?}");
}

/// Catches the boot entry's command leaving the rights path of a step: the whole command is the
/// step's shape, in its own unit, with the boot word after the declared arguments, no artifact
/// and no params file, every other property as a step has it and the `rollback` bound.
#[test]
fn the_boot_command_is_exactly_this() {
    let d = Declaration::parse(&with_in(
        &hook_wide_net_admin_and_empty_steps_text(),
        "rollback = 600",
        "rollback = 500",
    ))
    .unwrap();
    let command = boot_command(&declared(&d), 7, Path::new(RELEASE))
        .unwrap()
        .expect("the declaration names a boot entry");
    let mut expected: Vec<String> = [
        "--system",
        "--quiet",
        "--wait",
        "--collect",
        "--pipe",
        "--unit=updater-tierstore-boot-7",
    ]
    .map(str::to_owned)
    .to_vec();
    let mut properties: Vec<String> = [
        "User=tierstore-update",
        "Group=tierstore-update",
        "AmbientCapabilities=CAP_NET_ADMIN",
        "CapabilityBoundingSet=CAP_NET_ADMIN",
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=yes",
        "PrivateTmp=yes",
        "ReadOnlyPaths=/etc/tierstore /etc/updater.d/tierstore.toml /usr/libexec/kbf /usr/sbin/nft",
        "ReadWritePaths=/var/lib/updater/hooks/tierstore",
        "RestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK AF_UNIX",
        "RuntimeMaxSec=500",
    ]
    .map(str::to_owned)
    .to_vec();
    properties.extend(FIXED_PROPERTIES.iter().map(|p| (*p).to_owned()));
    expected.extend(properties.iter().map(|p| format!("--property={p}")));
    expected.extend(
        [
            "--",
            "/var/lib/updater/releases/tierstore/abc/tierstore",
            "update-hook",
            "restore",
        ]
        .map(str::to_owned),
    );
    assert_eq!(args(&command), expected);
    assert_eq!(boot_unit_name("tierstore", 7), "updater-tierstore-boot-7");
}

fn hook_wide_net_admin_and_empty_steps_text() -> String {
    let text = SAMPLE.replace("= [\"CAP_NET_ADMIN\"]", "= []");
    with_in(
        &text,
        "capabilities = []",
        "capabilities = [\"CAP_NET_ADMIN\"]",
    )
}

/// Catches a boot entry run for a declaration that names none: there is no command and nothing is
/// spawned.
#[test]
fn a_declaration_without_a_boot_entry_runs_nothing_at_boot() {
    let d = Declaration::parse(&with("boot = \"restore\"\n", "")).unwrap();
    assert_eq!(d.boot, None);
    let declared = declared(&d);
    assert_eq!(
        boot_command(&declared, 1, Path::new(RELEASE)).unwrap(),
        None
    );
    let fake = Fake::answering(Some(0), "");
    let ran = run_boot(&fake, &declared, 1, Path::new(RELEASE)).unwrap();
    assert!(ran.is_none());
    assert!(fake.seen.borrow().is_empty());
}

/// Catches the boot entry being read otherwise than a step: the exit status is the outcome, the
/// last line is the report, the unit is the boot entry's, and the command run is the one the pure
/// function builds.
#[test]
fn the_boot_entry_is_read_as_a_step_is() {
    let d = hook_wide_net_admin_and_empty_steps();
    let declared = declared(&d);
    let stdout = "restoring\n{\"class\":\"environment\",\"reason\":\"no_slot\"}\n";
    for (code, outcome) in [
        (Some(0), Outcome::Success),
        (Some(1), Outcome::Failure),
        (Some(76), Outcome::Halt),
        (None, Outcome::Failure),
    ] {
        let fake = Fake::answering(code, stdout);
        let run = run_boot(&fake, &declared, 3, Path::new(RELEASE))
            .unwrap()
            .expect("a boot entry ran");
        assert_eq!(run.outcome, outcome, "{code:?}");
        assert_eq!(run.unit, "updater-tierstore-boot-3");
        assert_eq!(run.report.class, Class::Environment);
        assert_eq!(run.report.reason.as_deref(), Some("no_slot"));
        assert_eq!(run.stderr, "to the log\n");
        let built = boot_command(&declared, 3, Path::new(RELEASE))
            .unwrap()
            .unwrap();
        assert_eq!(*fake.seen.borrow(), [built]);
    }
}

/// Catches a boot entry that is started when it must not be: from a release given by a relative
/// path (nothing is spawned), or when a target changed since the load (nothing is spawned), and a
/// spawner that fails is a spawn error.
#[test]
fn a_boot_entry_that_does_not_run_says_why() {
    let d = hook_wide_net_admin_and_empty_steps();
    let fake = Fake::answering(Some(0), "");
    let error = run_boot(&fake, &declared(&d), 1, Path::new("relative")).unwrap_err();
    assert!(
        matches!(error, RunError::Launch(LaunchError::RelativeRelease(_))),
        "{error}"
    );
    assert!(fake.seen.borrow().is_empty());

    let failing = Fake {
        seen: RefCell::default(),
        answer: Err(io::ErrorKind::NotFound),
    };
    let error = run_boot(&failing, &declared(&d), 1, Path::new(RELEASE)).unwrap_err();
    assert!(matches!(error, RunError::Spawn(_)), "{error}");

    let scratch = Scratch::new("boot-recheck");
    let real = scratch.dir("real", 0o755);
    let link = scratch.path().join("link");
    symlink(real, &link).unwrap();
    let text = with_in(
        &hook_wide_net_admin_and_empty_steps_text(),
        "\"/etc/tierstore\"",
        &format!("\"{}\"", link.display()),
    );
    let linked = declared(&Declaration::parse(&text).unwrap());
    run_boot(&fake, &linked, 1, Path::new(RELEASE)).expect("the targets are as loaded");
    assert_eq!(fake.seen.borrow().len(), 1);
    std::fs::remove_file(&link).unwrap();
    symlink("/etc", &link).unwrap();
    let error = run_boot(&fake, &linked, 1, Path::new(RELEASE)).unwrap_err();
    assert!(
        matches!(error, RunError::Recheck(Refusal::Reserved { .. })),
        "{error}"
    );
    assert_eq!(fake.seen.borrow().len(), 1, "nothing was spawned");
}
