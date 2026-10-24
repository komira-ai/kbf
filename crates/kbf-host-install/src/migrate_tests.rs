//! The migration over a scratch root: a host on the hand-placed layout whose unit runs the
//! node's flags, a package in a directory, and `dpkg`, `rpm` and `systemctl` played by the
//! test (the same rig as the package host's).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use kbf_installer::CommitId;
use kbf_installer::state::{Locked, State};

use super::*;
use crate::local::package_format::Format;
use crate::local::package_tests::{A, Rig, a_version, both, bytes_of, put, sha256};

const UNIT: &str = "kbf-storage-and-daemon.service";

const OLD_UNIT: &str = "[Unit]\nDescription=kbf storage and daemon\n\n[Service]\nUser=kbf\n\
Environment=RUST_LOG=info\nExecStart=@RELEASES@/current/kbf --mode=storage-and-daemon \\\n  \
--cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta\nRestart=always\n";

/// A host that runs release A by hand.
struct Mig {
    rig: Rig,
    place: Place,
    /// The directory the release files are in.
    files: PathBuf,
    original: String,
}

impl Mig {
    fn new(name: &str, format: Format) -> Self {
        let rig = Rig::new(name, format);
        rig.net.borrow_mut().installed = None;
        let layout = rig.backend.layout.clone();
        // The state first: a host with a releases directory and no state file is a damaged one.
        fs::create_dir_all(rig.root.join("run")).unwrap();
        fs::create_dir_all(rig.root.join("var/lib/kbf-installer")).unwrap();
        Locked::acquire(&layout)
            .unwrap()
            .write(&State {
                current: Some(CommitId::new(A).unwrap()),
                ..State::default()
            })
            .unwrap();
        let releases = rig.root.join("var/lib/kbf-installer/releases");
        fs::create_dir_all(releases.join(A)).unwrap();
        fs::write(releases.join(A).join("kbf"), b"old binary").unwrap();
        std::os::unix::fs::symlink(A, releases.join("current")).unwrap();

        let place = Place {
            systemd_dir: rig.dir.join("systemd"),
            proc_dir: rig.dir.join("proc"),
            kbf: rig.dir.join("usr-bin-kbf"),
        };
        fs::create_dir(&place.systemd_dir).unwrap();
        fs::set_permissions(&place.systemd_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let original = OLD_UNIT.replace("@RELEASES@", &releases.display().to_string());
        put(&place.systemd_dir.join(UNIT), &original);
        fs::create_dir_all(place.proc_dir.join("4242")).unwrap();
        std::os::unix::fs::symlink(&place.kbf, place.proc_dir.join("4242/exe")).unwrap();

        // The release files of A, as `gh release download` leaves them.
        let files = rig.dir.join("release");
        fs::create_dir(&files).unwrap();
        let tag = format!("v-{}", &A[..8]);
        for (name, bytes) in rig.net.borrow().assets[&tag].clone() {
            fs::write(files.join(name), bytes).unwrap();
        }
        {
            let mut net = rig.net.borrow_mut();
            net.kbf_commit = Some(A.to_owned());
            net.watch = Some(releases);
        }
        Self {
            rig,
            place,
            files,
            original,
        }
    }

    fn config(&self, apply: bool) -> Config {
        Config {
            format: self.rig.format,
            channel: crate::local::package_tests::stable(),
            unit: Unit::service(UNIT).unwrap(),
            dir: self.files.clone(),
            apply,
        }
    }

    fn run(&self, apply: bool) -> (u8, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(
            &self.config(apply),
            &self.rig.backend,
            &self.place,
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn releases(&self) -> PathBuf {
        self.rig.root.join("var/lib/kbf-installer/releases")
    }

    fn unit_file(&self) -> PathBuf {
        self.place.systemd_dir.join(UNIT)
    }

    fn drop_in(&self) -> PathBuf {
        self.place
            .systemd_dir
            .join(format!("{UNIT}.d"))
            .join(DROP_IN)
    }

    /// The release files of A are those of another build in this one.
    fn replace_release_file(&self, name_part: &str, bytes: &[u8]) {
        for entry in fs::read_dir(&self.files).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(name_part)
            {
                fs::write(path, bytes).unwrap();
            }
        }
    }

    /// The host is as it was: the unit, the old layout, no package, no drop-in, nothing run.
    fn assert_untouched(&self) {
        assert_eq!(fs::read_to_string(self.unit_file()).unwrap(), self.original);
        assert!(!self.drop_in().exists());
        assert!(self.releases().join(A).join("kbf").exists());
        assert_eq!(self.rig.installed(), None);
        let events = self.rig.events();
        assert!(
            !events
                .iter()
                .any(|e| e.starts_with("install:") || e.starts_with("restart:") || e == "reload"),
            "{events:?}"
        );
        assert!(self.rig.work().is_empty(), "{:?}", self.rig.work());
    }
}

/// What the drop-in is for the unit above.
const DROP_IN_TEXT: &str = "# Written by kbf-host-install --migrate-to-package: the flags this node's hand-placed\n\
# unit gave kbf, now on the kbf package's unit and its /usr/bin/kbf.\n\
[Unit]\nConditionPathExists=\n\n[Service]\nUser=kbf\nEnvironment=RUST_LOG=info\n\
ExecStart=\nExecStart=/usr/bin/kbf --mode=storage-and-daemon --cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta\n\
ExecStartPre=\nExecStartPre=+/usr/bin/kbf boot-check --mode=storage-and-daemon\n\
ExecStartPre=/usr/bin/kbf --mode=storage-and-daemon --cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta --check\n";

/// The dry run (the default) prints every step, the drop-in with the node's flags among them,
/// and changes nothing: not the unit, not the old layout, not the package database, and it
/// leaves no scratch.
#[test]
fn the_dry_run_prints_the_plan_and_changes_nothing() {
    both(|format| {
        let mig = Mig::new("mig-dry", format);
        let (code, out, err) = mig.run(false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("DRY-RUN: ")).collect();
        assert_eq!(lines.len(), 7, "{out}");
        assert!(lines[0].contains("install the verified package"));
        assert!(lines[1].contains("10-node-flags.conf"));
        assert!(lines[2].contains(".hand-placed"));
        assert!(lines[3].contains("daemon-reload"));
        assert!(lines[4].contains("restart"));
        assert!(lines[5].contains("check"));
        assert!(lines[6].contains("remove the old layout"));
        assert!(out.contains("    | ExecStart=/usr/bin/kbf --mode=storage-and-daemon --cold-store=file:///var/lib/kbf/cold"));
        assert!(out.ends_with("dry run: nothing changed. Re-run with --apply.\n"));
        mig.assert_untouched();
    });
}

/// The migration, applied: the verified package is installed, the node's flags and account are
/// on the packaged unit through a drop-in, the hand-placed unit is renamed aside, systemd
/// rereads, the unit restarts, its main process and the binary are checked, and the old layout
/// is still there when the check runs and gone after.
#[test]
fn the_migration_installs_carries_the_flags_restarts_checks_and_only_then_removes() {
    both(|format| {
        let mig = Mig::new("mig-apply", format);
        let (code, out, err) = mig.run(true);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(out.ends_with(&format!("migrated: {UNIT} runs the kbf package\n")));

        let marks: Vec<String> = mig
            .rig
            .events()
            .into_iter()
            .filter(|e| !e.starts_with("active:") && e != "download" && e != "query")
            .collect();
        assert_eq!(
            marks,
            [
                "attest".to_owned(),
                "inspect".to_owned(),
                format!(
                    "install:{}:trial=false:prev_kept=false:old={}",
                    a_version(),
                    format == Format::Rpm
                ),
                "reload".to_owned(),
                format!("restart:{UNIT}"),
                "show:old_layout=true".to_owned(),
                "kbf-version".to_owned(),
            ]
        );
        assert_eq!(mig.rig.installed(), Some(a_version()));
        assert_eq!(fs::read_to_string(mig.drop_in()).unwrap(), DROP_IN_TEXT);
        assert_eq!(
            fs::metadata(mig.drop_in()).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(!mig.unit_file().exists());
        assert_eq!(
            fs::read_to_string(mig.place.systemd_dir.join(format!("{UNIT}.hand-placed"))).unwrap(),
            mig.original
        );
        assert!(!mig.releases().exists(), "the old layout is removed");
        assert!(mig.rig.work().is_empty());
        // Only the unit migrated is restarted.
        assert_eq!(mig.rig.restarts(), [format!("restart:{UNIT}")]);
    });
}

/// A drop-in carries a flag-less unit's mode and a root unit's account; a unit that lists no
/// mode flag gets the mode of its name.
#[test]
fn the_drop_in_names_the_mode_and_the_account_the_unit_ran_as() {
    let old = unit::parse(
        "[Service]\nExecStart=/r/current/kbf --listen=0.0.0.0:8981\nGroup=kbf\n\
         WorkingDirectory=/srv\nSupplementaryGroups=a b\nEnvironmentFile=/etc/kbf/env\n",
        "/r",
    )
    .unwrap();
    let text = drop_in(&old, "daemon");
    assert!(text.contains("User=root\nGroup=kbf\nWorkingDirectory=/srv\nSupplementaryGroups=a b\nEnvironmentFile=/etc/kbf/env\n"), "{text}");
    assert!(text.contains("ExecStart=/usr/bin/kbf --mode=daemon --listen=0.0.0.0:8981\n"));
    assert!(
        text.contains("ExecStartPre=/usr/bin/kbf --mode=daemon --listen=0.0.0.0:8981 --check\n")
    );
    assert!(text.contains("boot-check --mode=daemon\n"));
}

/// A flag with `?` and `&` reaches the drop-in byte for byte, on `ExecStart=` and on the check.
#[test]
fn a_cold_store_url_with_a_query_reaches_the_drop_in_byte_for_byte() {
    let flag = "--cold-store=s3://bucket-name/prefix/?endpoint=http://127.0.0.1:30900&conditional-put=true";
    let old = unit::parse(
        &format!("[Service]\nExecStart=/r/current/kbf --mode=storage {flag}\n"),
        "/r",
    )
    .unwrap();
    let text = drop_in(&old, "storage");
    assert!(
        text.contains(&format!("ExecStart=/usr/bin/kbf --mode=storage {flag}\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "ExecStartPre=/usr/bin/kbf --mode=storage {flag} --check\n"
        )),
        "{text}"
    );
}

/// What the migration cannot carry, or finds wrong, is refused before anything changes: the
/// host is not on the hand-placed layout, a release is on trial, the unit is not one it can
/// read, or the package in the directory is not a verified package of the commit the host runs.
#[test]
fn what_cannot_be_carried_or_verified_is_refused_and_nothing_changes() {
    type Break = Box<dyn Fn(&Mig)>;
    let cases: Vec<(&str, Break, &str)> = vec![
        (
            "no-installer-state",
            Box::new(|m| {
                let dir = m.rig.root.join("var/lib/kbf-installer");
                for entry in fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("state.json")
                    {
                        fs::remove_file(path).unwrap();
                    }
                }
                fs::remove_dir_all(m.releases()).unwrap();
            }),
            "no installer state",
        ),
        (
            "no-current-release",
            Box::new(|m| {
                Locked::acquire(&m.rig.backend.layout)
                    .unwrap()
                    .write(&State::default())
                    .unwrap();
            }),
            "names no release that runs",
        ),
        (
            "a-release-on-trial",
            Box::new(|m| {
                let mut state = State {
                    current: Some(CommitId::new(A).unwrap()),
                    ..State::default()
                };
                state.pending_confirm = Some(kbf_installer::state::PendingConfirm {
                    commit: CommitId::new(&"b".repeat(40)).unwrap(),
                    previous: None,
                    deadline_wall: None,
                    deadline_mono_s: None,
                    starts_left: None,
                    activation: "x".to_owned(),
                });
                Locked::acquire(&m.rig.backend.layout)
                    .unwrap()
                    .write(&state)
                    .unwrap();
            }),
            "on trial",
        ),
        (
            "no-unit-file",
            Box::new(|m| fs::remove_file(m.unit_file()).unwrap()),
            "kbf-storage-and-daemon.service",
        ),
        (
            "a-unit-others-can-write",
            Box::new(|m| {
                fs::set_permissions(m.unit_file(), fs::Permissions::from_mode(0o666)).unwrap();
            }),
            "writable by its group or by others",
        ),
        (
            "the-unit-is-a-link",
            Box::new(|m| {
                fs::remove_file(m.unit_file()).unwrap();
                std::os::unix::fs::symlink("/etc/hostname", m.unit_file()).unwrap();
            }),
            "cannot read",
        ),
        (
            "two-exec-starts",
            Box::new(|m| {
                let two = format!("{}ExecStart=/bin/true\n", m.original);
                put(&m.unit_file(), two);
            }),
            "2 ExecStart= lines",
        ),
        (
            "the-unit-runs-another-program",
            Box::new(|m| {
                put(
                    &m.unit_file(),
                    "[Service]\nExecStart=/usr/local/bin/kbf --mode=storage-and-daemon\n",
                );
            }),
            "not the kbf of a release",
        ),
        (
            "a-flag-with-a-variable",
            Box::new(|m| {
                let text = m
                    .original
                    .replace("--meta-dir=/var/lib/kbf-meta", "--meta-dir=$DIR");
                put(&m.unit_file(), text);
            }),
            "does not carry",
        ),
        (
            "another-mode",
            Box::new(|m| {
                let text = m
                    .original
                    .replace("--mode=storage-and-daemon", "--mode=daemon");
                put(&m.unit_file(), text);
            }),
            "not the mode of",
        ),
        (
            "a-drop-in-is-there",
            Box::new(|m| {
                fs::create_dir_all(m.drop_in().parent().unwrap()).unwrap();
                fs::write(m.drop_in(), "x").unwrap();
            }),
            "is there already",
        ),
        (
            "the-unit-was-moved-aside-before",
            Box::new(|m| {
                fs::write(m.place.systemd_dir.join(format!("{UNIT}.hand-placed")), "x").unwrap();
            }),
            "is there already",
        ),
        (
            "no-package-in-the-directory",
            Box::new(|m| {
                for entry in fs::read_dir(&m.files).unwrap() {
                    let path = entry.unwrap().path();
                    if !path.to_string_lossy().ends_with("SHA256SUMS") {
                        fs::remove_file(path).unwrap();
                    }
                }
            }),
            "kbf-aaaaaaaa",
        ),
        (
            "the-attestation-fails",
            Box::new(|m| *m.rig.gh_exit.borrow_mut() = 1),
            "no matching attestation",
        ),
        (
            "the-sums-list-other-bytes",
            Box::new(|m| {
                m.replace_release_file(
                    "SHA256SUMS",
                    format!("{}  other\n", sha256(b"x")).as_bytes(),
                );
            }),
            "SHA256SUMS",
        ),
        (
            "the-package-is-another-build",
            Box::new(|m| {
                let other = bytes_of(
                    "kbf",
                    &crate::local::package_tests::new_version(),
                    m.rig.format_arch(),
                );
                let file = m.rig.file_name(A);
                fs::write(m.files.join(&file), &other).unwrap();
                let sums = format!("{}  {file}\n", sha256(&other));
                m.replace_release_file("SHA256SUMS", sums.as_bytes());
            }),
            "not the commit",
        ),
    ];
    for (name, break_it, mentions) in cases {
        both(|format| {
            let mig = Mig::new(&format!("mig-refuse-{name}"), format);
            break_it(&mig);
            for apply in [false, true] {
                let (code, out, err) = mig.run(apply);
                assert_eq!(code, 2, "{name}: {out} {err}");
                assert!(err.contains("refused:"), "{name}: {err}");
                assert!(err.contains(mentions), "{name}: {err}");
                assert!(!out.contains("+ "), "{name}: {out}");
            }
            // No drop-in was written (the one a case plants is as it was).
            assert!(!mig.drop_in().exists() || fs::read_to_string(mig.drop_in()).unwrap() == "x");
            let events = mig.rig.events();
            assert!(
                !events.iter().any(|e| e.starts_with("install:")
                    || e.starts_with("restart:")
                    || e == "reload"),
                "{name}: {events:?}"
            );
            assert_eq!(mig.rig.installed(), None);
        });
    }
}

/// Anything that goes wrong after the package is installed puts the hand-placed unit back: the
/// drop-in is gone, the unit has its name again, systemd rereads, the unit restarts, and the old
/// layout is untouched. The failure is the exit status 1 and a line that says so.
#[test]
fn a_node_that_does_not_come_up_or_does_not_check_out_gets_its_old_unit_back() {
    type Break = Box<dyn Fn(&Mig)>;
    let cases: Vec<(&str, Break, &str)> = vec![
        (
            "the-unit-does-not-restart",
            Box::new(|m| {
                m.rig
                    .net
                    .borrow_mut()
                    .restart_fails_once
                    .insert(UNIT.to_owned());
            }),
            "step 5",
        ),
        (
            "systemd-cannot-reread",
            Box::new(|m| m.rig.net.borrow_mut().reload_fails = 1),
            "step 4",
        ),
        (
            "systemd-cannot-be-asked-for-the-main-process",
            Box::new(|m| {
                m.rig.net.borrow_mut().broken.insert("no-show");
            }),
            "systemctl show",
        ),
        (
            "the-main-process-runs-the-old-binary",
            Box::new(|m| {
                let exe = m.place.proc_dir.join("4242/exe");
                fs::remove_file(&exe).unwrap();
                std::os::unix::fs::symlink("/old/release/kbf", exe).unwrap();
            }),
            "not",
        ),
        (
            "there-is-no-main-process",
            Box::new(|m| m.rig.net.borrow_mut().main_pid = "0\n".to_owned()),
            "no main process",
        ),
        (
            "the-main-process-has-no-exe",
            Box::new(|m| fs::remove_file(m.place.proc_dir.join("4242/exe")).unwrap()),
            "the main process 4242",
        ),
        (
            "the-binary-reports-another-commit",
            Box::new(|m| m.rig.net.borrow_mut().kbf_commit = Some("b".repeat(40))),
            "reports commit",
        ),
        (
            "the-binary-reports-nothing",
            Box::new(|m| m.rig.net.borrow_mut().kbf_commit = None),
            "reports no commit",
        ),
    ];
    for (name, break_it, mentions) in cases {
        both(|format| {
            let mig = Mig::new(&format!("mig-back-{name}"), format);
            break_it(&mig);
            let (code, out, err) = mig.run(true);
            assert_eq!(code, 1, "{name}: {out} {err}");
            assert!(err.contains(mentions), "{name}: {err}");
            assert!(
                err.contains("the hand-placed unit is back and running"),
                "{name}: {err}"
            );
            assert_eq!(
                fs::read_to_string(mig.unit_file()).unwrap(),
                mig.original,
                "{name}"
            );
            assert!(!mig.drop_in().exists());
            assert!(!mig.drop_in().parent().unwrap().exists());
            assert!(
                mig.releases().join(A).join("kbf").exists(),
                "{name}: the old layout stays"
            );
            let events = mig.rig.events();
            assert_eq!(
                events.iter().filter(|e| *e == "reload").count(),
                2,
                "{name}: {events:?}"
            );
            let restarts = mig.rig.restarts();
            assert!(
                !restarts.is_empty() && restarts.iter().all(|r| r == &format!("restart:{UNIT}"))
            );
        });
    }
}

/// A package that fails to install changes nothing else; and when going back fails too, the
/// operator is told, with the old layout still whole.
#[test]
fn a_failed_install_stops_at_once_and_a_failed_going_back_is_said() {
    both(|format| {
        let mig = Mig::new("mig-install-fails", format);
        mig.rig.net.borrow_mut().install_fails = 1;
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("step 1 failed"), "{err}");
        assert!(!err.contains("hand-placed unit is back"), "{err}");
        assert_eq!(fs::read_to_string(mig.unit_file()).unwrap(), mig.original);
        assert!(!mig.drop_in().exists());
        assert!(!mig.rig.events().iter().any(|e| e == "reload"));

        let mig = Mig::new("mig-no-way-back", format);
        mig.rig
            .net
            .borrow_mut()
            .restart_never
            .insert(UNIT.to_owned());
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("putting it back failed"), "{err}");
        assert!(mig.releases().join(A).join("kbf").exists());
    });
}

