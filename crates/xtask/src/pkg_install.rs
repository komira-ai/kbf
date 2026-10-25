//! `pkg-install-test --name <package> --package <file> --format deb|rpm --listing <file>
//! --image-dir <dir> --gate <unit>=<path>.. [--probe '<command>::<text>'].. [--docker <program>]`:
//! installs the `kbf` package into a fresh container with systemd as PID 1 and checks what the
//! contract promises of it (docs/design/kbf-installer.md, sections 3 and 7;
//! packaging/linux/README.md).
//!
//! The container is built from `<image-dir>/Dockerfile` and run with `--privileged`.
//! After the package manager installs the package:
//!
//! 1. it has no dependency, its recommendations are exactly the ones this crate lists, and it
//!    has no maintainer script (nothing can start or enable anything);
//! 2. every entry of the listing is at its path, `root:root`, with its mode and bytes;
//! 3. every packaged unit is `disabled` or `static` and none is active;
//! 4. after `systemd-sysusers` and `systemd-tmpfiles --create` (what a boot, and
//!    `kbf setup`, do) the three users exist with no login shell and the three
//!    directories exist, `root:root` 0755;
//! 5. each `--gate` unit does not start without its file (the journal says its condition
//!    skipped the start), and begins to start once the file exists;
//! 6. each `--probe` command exits 0 in the container and prints its text: the packaged
//!    `gh` runs on the container's architecture, the binary names the commit.
//!
//! Remove must delete every packaged path (but a deb's conffiles, its files under `/etc`,
//! which dpkg keeps until a purge) and keep `/etc/kbf` and the installer tree (a file
//! `setup` would have written) and the users; a purge (deb only, of a package installed
//! again) must delete every packaged path, conffiles included, keep the same files, and
//! drop the package's record. The container is removed whatever the result.
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
    /// The recommendations the package must list, as the contract says them (and not as
    /// `pkg_linux` does: a check that read `pkg_linux`'s lists would pass whatever they held).
    /// It must list no dependency at all.
    fn recommends(self) -> &'static [&'static str] {
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

    /// The command that prints the dependencies of the package `name`.
    fn depends_command(self, name: &str) -> Vec<&str> {
        match self {
            Format::Deb => vec!["dpkg-query", "-W", "-f=${Depends}", name],
            Format::Rpm => vec!["rpm", "-qR", name],
        }
    }

    /// The command that prints the recommendations of the package `name`.
    fn recommends_command(self, name: &str) -> Vec<&str> {
        match self {
            Format::Deb => vec!["dpkg-query", "-W", "-f=${Recommends}", name],
            Format::Rpm => vec!["rpm", "-q", "--recommends", name],
        }
    }

    /// The command that prints the maintainer scripts of the package `name`, one per line.
    fn scripts_command<'a>(self, name: &'a str, pattern: &'a str) -> Vec<&'a str> {
        match self {
            Format::Deb => vec![
                "find",
                "/var/lib/dpkg/info",
                "-name",
                pattern,
                "-printf",
                "%f\\n",
            ],
            Format::Rpm => vec!["rpm", "-q", "--scripts", name],
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

    /// Whether the package `name` is installed.
    fn installed(self, c: &mut Container, name: &str) -> Result<bool, Error> {
        match self {
            Format::Deb => {
                let status = c.say(&["dpkg-query", "-W", "-f=${Status}", name])?;
                Ok(status == "install ok installed")
            }
            Format::Rpm => c.ok(&["rpm", "-q", name]),
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

/// A command to run in the installed container and what its output must contain, so a
/// package built from real files is shown to run where it is installed: the packaged
/// `gh` of the right architecture, the binary that names its commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub argv: Vec<String>,
    pub contains: String,
}

impl FromStr for Probe {
    type Err = String;

    fn from_str(s: &str) -> Result<Probe, String> {
        let (command, contains) = s
            .split_once("::")
            .ok_or_else(|| format!("{s:?}: expected <program> <argument>..::<text>"))?;
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if argv.is_empty() || contains.is_empty() {
            return Err(format!("{s:?}: a command and the text it must print"));
        }
        Ok(Probe {
            argv,
            contains: contains.to_owned(),
        })
    }
}

/// A running container.
pub(crate) struct Container<'a, 'b> {
    docker: &'a mut Docker<'b>,
    name: String,
}

impl<'a, 'b> Container<'a, 'b> {
    /// The running container `name`, once it has finished booting: `--wait` returns when
    /// boot is over, `running`, or `degraded` when some unrelated unit of the image failed.
    pub(crate) fn booted(docker: &'a mut Docker<'b>, name: &str) -> Result<Self, Error> {
        let mut c = Container {
            docker,
            name: name.to_owned(),
        };
        c.exec(&["systemctl", "is-system-running", "--wait"])?;
        Ok(c)
    }

    /// `docker exec <name> <args>`.
    pub(crate) fn exec(&mut self, args: &[&str]) -> Result<Out, Error> {
        let mut argv = vec!["exec", &self.name];
        argv.extend_from_slice(args);
        (self.docker)(&argv)
    }

    /// The trimmed output of `args`, whether or not it succeeded.
    pub(crate) fn say(&mut self, args: &[&str]) -> Result<String, Error> {
        Ok(self.exec(args)?.stdout.trim().to_owned())
    }

    /// Whether `args` succeeded.
    pub(crate) fn ok(&mut self, args: &[&str]) -> Result<bool, Error> {
        Ok(self.exec(args)?.ok)
    }

    /// `args` must succeed; otherwise the problem is `what` and its stderr.
    pub(crate) fn must(
        &mut self,
        what: &str,
        args: &[&str],
        problems: &mut Vec<String>,
    ) -> Result<(), Error> {
        let out = self.exec(args)?;
        if !out.ok {
            problems.push(format!("{what} failed: {}", out.stderr.trim()));
        }
        Ok(())
    }
}

/// A problem unless `got` is `want`.
pub(crate) fn eq(problems: &mut Vec<String>, what: &str, got: &str, want: &str) {
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

/// What the journal says of the starts of `unit` since boot: how many the condition skipped
/// (`was skipped because of an unmet condition check`) and how many began (`Starting ...`, which
/// systemd logs only once the conditions hold).
///
/// The journal and not `systemctl show -p ConditionResult`: systemd reports `no` for a unit that
/// is inactive again, and a unit whose program ends at once (the test's stand-in for `kbf`, or a
/// real one that fails) is inactive again before `show` runs.
fn starts(c: &mut Container, unit: &str) -> Result<(usize, usize), Error> {
    c.exec(&["journalctl", "--sync"])?;
    let log = c.say(&["journalctl", "-u", unit, "-o", "cat", "--no-pager"])?;
    let skipped = log
        .lines()
        .filter(|l| l.contains("was skipped because of an unmet condition check"))
        .count();
    let begun = log.lines().filter(|l| l.starts_with("Starting ")).count();
    Ok((skipped, begun))
}

/// The unit `gate` names, started without and then with its file.
fn gate(c: &mut Container, g: &Gate, problems: &mut Vec<String>) -> Result<(), Error> {
    let unit = &g.unit;
    // A start the condition skips succeeds; the journal says what happened.
    c.exec(&["rm", "-f", &g.path])?;
    c.exec(&["systemctl", "start", unit])?;
    let (skipped, begun) = starts(c, unit)?;
    if skipped == 0 {
        problems.push(format!(
            "{unit} without {}: its start was not skipped by its condition",
            g.path
        ));
    }
    if begun != 0 {
        problems.push(format!("{unit} without {}: it began to start", g.path));
    }
    c.must(
        "creating the gate file",
        &["install", "-D", "-m", "0644", "/dev/null", &g.path],
        problems,
    )?;
    c.exec(&["systemctl", "start", unit])?;
    let (skipped_again, begun_again) = starts(c, unit)?;
    if skipped_again != skipped {
        problems.push(format!(
            "{unit} with {}: its start was skipped by its condition again",
            g.path
        ));
    }
    if begun_again == 0 {
        problems.push(format!("{unit} with {}: it did not begin to start", g.path));
    }
    // The unit may have started or failed on what comes after its condition; either
    // way it must not stay.
    c.exec(&["systemctl", "stop", unit])?;
    c.exec(&["systemctl", "reset-failed", unit])?;
    Ok(())
}

/// The path of `e` in the container must be what the listing says.
pub(crate) fn entry(c: &mut Container, e: &Entry, problems: &mut Vec<String>) -> Result<(), Error> {
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

/// Whether `after` deletes the packaged path `e`. A deb's `remove` keeps the package's
/// conffiles, its files under `/etc` (nfpm's `config|noreplace`), as dpkg does, and its
/// `purge` deletes them; an rpm's `remove` deletes an unchanged config file.
fn deletes(format: Format, after: &str, e: &Entry) -> bool {
    !(format == Format::Deb && after == "remove" && e.dst.starts_with("/etc/"))
}

/// Every packaged path that `after` deletes must be gone.
fn gone(
    c: &mut Container,
    entries: &[Entry],
    format: Format,
    after: &str,
    problems: &mut Vec<String>,
) -> Result<(), Error> {
    for e in entries.iter().filter(|e| deletes(format, after, e)) {
        if c.ok(&["test", "-e", &e.dst])? {
            problems.push(format!("{} is still there after {after}", e.dst));
        }
    }
    Ok(())
}

/// Where the package is copied to in the container: apt takes only a path that ends
/// in `.deb`, so the file keeps its name.
pub(crate) fn inside(package: &Path) -> String {
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
    if !format.installed(c, t.name)? {
        problems.push("the package is not installed after install".to_owned());
    }
    let depends = names(&c.say(&format.depends_command(t.name))?);
    eq(&mut problems, "dependencies", &depends.join(" "), "");
    let recommends = names(&c.say(&format.recommends_command(t.name))?);
    let mut want: Vec<String> = format
        .recommends()
        .iter()
        .map(|d| (*d).to_owned())
        .collect();
    want.sort();
    eq(
        &mut problems,
        "recommendations",
        &recommends.join(" "),
        &want.join(" "),
    );
    let pattern = format!("{}.*", t.name);
    let scripts = format.scripts(&c.say(&format.scripts_command(t.name, &pattern))?);
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
    for p in t.probes {
        let argv: Vec<&str> = p.argv.iter().map(String::as_str).collect();
        let out = c.exec(&argv)?;
        if !out.ok {
            problems.push(format!(
                "`{}` failed in the container: {}",
                p.argv.join(" "),
                out.stderr.trim()
            ));
        } else if !out.stdout.contains(&p.contains) {
            problems.push(format!(
                "`{}` printed {:?}, which does not contain {:?}",
                p.argv.join(" "),
                out.stdout.trim(),
                p.contains
            ));
        }
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
    c.must("remove", &[manager, yes, "remove", t.name], &mut problems)?;
    gone(c, entries, format, "remove", &mut problems)?;
    kept(c, "remove", &mut problems)?;
    if format == Format::Deb {
        // A removed deb keeps its conffiles (its record says config-files), so purge acts
        // on a fresh install and must delete them.
        c.must(
            "reinstall",
            &[manager, yes, "install", &inside(t.package)],
            &mut problems,
        )?;
        c.must("purge", &[manager, yes, "purge", t.name], &mut problems)?;
        gone(c, entries, format, "purge", &mut problems)?;
        kept(c, "purge", &mut problems)?;
        let status = c.say(&["dpkg-query", "-W", "-f=${Status}", t.name])?;
        if !(status.is_empty() || status.ends_with("not-installed")) {
            problems.push(format!("the package's status is {status:?} after purge"));
        }
    }
    Ok(problems)
}

/// What `pkg-install-test` is given.
pub struct InstallTest<'a> {
    /// The package's name, as the package manager knows it.
    pub name: &'a str,
    pub package: &'a Path,
    pub format: Format,
    pub listing: &'a Path,
    pub image_dir: &'a Path,
    pub gates: &'a [Gate],
    /// Commands to run after the install, each with the text its output holds.
    pub probes: &'a [Probe],
    /// A name no other container of the host has.
    pub container: &'a str,
}

/// `docker <args>` must succeed.
pub(crate) fn must(docker: &mut Docker, args: &[&str]) -> Result<(), Error> {
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

/// The image built from `image_dir` as `image`, and a privileged container `container` of it
/// with systemd as PID 1.
pub(crate) fn up(
    docker: &mut Docker,
    image_dir: &Path,
    image: &str,
    container: &str,
) -> Result<(), Error> {
    must(
        docker,
        &["build", "-t", image, &image_dir.to_string_lossy()],
    )?;
    let up = [
        "run",
        "-d",
        "--name",
        container,
        "--privileged",
        "--cgroupns=host",
        "--tmpfs",
        "/run",
        "--tmpfs",
        "/tmp",
        "-v",
        "/sys/fs/cgroup:/sys/fs/cgroup:rw",
        image,
    ];
    must(docker, &up)
}

/// The container up, the checks run in it.
fn session(docker: &mut Docker, t: &InstallTest, entries: &[Entry]) -> Result<Vec<String>, Error> {
    let image = format!("{}-test-{}", t.name, t.container);
    up(docker, t.image_dir, &image, t.container)?;
    let target = format!("{}:{}", t.container, inside(t.package));
    must(docker, &["cp", &t.package.to_string_lossy(), &target])?;
    let mut c = Container::booted(docker, t.container)?;
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
    let _ = docker(&["rmi", &format!("{}-test-{}", t.name, t.container)]);
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
