//! `pkg-install-test --package <file> --format deb|rpm --listing <file> --image-dir <dir>
//! --gate <unit>=<path>.. [--docker <program>]`: installs the `kbf-installer` package
//! into a fresh container with systemd as PID 1 and checks what the contract promises of
//! it (docs/design/kbf-installer.md, sections 3 and 7; packaging/linux/README.md).
//!
//! The container is built from `<image-dir>/Dockerfile` and run with `--privileged`.
//! After the package manager installs the package:
//!
//! 1. its dependencies are exactly the ones this crate lists, and it has no maintainer
//!    script (nothing can start or enable anything);
//! 2. every entry of the listing is at its path, `root:root`, with its mode and bytes;
//! 3. every packaged unit is `disabled` or `static` and none is active;
//! 4. after `systemd-sysusers` and `systemd-tmpfiles --create` (what a boot, and
//!    `kbf-installer setup`, do) the three users exist with no login shell and the three
//!    directories exist, `root:root` 0755;
//! 5. each `--gate` unit does not start without its file, and its condition passes once
//!    the file exists.
//!
//! Remove must delete every packaged path and keep `/etc/kbf` and the installer tree
//! (a file `setup` would have written) and the users; a purge (deb only, of a package
//! installed again) must do the same and drop the package's record. The container is removed whatever the result.
//!
//! Every command is one `docker` argv run through a closure ([`Docker`]), so the checks
//! are tested against a model of the container; [`process`] runs the real program.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use crate::pkg_linux::{self, Entry, Out};
use crate::{Error, package::read};

/// Runs `docker <args>`.
pub type Docker<'a> = dyn FnMut(&[&str]) -> Result<Out, Error> + 'a;

/// The closure that runs the program `docker`.
pub fn process(docker: &Path) -> impl FnMut(&[&str]) -> Result<Out, Error> {
    let docker = docker.to_owned();
    move |args| pkg_linux::spawn(std::process::Command::new(&docker).args(args))
}

/// A package format and the tools that install it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Deb,
    Rpm,
}

impl Format {
    /// The dependencies the package must list, as the contract says them (and not as
    /// `pkg_linux` does: a check that read `pkg_linux`'s lists would pass whatever they held).
    fn depends(self) -> &'static [&'static str] {
        match self {
            Format::Deb => &["crun", "uidmap", "systemd", "ca-certificates"],
            Format::Rpm => &["crun", "shadow-utils", "systemd", "ca-certificates"],
        }
    }

    /// The package manager's command, and the `remove` verb's.
    fn manager(self) -> [&'static str; 2] {
        match self {
            Format::Deb => ["apt-get", "-y"],
            Format::Rpm => ["dnf", "-y"],
        }
    }

    /// The command that prints the dependencies.
    fn depends_command(self) -> &'static [&'static str] {
        match self {
            Format::Deb => &["dpkg-query", "-W", "-f=${Depends}", pkg_linux::NAME],
            Format::Rpm => &["rpm", "-qR", pkg_linux::NAME],
        }
    }

    /// The command that prints the package's maintainer scripts, one per line.
    fn scripts_command(self) -> &'static [&'static str] {
        match self {
            Format::Deb => &[
                "find",
                "/var/lib/dpkg/info",
                "-name",
                "kbf-installer.*",
                "-printf",
                "%f\\n",
            ],
            Format::Rpm => &["rpm", "-q", "--scripts", pkg_linux::NAME],
        }
    }

    /// The scripts among `lines` of [`Format::scripts_command`].
    fn scripts(self, lines: &str) -> Vec<String> {
        const DPKG: [&str; 4] = [".preinst", ".postinst", ".prerm", ".postrm"];
        lines
            .lines()
            .filter(|l| match self {
                Format::Deb => DPKG.iter().any(|s| l.ends_with(s)),
                Format::Rpm => !l.trim().is_empty(),
            })
            .map(str::to_owned)
            .collect()
    }

    /// Whether the package is installed.
    fn installed(self, c: &mut Container) -> Result<bool, Error> {
        match self {
            Format::Deb => {
                let status = c.say(&["dpkg-query", "-W", "-f=${Status}", pkg_linux::NAME])?;
                Ok(status == "install ok installed")
            }
            Format::Rpm => c.ok(&["rpm", "-q", pkg_linux::NAME]),
        }
    }
}

/// A unit and the file whose absence keeps it from starting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub unit: String,
    pub path: String,
}

impl FromStr for Gate {
    type Err = String;

