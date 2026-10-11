//! [`AptSnapshot`] over `kbf_host::testkit::FakeExec`, which records each step and
//! changes only an in-memory host, and [`FakeApplier`] itself. Nothing here runs apt or
//! writes outside a scratch directory.

use kbf_host::testkit::{FakeExec, RUNNER};
use kbf_host::{Entry, Owner, Step};

use super::*;
use crate::set::AptPin;
use crate::testkit;

const BIN: &str = "/usr/local/libexec/kbf";

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

/// An applier over a fake host that runs as `euid`, logging into memory, and an empty
/// staging directory.
fn apt(name: &str, euid: u32) -> (AptSnapshot<FakeExec, Vec<u8>>, std::path::PathBuf) {
    let staged = testkit::scratch(name);
    let a = AptSnapshot {
        exec: FakeExec::new(euid),
        bin_dir: p(BIN),
        log: Vec::new(),
    };
    (a, staged)
}

fn snapshot_set() -> SoftwareSet {
    let mut s = testkit::set(5);
    s.snapshot = Some(AptPin {
        snapshot: "20261001T000000Z".into(),
        kernel: "linux-image-6.8".into(),
    });
    s
}

fn apt_step(args: &[&str]) -> Step {
    let cmd = ["--snapshot", "20261001T000000Z"]
        .iter()
        .chain(args)
        .fold(Cmd::new(p(APT_GET)), |cmd, a| cmd.arg(*a));
    Step::Run(
        cmd.env("DEBIAN_FRONTEND", "noninteractive")
            .env("PATH", APT_PATH),
    )
}

fn binary(bytes: &[u8], owner: Owner) -> Entry {
    Entry::File {
        perms: Perms::new(0o755),
        owner,
        bytes: bytes.to_vec(),
    }
}

/// Catches: apt run without the pinned snapshot (the node drifts to whatever the
/// archive serves today), the kernel package not installed, the commands out of order,
/// apt found through a search path instead of by absolute path, apt run without
/// `DEBIAN_FRONTEND=noninteractive` (a debconf prompt with no terminal fails the
/// upgrade) or without `PATH` (dpkg refuses to run), or steps run without the
/// `+ <step>` line that puts them in the journal.
#[test]
fn a_snapshot_set_runs_apt_pinned_to_its_snapshot() {
    let (mut a, staged) = apt("apt-plan", 0);
    assert_eq!(
        a.install(&snapshot_set(), &staged),
        Ok(Installed { reboot: false })
    );
    assert_eq!(
        a.exec.log,
        [
            apt_step(&["update"]),
            apt_step(&["--yes", "full-upgrade"]),
            apt_step(&["--yes", "install", "linux-image-6.8"]),
        ]
    );
    let run = "+ run /usr/bin/apt-get --snapshot 20261001T000000Z";
    let env = "(env DEBIAN_FRONTEND, PATH)";
    assert_eq!(
        String::from_utf8_lossy(&a.log),
        format!(
            "{run} update {env}\n{run} --yes full-upgrade {env}\n\
             {run} --yes install linux-image-6.8 {env}\n"
        )
    );
    a.exec.log.clear();
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: false })
    );
    assert!(
        a.exec.log.is_empty(),
        "no snapshot, no apt: {:?}",
        a.exec.log
    );
}

/// Catches: a failed apt step ignored and the next one run, or a binary installed
/// after the package upgrade failed.
#[test]
fn a_failed_apt_step_stops_the_install() {
    for fail_at in 0..3 {
        let (mut a, staged) = apt(&format!("apt-fail-{fail_at}"), 0);
        fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
        a.exec.fail_at = Some(fail_at);
        let err = a.install(&snapshot_set(), &staged).unwrap_err();
        assert!(err.contains("planted failure"), "{fail_at}: {err}");
        assert_eq!(a.exec.log.len(), fail_at + 1, "{fail_at}: {:?}", a.exec.log);
        assert!(
            a.exec.entries.is_empty(),
            "{fail_at}: a binary was installed"
        );
    }
}

/// Catches: a staged binary not installed, installed without the executable bit or
/// outside the bin directory, an unstaged (unchanged) artifact touched, or a reboot
/// asked for when none is needed (and none asked for when one is). The reboot flag is
/// read on the host the plans change, so the fake's flag is the one that counts.
#[test]
fn staged_artifacts_replace_the_installed_binaries() {
    let (mut a, staged) = apt("apt-bins", 0);
    fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
    let updater = p(&format!("{BIN}/kbf-updater"));
    a.exec
        .entries
        .insert(updater.clone(), binary(b"old updater", RUNNER));
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: false })
    );
    let daemon = p(&format!("{BIN}/kbf-daemon"));
    assert_eq!(
        a.exec.entries.get(&daemon),
        Some(&binary(b"new daemon", ROOT))
    );
    assert_eq!(
        a.exec.entries.get(&updater),
        Some(&binary(b"old updater", RUNNER))
    );
    assert_eq!(a.exec.entries.len(), 2, "{:?}", a.exec.entries.keys());

    fs::write(staged.join("kbf-daemon"), b"newer daemon").unwrap();
    a.exec
        .entries
        .insert(p(REBOOT_REQUIRED), binary(b"", RUNNER));
    a.exec.log.clear();
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: true })
    );
    assert_eq!(
        a.exec.entries.get(&daemon),
        Some(&binary(b"newer daemon", ROOT))
    );
    assert_eq!(a.reboot(), Ok(()));
    let reboot = Step::Run(Cmd::new(p(SYSTEMCTL)).arg("reboot"));
    assert_eq!(a.exec.log.last(), Some(&reboot));
}