/// The old layout is removed last, and a removal that fails leaves a node that runs the package:
/// the unit is not put back.
#[test]
fn a_removal_that_fails_leaves_the_migrated_node_alone() {
    if rustix::process::geteuid().is_root() {
        return; // root can remove what a mode bit would protect
    }
    let mig = Mig::new("mig-remove-fails", Format::Deb);
    // A directory that cannot be emptied: it is closed to writing.
    struct Close(PathBuf);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    fs::set_permissions(mig.releases().join(A), fs::Permissions::from_mode(0o500)).unwrap();
    let _close = Close(mig.releases().join(A));
    let (code, _, err) = mig.run(true);
    assert_eq!(code, 1);
    assert!(err.contains("step 7 failed"), "{err}");
    assert!(!err.contains("hand-placed unit is back"));
    assert!(mig.drop_in().exists());
    assert!(!mig.unit_file().exists());
}

/// The flags: the layout, the channel, the unit and the directory are required, each once.
#[test]
fn the_flags_make_one_config_and_refuse_the_rest() {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let ok = Config::parse(&words(&[
        "--layout=deb",
        "--channel",
        "stable",
        "--unit",
        UNIT,
        "--dir",
        "/srv/release",
    ]))
    .unwrap();
    assert_eq!(ok.format, Format::Deb);
    assert_eq!(ok.dir, PathBuf::from("/srv/release"));
    assert!(!ok.apply);
    let applied = Config::parse(&words(&[
        "--apply",
        "--layout",
        "rpm",
        "--channel",
        "stable",
        "--unit",
        UNIT,
        "--dir",
        "/d",
    ]))
    .unwrap();
    assert!(applied.apply && applied.format == Format::Rpm);

    let base = [
        "--layout",
        "deb",
        "--channel",
        "stable",
        "--unit",
        UNIT,
        "--dir",
        "/d",
    ];
    for (extra, why) in [
        (&["--layout", "rpm"][..], "given twice"),
        (&["--bogus", "1"], "unknown argument"),
        (&["--apply=yes"], "unknown argument"),
    ] {
        let mut all = base.to_vec();
        all.extend_from_slice(extra);
        assert!(
            Config::parse(&words(&all)).unwrap_err().contains(why),
            "{extra:?}"
        );
    }
    for (missing, why) in [
        ("--layout", "--layout is required"),
        ("--channel", "--channel is required"),
        ("--unit", "--unit is required"),
        ("--dir", "--dir is required"),
    ] {
        let mut all: Vec<&str> = Vec::new();
        let mut skip = false;
        for w in base {
            if skip {
                skip = false;
            } else if w == missing {
                skip = true;
            } else {
                all.push(w);
            }
        }
        assert!(
            Config::parse(&words(&all)).unwrap_err().contains(why),
            "{missing}"
        );
    }
    let bad_cases: [(&[&str], &str); 5] = [
        (
            &[
                "--layout",
                "tar",
                "--channel",
                "stable",
                "--unit",
                UNIT,
                "--dir",
                "/d",
            ],
            "deb or rpm",
        ),
        (
            &[
                "--layout",
                "deb",
                "--channel",
                "Stable!",
                "--unit",
                UNIT,
                "--dir",
                "/d",
            ],
            "not a channel",
        ),
        (
            &[
                "--layout",
                "deb",
                "--channel",
                "stable",
                "--unit",
                "sshd.service",
                "--dir",
                "/d",
            ],
            "not a kbf unit",
        ),
        (
            &[
                "--layout",
                "deb",
                "--channel",
                "stable",
                "--unit",
                UNIT,
                "--dir",
                "d",
            ],
            "absolute",
        ),
        (
            &[
                "--layout",
                "deb",
                "--channel",
                "stable",
                "--unit",
                UNIT,
                "--dir",
            ],
            "needs a value",
        ),
    ];
    for (bad, why) in bad_cases {
        assert!(
            Config::parse(&words(bad)).unwrap_err().contains(why),
            "{bad:?}"
        );
    }
}