    fn from_str(s: &str) -> Result<Gate, String> {
        let (unit, path) = s
            .split_once('=')
            .ok_or_else(|| format!("{s:?}: expected <unit>=<file>"))?;
        Ok(Gate {
            unit: unit.to_owned(),
            path: path.to_owned(),
        })
    }
}

/// A running container.
struct Container<'a, 'b> {
    docker: &'a mut Docker<'b>,
    name: String,
}

impl Container<'_, '_> {
    /// `docker exec <name> <args>`.
    fn exec(&mut self, args: &[&str]) -> Result<Out, Error> {
        let mut argv = vec!["exec", &self.name];
        argv.extend_from_slice(args);
        (self.docker)(&argv)
    }

    /// The trimmed output of `args`, whether or not it succeeded.
    fn say(&mut self, args: &[&str]) -> Result<String, Error> {
        Ok(self.exec(args)?.stdout.trim().to_owned())
    }

    /// Whether `args` succeeded.
    fn ok(&mut self, args: &[&str]) -> Result<bool, Error> {
        Ok(self.exec(args)?.ok)
    }

    /// `args` must succeed; otherwise the problem is `what` and its stderr.
    fn must(&mut self, what: &str, args: &[&str], problems: &mut Vec<String>) -> Result<(), Error> {
        let out = self.exec(args)?;
        if !out.ok {
            problems.push(format!("{what} failed: {}", out.stderr.trim()));
        }
        Ok(())
    }

    /// The properties `systemctl show` prints for `unit`.
    fn show(&mut self, unit: &str) -> Result<BTreeMap<String, String>, Error> {
        let mut args = vec!["systemctl", "show", unit];
        args.extend(["-p", "ConditionResult", "-p", "ExecMainStartTimestamp"]);
        let shown = self.say(&args)?;
        Ok(shown
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect())
    }
}

/// A problem unless `got` is `want`.
fn eq(problems: &mut Vec<String>, what: &str, got: &str, want: &str) {
    if got != want {
        problems.push(format!("{what}: {got:?}, not {want:?}"));
    }
}

/// The names in the output of a dependency query, sorted: `rpmlib(...)` lines and the
/// separators of dpkg's one line dropped.
fn names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|n| !n.is_empty() && !n.starts_with("rpmlib("))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// The unit `gate` names, started without and then with its file.
fn gate(c: &mut Container, g: &Gate, problems: &mut Vec<String>) -> Result<(), Error> {
    let unit = &g.unit;
    // A start the condition skips succeeds; the properties say what happened.
    c.exec(&["rm", "-f", &g.path])?;
    c.exec(&["systemctl", "start", unit])?;
    let shown = c.show(unit)?;
    let got = |k: &str| shown.get(k).map_or("", String::as_str).to_owned();
    eq(
        problems,
        &format!("{unit} without {}: ConditionResult", g.path),
        &got("ConditionResult"),
        "no",
    );
    eq(
        problems,
        &format!("{unit} without {}: ExecMainStartTimestamp", g.path),
        &got("ExecMainStartTimestamp"),
        "",
    );
    c.must(
        "creating the gate file",
        &["install", "-D", "-m", "0644", "/dev/null", &g.path],
        problems,
    )?;
    c.exec(&["systemctl", "start", unit])?;
    let shown = c.show(unit)?;
    let got = |k: &str| shown.get(k).map_or("", String::as_str).to_owned();
    eq(
        problems,
        &format!("{unit} with {}: ConditionResult", g.path),
        &got("ConditionResult"),
        "yes",
    );
    // The unit may have started or failed on what comes after its condition; either
    // way it must not stay.
    c.exec(&["systemctl", "stop", unit])?;
    c.exec(&["systemctl", "reset-failed", unit])?;
    Ok(())
}

/// The path of `e` in the container must be what the listing says.
fn entry(c: &mut Container, e: &Entry, problems: &mut Vec<String>) -> Result<(), Error> {
    let kind = if e.sha256.is_some() {
        "regular file"
    } else {
        "directory"
    };
    let want = format!("root:root:{:o}:{kind}", e.mode);
    let got = c.say(&["stat", "-c", "%U:%G:%a:%F", &e.dst])?;
    eq(problems, &e.dst, &got, &want);
    if let Some(sha) = e.sha256 {
        let sum = c.say(&["sha256sum", &e.dst])?;
        eq(
            problems,
            &format!("{} sha256", e.dst),
            sum.split(' ').next().unwrap_or(""),
            &sha.to_string(),
        );
    }
    Ok(())
}