/// Catches: an applier that writes every staged binary whether or not it is already in
/// place, so the install the updater repeats after a crash mid-apply is not a no-op.
#[test]
fn a_repeated_install_rewrites_nothing_in_place() {
    let (mut a, staged) = apt("apt-again", 0);
    fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: false })
    );
    assert_eq!(a.exec.log.len(), 1, "{:?}", a.exec.log);
    a.exec.log.clear();
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: false })
    );
    assert!(a.exec.log.is_empty(), "rewrote: {:?}", a.exec.log);
}

/// Catches: an applier that changes the node without `kbf_host::execute`'s guards, so
/// it would install as a user other than root or from a binary someone else can
/// replace (and so run their code as root). Neither installs nor reboots anything.
#[test]
fn the_install_applies_only_as_root_from_a_trusted_binary() {
    let staged = testkit::scratch("apt-guard");
    let mut a = AptSnapshot::new(FakeExec::new(990), p(BIN));
    fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
    let err = a.install(&snapshot_set(), &staged).unwrap_err();
    assert!(err.contains("effective uid is 990"), "{err}");
    assert!(a.reboot().unwrap_err().contains("effective uid is 990"));
    a.exec.euid = 0;
    a.exec.untrusted = true;
    let err = a.install(&snapshot_set(), &staged).unwrap_err();
    assert!(err.contains("root does not own alone"), "{err}");
    assert!(a.reboot().is_err());
    assert!(a.exec.log.is_empty(), "{:?}", a.exec.log);
    assert!(a.exec.entries.is_empty());
}

/// Catches: a staged artifact that cannot be read skipped as if the set did not change
/// it, so the set is recorded installed with the old binary in place.
#[test]
fn an_unreadable_staged_artifact_fails_the_install() {
    let (mut a, staged) = apt("apt-unreadable", 0);
    fs::create_dir(staged.join("kbf-daemon")).unwrap();
    let err = a.install(&testkit::set(5), &staged).unwrap_err();
    assert!(err.starts_with("install kbf-daemon: "), "{err}");
    assert!(a.exec.log.is_empty(), "{:?}", a.exec.log);
}

/// Catches: the fake installing while told to fail, or losing the staged names.
#[test]
fn the_fake_records_installs_and_reboots() {
    let dir = testkit::scratch("fake");
    fs::write(dir.join("b"), b"").unwrap();
    fs::write(dir.join("a"), b"").unwrap();
    let mut f = FakeApplier {
        needs_reboot: true,
        ..FakeApplier::default()
    };
    assert_eq!(
        f.install(&testkit::set(5), &dir),
        Ok(Installed { reboot: true })
    );
    assert_eq!(f.installs, [(5, vec!["a".to_owned(), "b".to_owned()])]);
    assert_eq!(f.reboot(), Ok(()));
    assert_eq!(f.reboots, 1);
    assert!(f.install(&testkit::set(5), &dir.join("missing")).is_err());
    f.fail_install = true;
    assert_eq!(
        f.install(&testkit::set(5), &dir),
        Err("planned failure".into())
    );
}

/// Catches: an installed binary left in place because its bytes and mode match while
/// root does not own it (the skip check ignoring the owner). Its owner can replace it
/// with their own code, which root then runs; and for the updater's own binary the
/// trust check refuses every later apply. The install must write the staged copy as
/// root's, over the one uid 1000 owns.
#[test]
fn a_right_binary_that_root_does_not_own_is_replaced() {
    let (mut a, staged) = apt("apt-owner", 0);
    fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
    let daemon = p(&format!("{BIN}/kbf-daemon"));
    let user = Owner {
        uid: 1000,
        gid: 1000,
    };
    a.exec
        .entries
        .insert(daemon.clone(), binary(b"new daemon", user));
    assert_eq!(
        a.install(&testkit::set(5), &staged),
        Ok(Installed { reboot: false })
    );
    assert_eq!(
        a.exec.entries.get(&daemon),
        Some(&binary(b"new daemon", ROOT))
    );
    assert_eq!(a.exec.log.len(), 1, "{:?}", a.exec.log);
}