/// A unit of a frontend, or of any mode a daemon host does not run, is not migrated by this.
#[test]
fn only_a_daemon_modes_unit_is_migrated() {
    let mig = Mig::new("mig-mode", Format::Deb);
    for name in [
        "kbf-frontend.service",
        "kbf-proxy.service",
        "kbf-frontend@a.service",
    ] {
        let mut config = mig.config(true);
        config.unit = Unit::service(name).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with(&config, &mig.rig.backend, &mig.place, &mut out, &mut err);
        assert_eq!(code, 2);
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("not the unit of a daemon mode")
        );
    }
}

#[test]
fn the_program_says_usage_for_flags_it_does_not_take() {
    let code = main(&["--layout".to_owned()]);
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
    // Good flags for a unit it does not migrate: refused before the host is looked at.
    let words = [
        "--layout",
        "deb",
        "--channel",
        "stable",
        "--unit",
        "kbf-frontend.service",
        "--dir",
        "/d",
    ];
    let code = main(&words.map(str::to_owned));
    assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
    let place = Place::production();
    assert_eq!(place.kbf, PathBuf::from("/usr/bin/kbf"));
    assert_eq!(place.systemd_dir, PathBuf::from("/etc/systemd/system"));
    assert_eq!(place.proc_dir, PathBuf::from("/proc"));
    assert!(USAGE.contains("--migrate-to-package"));
}