/// What remove and purge leave: the operator's files, the installer tree and the users.
fn kept(c: &mut Container, after: &str, problems: &mut Vec<String>) -> Result<(), Error> {
    for path in ["/etc/kbf", "/etc/kbf/kbf.toml", "/etc/kbf/installer.toml"] {
        if !c.ok(&["test", "-e", path])? {
            problems.push(format!("{after} deleted {path}"));
        }
    }
    let state = format!("{}/state.json", pkg_linux::STATE_DIR);
    if !c.ok(&["test", "-e", &state])? {
        problems.push(format!("{after} deleted {state}"));
    }
    for user in pkg_linux::USERS {
        if !c.ok(&["getent", "passwd", user])? {
            problems.push(format!("{after} deleted the user {user}"));
        }
    }
    Ok(())
}

/// Every packaged path must be gone.
fn gone(
    c: &mut Container,
    entries: &[Entry],
    after: &str,
    problems: &mut Vec<String>,
) -> Result<(), Error> {
    for e in entries {
        if c.ok(&["test", "-e", &e.dst])? {
            problems.push(format!("{} is still there after {after}", e.dst));
        }
    }
    Ok(())
}

/// Where the package is copied to in the container: apt takes only a path that ends
/// in `.deb`, so the file keeps its name.
fn inside(package: &Path) -> String {
    format!(
        "/var/tmp/{}",
        package.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// The checks after the container is up.
fn checks(c: &mut Container, t: &InstallTest, entries: &[Entry]) -> Result<Vec<String>, Error> {
    let (format, gates) = (t.format, t.gates);
    let mut problems = Vec::new();
    let [manager, yes] = format.manager();
    c.must(
        "install",
        &[manager, yes, "install", &inside(t.package)],
        &mut problems,
    )?;
    if !format.installed(c)? {
        problems.push("the package is not installed after install".to_owned());
    }
    let depends = names(&c.say(format.depends_command())?);
    let mut want: Vec<String> = format.depends().iter().map(|d| (*d).to_owned()).collect();
    want.sort();
    eq(
        &mut problems,
        "dependencies",
        &depends.join(" "),
        &want.join(" "),
    );
    let scripts = format.scripts(&c.say(format.scripts_command())?);
    eq(&mut problems, "maintainer scripts", &scripts.join(" "), "");
    for e in entries {
        entry(c, e, &mut problems)?;
    }
    // `kbf*`, not `kbf-*`: the slice is `kbf.slice`, which the hyphen glob does not match.
    let known = c.say(&["systemctl", "list-unit-files", "kbf*", "--no-legend"])?;
    let states: BTreeMap<&str, &str> = known
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(unit, rest)| (unit, rest.trim().split(' ').next().unwrap_or("")))
        .collect();
    for e in entries {
        let Some(unit) = e
            .dst
            .strip_prefix(pkg_linux::UNIT_DIR)
            .and_then(|u| u.strip_prefix('/'))
        else {
            continue;
        };
        let state = states.get(unit).copied().unwrap_or("not known to systemd");
        if !matches!(state, "disabled" | "static") {
            problems.push(format!("{unit} is {state:?} after install, not disabled"));
        }
    }
    let running = c.say(&[
        "systemctl",
        "list-units",
        "kbf-*",
        "--all",
        "--state=active,activating,failed",
        "--no-legend",
    ])?;
    eq(
        &mut problems,
        "units active or failed after install",
        &running,
        "",
    );
    c.must("systemd-sysusers", &["systemd-sysusers"], &mut problems)?;
    c.must(
        "systemd-tmpfiles",
        &["systemd-tmpfiles", "--create"],
        &mut problems,
    )?;
    for user in pkg_linux::USERS {
        let line = c.say(&["getent", "passwd", user])?;
        let shell = line.rsplit(':').next().unwrap_or("");
        eq(
            &mut problems,
            &format!("{user}'s login shell"),
            shell,
            "/usr/sbin/nologin",
        );
    }
    // Every account a packaged unit runs as exists once sysusers has run.
    for e in entries {
        if !e.dst.starts_with(pkg_linux::UNIT_DIR) || e.sha256.is_none() {
            continue;
        }
        for (key, name) in pkg_linux::unit_accounts(&c.say(&["cat", &e.dst])?) {
            let database = if key == "User" { "passwd" } else { "group" };
            if !c.ok(&["getent", database, &name])? {
                problems.push(format!(
                    "{} names {key}={name}, which does not exist",
                    e.dst
                ));
            }
        }
    }
    for (path, mode) in pkg_linux::TMPFILES_DIRS {
        let got = c.say(&["stat", "-c", "%U:%G:%a:%F", path])?;
        eq(
            &mut problems,
            path,
            &got,
            &format!("root:root:{mode}:directory"),
        );
    }
    for g in gates {
        gate(c, g, &mut problems)?;
    }
    // What `setup` would have written, which remove and purge must leave.
    c.must(
        "writing the installer state",
        &[
            "install",
            "-D",
            "-m",
            "0600",
            "/dev/null",
            &format!("{}/state.json", pkg_linux::STATE_DIR),
        ],
        &mut problems,
    )?;
    c.must(
        "writing the installer config",
        &[
            "install",
            "-D",
            "-m",
            "0644",
            "/dev/null",
            "/etc/kbf/installer.toml",
        ],
        &mut problems,
    )?;
    c.must(
        "writing the runtime config",
        &["install", "-m", "0640", "/dev/null", "/etc/kbf/kbf.toml"],
        &mut problems,
    )?;
    c.must(
        "remove",
        &[manager, yes, "remove", pkg_linux::NAME],
        &mut problems,
    )?;
    gone(c, entries, "remove", &mut problems)?;
    kept(c, "remove", &mut problems)?;
    if format == Format::Deb {
        // A removed package without conffiles is gone, so purge acts on a fresh install.
        c.must(
            "reinstall",
            &[manager, yes, "install", &inside(t.package)],
            &mut problems,
        )?;
        c.must(
            "purge",
            &[manager, yes, "purge", pkg_linux::NAME],
            &mut problems,
        )?;
        kept(c, "purge", &mut problems)?;
        let status = c.say(&["dpkg-query", "-W", "-f=${Status}", pkg_linux::NAME])?;
        if !(status.is_empty() || status.ends_with("not-installed")) {
            problems.push(format!("the package's status is {status:?} after purge"));
        }
    }
    Ok(problems)
}

/// What `pkg-install-test` is given.
pub struct InstallTest<'a> {
    pub package: &'a Path,
    pub format: Format,
    pub listing: &'a Path,
    pub image_dir: &'a Path,
    pub gates: &'a [Gate],
    /// A name no other container of the host has.
    pub container: &'a str,
}

