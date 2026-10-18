use std::collections::BTreeSet;
use std::path::PathBuf;

use super::*;
use crate::fetch::Sha256;

/// A defect of the package or of the system, planted in the model of the container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Defect {
    None,
    InstallFails,
    NotInstalled,
    WrongDepends,
    PodmanDepends,
    MaintainerScript,
    WrongOwner,
    WrongBytes,
    UnitEnabled,
    UnitMissing,
    UnitActive,
    UserWithShell,
    DirNotMade,
    GateIgnored,
    GateRunsWithoutFile,
    RemoveKeepsFile,
    RemoveDeletesConfig,
    RemoveDeletesState,
    RemoveDeletesUser,
    PurgeDeletesConfig,
    PurgeLeavesRecord,
    BuildFails,
}

/// A model of a container: the files that exist, the users, what is installed, the
/// answers of the tools. `log` holds every argv.
struct World {
    entries: Vec<Entry>,
    defect: Defect,
    files: BTreeSet<String>,
    users_made: bool,
    installed: bool,
    purged: bool,
    started: BTreeSet<String>,
    log: Vec<String>,
}

fn ok(stdout: &str) -> Out {
    Out {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn fail(stderr: &str) -> Out {
    Out {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

fn entry_file(dst: &str, mode: u32, text: &str) -> Entry {
    Entry {
        dst: dst.to_owned(),
        mode,
        sha256: Some(Sha256::of(text.as_bytes())),
        src: PathBuf::new(),
    }
}

fn entry_dir(dst: &str) -> Entry {
    Entry {
        dst: dst.to_owned(),
        mode: 0o755,
        sha256: None,
        src: PathBuf::new(),
    }
}

fn entries() -> Vec<Entry> {
    vec![
        entry_file("/usr/bin/kbf-installer", 0o755, "binary"),
        entry_file("/usr/lib/systemd/system/kbf-node.service", 0o644, "node"),
        entry_dir("/usr/share/kbf-installer"),
    ]
}

impl World {
    fn new(defect: Defect) -> World {
        World {
            entries: entries(),
            defect,
            files: BTreeSet::new(),
            users_made: false,
            installed: false,
            purged: false,
            started: BTreeSet::new(),
            log: Vec::new(),
        }
    }

    fn is(&self, d: Defect) -> bool {
        self.defect == d
    }

    /// The model's answer to `docker <args>`.
    fn docker(&mut self, args: &[&str]) -> Out {
        self.log.push(args.join(" "));
        match args {
            ["build", ..] if self.is(Defect::BuildFails) => fail("no such image"),
            ["exec", _, rest @ ..] => self.exec(rest),
            _ => ok(""),
        }
    }

    fn exec(&mut self, args: &[&str]) -> Out {
        match args {
            [manager, "-y", "install", _] if matches!(*manager, "apt-get" | "dnf") => {
                self.install()
            }
            [_, "-y", "remove", _] => self.remove(false),
            [_, "-y", "purge", _] => self.remove(true),
            ["dpkg-query", "-W", "-f=${Status}", _] => ok(self.status()),
            ["rpm", "-q", _] => Out {
                ok: self.installed,
                ..ok("")
            },
            ["dpkg-query", ..] => ok(if self.is(Defect::WrongDepends) {
                "crun, systemd"
            } else if self.is(Defect::PodmanDepends) {
                "crun, uidmap, systemd, ca-certificates, podman"
            } else {
                "crun, uidmap, systemd, ca-certificates"
            }),
            ["rpm", "-qR", _] => ok(
                "crun\nshadow-utils\nsystemd\nca-certificates\nrpmlib(CompressedFileNames) <= 3.0.4-1\n",
            ),
            ["find", ..] => ok(if self.is(Defect::MaintainerScript) {
                "kbf-installer.list\nkbf-installer.postinst\n"
            } else {
                "kbf-installer.list\nkbf-installer.md5sums\n"
            }),
            ["rpm", "-q", "--scripts", _] => ok(if self.is(Defect::MaintainerScript) {
                "postinstall scriptlet\n"
            } else {
                ""
            }),
            ["stat", "-c", _, path] => self.stat(path),
            ["sha256sum", path] => {
                let sha = self
                    .entries
                    .iter()
                    .find(|e| e.dst == *path)
                    .and_then(|e| e.sha256)
                    .expect("file");
                let sha = if self.is(Defect::WrongBytes) {
                    Sha256::of(b"other")
                } else {
                    sha
                };
                ok(&format!("{sha}  {path}\n"))
            }
            ["systemctl", "list-unit-files", ..] => self.unit_files(),
            ["systemctl", "list-units", ..] => ok(if self.is(Defect::UnitActive) {
                "kbf-node.service loaded active running\n"
            } else {
                ""
            }),
            ["systemctl", "start", unit] => {
                self.started.insert((*unit).to_owned());
                ok("")
            }
            ["systemctl", "show", unit, ..] => self.show(unit),
            ["systemd-sysusers"] => {
                self.users_made = true;
                ok("")
            }
            ["getent", "passwd", user] => self.getent(user),
            ["rm", "-f", path] => {
                self.files.remove(*path);
                ok("")
            }
            ["install", .., "/dev/null", path] => {
                self.files.insert((*path).to_owned());
                // Its directory exists too (`install -D`; /etc/kbf for the config).
                if let Some((dir, _)) = path.rsplit_once('/') {
                    self.files.insert(dir.to_owned());
                }
                ok("")
            }
            ["test", "-e", path] => Out {
                ok: self.files.contains(*path),
                ..ok("")
            },
            _ => ok(""),
        }
    }

    fn status(&self) -> &'static str {
        match (self.installed, self.purged) {
            (true, _) => "install ok installed",
            (false, true) if !self.is(Defect::PurgeLeavesRecord) => "unknown ok not-installed",
            (false, true) => "deinstall ok config-files",
            (false, false) => "",
        }
    }

    fn install(&mut self) -> Out {
        if self.is(Defect::InstallFails) {
            return fail("E: unable to locate package");
        }
        self.installed = !self.is(Defect::NotInstalled);
        self.files
            .extend(self.entries.iter().map(|e| e.dst.clone()));
        ok("")
    }

    fn remove(&mut self, purge: bool) -> Out {
        if !self.installed {
            return fail("E: Unable to locate package kbf-installer");
        }
        self.installed = false;
        self.purged = purge;
        for e in &self.entries {
            if !(self.defect == Defect::RemoveKeepsFile && e.dst.ends_with("kbf-installer")) {
                self.files.remove(&e.dst);
            }
        }
        let doomed = match (purge, self.defect) {
            (false, Defect::RemoveDeletesConfig) | (true, Defect::PurgeDeletesConfig) => {
                Some("/etc/kbf/kbf.toml")
            }
            (false, Defect::RemoveDeletesState) => Some("/var/lib/kbf-installer/state.json"),
            _ => None,
        };
        if let Some(path) = doomed {
            self.files.remove(path);
        }
        if self.is(Defect::RemoveDeletesUser) {
            self.users_made = false;
        }
        ok("")
    }

    fn stat(&self, path: &str) -> Out {
        if let Some((_, mode)) = pkg::TMPFILES_DIRS.iter().find(|(p, _)| *p == path) {
            return if self.users_made && !self.is(Defect::DirNotMade) {
                ok(&format!("root:root:{mode}:directory"))
            } else {
                fail("no such file")
            };
        }
        match self.entries.iter().find(|e| e.dst == path) {
            Some(e) if self.files.contains(path) => {
                let owner = if self.is(Defect::WrongOwner) {
                    "kbf"
                } else {
                    "root"
                };
                let kind = if e.sha256.is_some() {
                    "regular file"
                } else {
                    "directory"
                };
                ok(&format!("{owner}:root:{:o}:{kind}\n", e.mode))
            }
            _ => fail("no such file"),
        }
    }

    fn unit_files(&self) -> Out {
        if self.is(Defect::UnitMissing) {
            return ok("");
        }
        let state = if self.is(Defect::UnitEnabled) {
            "enabled"
        } else {
            "disabled"
        };
        ok(&format!("kbf-node.service     {state} enabled\n"))
    }

    fn show(&self, unit: &str) -> Out {
        // The gate: the unit's condition holds when its file exists.
        let path = if unit.contains("node") {
            "/etc/kbf/kbf.toml"
        } else {
            "/etc/kbf/installer.toml"
        };
        let holds = self.files.contains(path)
            || self.is(Defect::GateIgnored)
            || (self.is(Defect::GateRunsWithoutFile) && !self.files.contains(path));
        let ran = if self.is(Defect::GateRunsWithoutFile) {
            "Mon 2026-01-01 00:00:00 UTC"
        } else {
            ""
        };
        ok(&format!(
            "ConditionResult={}\nExecMainStartTimestamp={ran}\n",
            if holds { "yes" } else { "no" }
        ))
    }

    fn getent(&self, user: &str) -> Out {
        if !self.users_made {
            return fail("");
        }
        let shell = if self.is(Defect::UserWithShell) && user == "kbf-verify" {
            "/opt/shell"
        } else {
            "/usr/sbin/nologin"
        };
        ok(&format!("{user}:x:999:999:{user}:/nonexistent:{shell}\n"))
    }
}

const GATES: [(&str, &str); 2] = [
    ("kbf-node.service", "/etc/kbf/kbf.toml"),
    ("kbf-installer.service", "/etc/kbf/installer.toml"),
];

fn gates() -> Vec<Gate> {
    GATES
        .iter()
        .map(|(u, p)| format!("{u}={p}").parse().expect("gate"))
        .collect()
}

/// The verb over a model with `defect`: the world after, and the result.
fn run_with(format: Format, defect: Defect) -> (World, Result<Vec<String>, Error>) {
    let dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .expect("deps")
        .join("xtask-pkg-install-unit");
    std::fs::create_dir_all(&dir).expect("dir");
    let listing = dir.join(format!("listing-{}.txt", std::process::id()));
    std::fs::write(&listing, pkg::listing(&entries())).expect("listing");
    let world = std::cell::RefCell::new(World::new(defect));
    let gates = gates();
    let result = run(
        &mut |args| Ok(world.borrow_mut().docker(args)),
        &InstallTest {
            package: Path::new("/p/kbf-installer.pkg"),
            format,
            listing: &listing,
            image_dir: Path::new("/p/image"),
            gates: &gates,
            container: "c1",
        },
    );
    (world.into_inner(), result)
}

fn problems(format: Format, defect: Defect) -> String {
    match run_with(format, defect).1 {
        Err(Error::Refused(p)) => p.join("\n"),
        other => panic!("{defect:?}: expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_package_that_does_everything_the_contract_says_passes_in_both_formats() {
    for format in [Format::Deb, Format::Rpm] {
        let (world, result) = run_with(format, Defect::None);
        let lines = result.unwrap_or_else(|e| panic!("{format:?}: {e}"));
        assert_eq!(lines.len(), 1);
        let ran = |needle: &str| world.log.iter().any(|l| l.contains(needle));
        assert_eq!(ran("purge"), format == Format::Deb, "only a deb is purged");
        assert!(ran("remove"));
        // The container and its image are removed whatever happened.
        assert_eq!(world.log[world.log.len() - 2], "rm -f c1");
        assert!(world.log[world.log.len() - 1].starts_with("rmi kbf-installer-test-c1"));
        // Nothing the test runs starts a unit on its own: it only starts the gated ones
        // it asks to.
        assert_eq!(world.started.len(), 2);
    }
}

#[test]
fn each_defect_of_the_package_or_the_system_is_a_named_problem() {
    let cases: [(Format, Defect, &str); 17] = [
        (
            Format::Deb,
            Defect::NotInstalled,
            "the package is not installed after install",
        ),
        (Format::Deb, Defect::WrongDepends, "dependencies:"),
        (Format::Deb, Defect::PodmanDepends, "podman"),
        (
            Format::Deb,
            Defect::MaintainerScript,
            "maintainer scripts: \"kbf-installer.postinst\"",
        ),
        (
            Format::Rpm,
            Defect::MaintainerScript,
            "maintainer scripts: \"postinstall scriptlet\"",
        ),
        (
            Format::Deb,
            Defect::WrongOwner,
            "/usr/bin/kbf-installer: \"kbf:root:755:regular file\"",
        ),
        (
            Format::Deb,
            Defect::WrongBytes,
            "/usr/bin/kbf-installer sha256",
        ),
        (
            Format::Deb,
            Defect::UnitEnabled,
            "kbf-node.service is \"enabled\" after install",
        ),
        (
            Format::Deb,
            Defect::UnitMissing,
            "kbf-node.service is \"not known to systemd\"",
        ),
        (
            Format::Deb,
            Defect::UnitActive,
            "units active or failed after install",
        ),
        (
            Format::Deb,
            Defect::UserWithShell,
            "kbf-verify's login shell: \"/opt/shell\"",
        ),
        (Format::Deb, Defect::DirNotMade, "/run/kbf-installer"),
        (
            Format::Deb,
            Defect::GateIgnored,
            "kbf-node.service without /etc/kbf/kbf.toml: ConditionResult",
        ),
        (
            Format::Deb,
            Defect::GateRunsWithoutFile,
            "kbf-installer.service without /etc/kbf/installer.toml: ExecMainStartTimestamp",
        ),
        (
            Format::Deb,
            Defect::RemoveKeepsFile,
            "/usr/bin/kbf-installer is still there after remove",
        ),
        (
            Format::Deb,
            Defect::RemoveDeletesConfig,
            "remove deleted /etc/kbf/kbf.toml",
        ),
        (
            Format::Rpm,
            Defect::RemoveDeletesState,
            "remove deleted /var/lib/kbf-installer/state.json",
        ),
    ];
    for (format, defect, says) in cases {
        let text = problems(format, defect);
        assert!(text.contains(says), "{defect:?} on {format:?}: {text}");
    }
}

#[test]
fn remove_and_purge_must_leave_the_users_and_the_operators_files() {
    let text = problems(Format::Deb, Defect::RemoveDeletesUser);
    assert!(text.contains("remove deleted the user kbf"), "{text}");
    let text = problems(Format::Deb, Defect::PurgeDeletesConfig);
    assert!(text.contains("purge deleted /etc/kbf/kbf.toml"), "{text}");
    let text = problems(Format::Deb, Defect::PurgeLeavesRecord);
    assert!(
        text.contains("status is \"deinstall ok config-files\" after purge"),
        "{text}"
    );
}

#[test]
fn docker_that_fails_to_build_or_the_installer_that_fails_ends_the_run() {
    let (world, result) = run_with(Format::Deb, Defect::BuildFails);
    assert_eq!(
        result.expect_err("build"),
        Error::Failed("docker build: no such image".to_owned())
    );
    assert!(
        world.log.iter().any(|l| l == "rm -f c1"),
        "the container is removed"
    );
    let text = problems(Format::Deb, Defect::InstallFails);
    assert!(
        text.contains("install failed: E: unable to locate package"),
        "{text}"
    );
}

#[test]
fn a_gate_is_unit_equals_file() {
    let g: Gate = "kbf-node.service=/etc/kbf/kbf.toml".parse().expect("gate");
    assert_eq!(
        (g.unit.as_str(), g.path.as_str()),
        ("kbf-node.service", "/etc/kbf/kbf.toml")
    );
    assert!(
        "kbf-node.service"
            .parse::<Gate>()
            .expect_err("no file")
            .contains("expected <unit>=<file>")
    );
}

#[test]
fn dependency_names_ignore_rpmlib_lines_and_order() {
    assert_eq!(names("b, a\nrpmlib(X) <= 1\n\nc"), ["a", "b", "c"]);
}

#[test]
fn a_listing_that_is_not_there_is_an_error_before_docker_runs() {
    let mut never = |_: &[&str]| -> Result<Out, Error> { panic!("docker ran") };
    let err = run(
        &mut never,
        &InstallTest {
            package: Path::new("/p"),
            format: Format::Deb,
            listing: Path::new("/nonexistent/listing.txt"),
            image_dir: Path::new("/i"),
            gates: &[],
            container: "c",
        },
    )
    .expect_err("no listing");
    assert!(
        err.to_string().contains("/nonexistent/listing.txt"),
        "{err}"
    );
}

#[test]
fn the_docker_process_runs_the_program_with_the_arguments() {
    let mut echo = process(Path::new("/usr/bin/echo"));
    let out = echo(&["a", "b"]).expect("echo");
    assert_eq!(out.stdout, "a b\n");
}