/// A drop-in that cannot be written, and a systemd that cannot reread even when asked to put the
/// old unit back, each stop with status 1 and say what could not be undone.
#[test]
fn a_drop_in_that_cannot_be_written_and_a_systemd_that_cannot_reread_are_said() {
    both(|format| {
        let mig = Mig::new("mig-drop-in", format);
        // The drop-in directory's name is taken by a file.
        fs::write(
            mig.place.systemd_dir.join(format!("{UNIT}.d")),
            b"in the way",
        )
        .unwrap();
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("step 2 failed"), "{err}");
        assert!(err.contains("putting it back failed"), "{err}");
        assert_eq!(fs::read_to_string(mig.unit_file()).unwrap(), mig.original);
        assert!(mig.releases().join(A).join("kbf").exists());

        let mig = Mig::new("mig-reread", format);
        mig.rig.net.borrow_mut().reload_fails = 2;
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("step 4 failed"), "{err}");
        assert!(
            err.contains("systemctl daemon-reload did not work"),
            "{err}"
        );
        assert!(mig.releases().join(A).join("kbf").exists());
    });
}

/// A package manager that cannot be started is a failed first step.
#[test]
fn a_package_manager_that_cannot_start_stops_the_first_step() {
    both(|format| {
        let mig = Mig::new("mig-no-pm", format);
        mig.rig.net.borrow_mut().broken.insert("no-install-tool");
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("cannot start the installer"), "{err}");
        assert_eq!(fs::read_to_string(mig.unit_file()).unwrap(), mig.original);
    });
}

/// A unit that gives no `--mode` runs the mode of its name, and the drop-in says so; and a
/// drop-in directory that cannot be made (its name is a link to nowhere) is undone as far as
/// there is anything to undo, with the rest said.
#[test]
fn a_unit_without_a_mode_flag_gets_its_names_mode_and_a_dangling_drop_in_directory_is_said() {
    both(|format| {
        let mig = Mig::new("mig-no-mode", format);
        let text = mig.original.replace("--mode=storage-and-daemon ", "");
        put(&mig.unit_file(), &text);
        let (code, out, err) = mig.run(false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        assert!(
            out.contains("| ExecStart=/usr/bin/kbf --mode=storage-and-daemon --cold-store="),
            "{out}"
        );

        let mig = Mig::new("mig-dangling", format);
        std::os::unix::fs::symlink(
            "/nonexistent/kbf-drop-in",
            mig.place.systemd_dir.join(format!("{UNIT}.d")),
        )
        .unwrap();
        let (code, _, err) = mig.run(true);
        assert_eq!(code, 1);
        assert!(err.contains("step 2 failed"), "{err}");
        assert!(err.contains("putting it back failed"), "{err}");
        assert_eq!(fs::read_to_string(mig.unit_file()).unwrap(), mig.original);
    });
}