/// `docker <args>` must succeed.
fn must(docker: &mut Docker, args: &[&str]) -> Result<(), Error> {
    let out = docker(args)?;
    if out.ok {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "docker {}: {}",
        args[0],
        out.stderr.trim()
    )))
}

/// The container up, the checks run in it.
fn session(docker: &mut Docker, t: &InstallTest, entries: &[Entry]) -> Result<Vec<String>, Error> {
    let image = format!("{}-test-{}", pkg_linux::NAME, t.container);
    must(
        docker,
        &["build", "-t", &image, &t.image_dir.to_string_lossy()],
    )?;
    let up = [
        "run",
        "-d",
        "--name",
        t.container,
        "--privileged",
        "--cgroupns=host",
        "--tmpfs",
        "/run",
        "--tmpfs",
        "/tmp",
        "-v",
        "/sys/fs/cgroup:/sys/fs/cgroup:rw",
        &image,
    ];
    must(docker, &up)?;
    let target = format!("{}:{}", t.container, inside(t.package));
    must(docker, &["cp", &t.package.to_string_lossy(), &target])?;
    let mut c = Container {
        docker,
        name: t.container.to_owned(),
    };
    // `--wait` returns once boot is over: `running`, or `degraded` when some unrelated
    // unit of the image failed.
    c.exec(&["systemctl", "is-system-running", "--wait"])?;
    checks(&mut c, t, entries)
}

/// The `pkg-install-test` verb: the lines to print.
///
/// # Errors
/// The listing cannot be read, docker fails, or a check finds a problem.
pub fn run(docker: &mut Docker, t: &InstallTest) -> Result<Vec<String>, Error> {
    let text = String::from_utf8_lossy(&read(t.listing)?).into_owned();
    let entries = pkg_linux::parse_listing(&text)?;
    let result = session(docker, t, &entries);
    // The container and its image are not needed after the run, whatever its result.
    let _ = docker(&["rm", "-f", t.container]);
    let _ = docker(&["rmi", &format!("{}-test-{}", pkg_linux::NAME, t.container)]);
    let problems = result?;
    if problems.is_empty() {
        return Ok(vec![format!(
            "{} installs disabled, gated and without scripts; remove (and purge, for a deb) leave the operator's files",
            t.package.display()
        )]);
    }
    Err(Error::Refused(problems))
}

#[cfg(test)]
mod tests;
