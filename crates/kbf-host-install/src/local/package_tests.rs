//! The package host over a scratch root: `dpkg`, `rpm`, `systemctl`, `curl` and `gh` are played
//! by the test, and the package database is a field of the fake. A package file's bytes are
//! `name|version|arch`, which the fake package manager reads back the way a real one reads its
//! file, so a package that is not the one asked for is a file with other bytes.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_installer::binary::Arch;
use kbf_installer::fixtures::{COMMIT, FakeRunner, SETTINGS_PATH, me, ran, trusted_host_on};
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::run::Spawn;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

use super::answer::{Answer, Code};
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::linux::Tools;
use super::package::PackageHost;
use super::package_format::Format;
use super::package_store::{Record, Store};
use super::request::LocalRequest;
use crate::host::Host;
use crate::plan::Wait;
use crate::request::{Channel, Unit, Version};
use crate::runner::{Cmd, Exec, Runner};
use crate::trust::Trust;

/// The release that runs when a test starts, installed by the package manager and never staged
/// by this program.
pub(crate) const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A third build.
pub(crate) const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

pub(crate) fn build(commit: &str, seq: u32, run: u32) -> String {
    format!("0.1.{seq}+g{}-{run}", &commit[..12])
}

pub(crate) fn a_version() -> String {
    build(A, 6, 2)
}

pub(crate) fn new_version() -> String {
    build(COMMIT, 7, 3)
}

pub(crate) fn c_version() -> String {
    build(C, 8, 4)
}

pub(crate) fn arch_name() -> String {
    Arch::current().to_string()
}

pub(crate) fn stable() -> Channel {
    Channel::new("stable").unwrap()
}

pub(crate) fn version(text: &str) -> Version {
    Version::new(text).unwrap()
}

/// The bytes of a package file: its name, version and architecture.
pub(crate) fn bytes_of(name: &str, version: &str, arch: &str) -> Vec<u8> {
    format!("{name}|{version}|{arch}").into_bytes()
}

pub(crate) fn fields_of(bytes: &[u8]) -> (String, String, String) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut each = text.split('|');
    let mut next = || each.next().unwrap_or("").to_owned();
    (next(), next(), next())
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An rpm's Version and Release for the deb's one version text.
pub(crate) fn rpm_pair(version: &str) -> String {
    let Some((base, rest)) = version.split_once("+g") else {
        return version.to_owned();
    };
    let (sha, run) = rest.split_once('-').unwrap();
    format!("{base} {run}.g{sha}")
}

pub(crate) fn put(path: &Path, text: impl AsRef<str>) {
    fs::write(path, text.as_ref()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

pub(crate) fn exec(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

/// What the network, the package database and the service manager are.
pub(crate) struct Net {
    pub(crate) format: Format,
    /// The state directory, which the fake package manager looks at when it installs.
    pub(crate) state_dir: PathBuf,
    /// The listing of the releases, one JSON object each.
    pub(crate) releases: Vec<String>,
    /// The files of each release, by tag and then by asset name.
    pub(crate) assets: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// The version of the installed package.
    pub(crate) installed: Option<String>,
    /// What happened, in order.
    pub(crate) events: Vec<String>,
    pub(crate) broken: BTreeSet<&'static str>,
    /// Units that do not restart the first time.
    pub(crate) restart_fails_once: BTreeSet<String>,
    /// How many installations fail before they work.
    pub(crate) install_fails: usize,
    /// Units that never restart.
    pub(crate) restart_never: BTreeSet<String>,
    /// How many times `systemctl daemon-reload` fails before it works.
    pub(crate) reload_fails: usize,
    /// What `systemctl show --property=MainPID --value` prints.
    pub(crate) main_pid: String,
    /// The commit `/usr/bin/kbf --version --json` prints, when it prints one.
    pub(crate) kbf_commit: Option<String>,
    /// A path whose existence is noted when the main process is asked for: the old layout.
    pub(crate) watch: Option<PathBuf>,
    /// Release files (by the end of their names) that `gh release download` does not leave.
    pub(crate) skip_files: Vec<String>,
    /// The answers of `systemctl is-active`, first to last: the exit status and what it prints;
    /// once they are used up the unit is active.
    pub(crate) active_script: VecDeque<(i32, String)>,
    /// The package's kbf, run to read flags, refuses them with this line.
    pub(crate) kbf_refuses: Option<String>,
    /// A directory whose mode each restart notes in the events.
    pub(crate) watch_dir: Option<PathBuf>,
    /// A directory set to a mode by the first restart, as the packaged unit's
    /// `StateDirectoryMode=` does.
    pub(crate) narrow_on_restart: Option<(PathBuf, u32)>,
    /// A (then empty) directory the first restart removes.
    pub(crate) remove_on_restart: Option<PathBuf>,
    /// A directory set to a mode by the next installation.
    pub(crate) narrow_on_install: Option<(PathBuf, u32)>,
}

impl Net {
    pub(crate) fn file_name(&self, commit: &str) -> String {
        let ext = self.format.asset().extension();
        format!("kbf-{commit}-linux-{}.{ext}", arch_name())
    }

    /// Install the package file `path`, as the package manager does; the event records what
    /// the host looked like at that moment: whether a trial is on disk and the previous
    /// package is kept.
    pub(crate) fn install(&mut self, path: &str, args: &[String]) -> io::Result<Exec> {
        let bytes = fs::read(path).unwrap();
        let (_, version, _) = fields_of(&bytes);
        let record: serde_json::Value = fs::read_to_string(self.state_dir.join("package.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let trial = record["pending"].is_object();
        let kept = record["pending"]["from"].as_str().is_some_and(|from| {
            self.state_dir
                .join("packages")
                .join(self.file_name(from))
                .exists()
        });
        let old = args.iter().any(|a| a == "--oldpackage");
        self.events.push(format!(
            "install:{version}:trial={trial}:prev_kept={kept}:old={old}"
        ));
        if self.install_fails > 0 {
            self.install_fails -= 1;
            return exec(1, "", "dpkg: error processing archive");
        }
        self.installed = Some(version);
        if let Some((dir, mode)) = self.narrow_on_install.take() {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        }
        exec(0, "", "")
    }
}

/// The programs of the host-install side.
pub(crate) struct Sim {
    pub(crate) net: Rc<RefCell<Net>>,
    pub(crate) calls: Rc<RefCell<Vec<Cmd>>>,
}

impl Runner for Sim {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        self.calls.borrow_mut().push(cmd.clone());
        let mut net = self.net.borrow_mut();
        let name = Path::new(&cmd.program)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let last = cmd.args.last().cloned().unwrap_or_default();
        match name.as_str() {
            "kbf" if cmd.args.last().is_some_and(|a| a == "--help") => {
                net.events.push("parse-only".to_owned());
                if net.broken.contains("kbf-cannot-run") {
                    return Err(io::Error::other("cannot start the package's kbf"));
                }
                // Its parser: a word that is no flag is an unexpected argument.
                let word = cmd.args.iter().find(|a| !a.starts_with('-'));
                match (&net.kbf_refuses, word) {
                    (Some(line), _) => exec(2, "", line),
                    (None, Some(word)) => exec(
                        2,
                        "",
                        &format!("error: unexpected argument '{word}' found\n"),
                    ),
                    (None, None) => exec(0, "usage", ""),
                }
            }
            "kbf" => {
                net.events.push("kbf-version".to_owned());
                match &net.kbf_commit {
                    Some(commit) => exec(
                        0,
                        &format!(
                            "{{\"commit\":\"{}\",\"version\":\"0.1.0\"}}\n",
                            &commit[..12]
                        ),
                        "",
                    ),
                    None => exec(1, "", "no"),
                }
            }
            "systemctl" => match cmd.args[0].as_str() {
                "restart" => {
                    net.events.push(format!("restart:{last}"));
                    if let Some(dir) = &net.watch_dir {
                        let mode = fs::metadata(dir).unwrap().mode() & 0o7777;
                        net.events.push(format!("dir-mode-at-restart:{mode:o}"));
                    }
                    if let Some((dir, mode)) = net.narrow_on_restart.take() {
                        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
                    }
                    if let Some(dir) = net.remove_on_restart.take() {
                        fs::remove_dir(dir).unwrap();
                    }
                    if net.restart_fails_once.remove(&last) {
                        exec(5, "", "failed once")
                    } else if net.restart_never.contains(&last) {
                        exec(5, "", "failed")
                    } else {
                        exec(0, "", "")
                    }
                }
                "daemon-reload" => {
                    net.events.push("reload".to_owned());
                    if net.reload_fails > 0 {
                        net.reload_fails -= 1;
                        exec(1, "", "failed")
                    } else {
                        exec(0, "", "")
                    }
                }
                "show" => {
                    if net.broken.contains("no-show") {
                        return Err(io::Error::other("cannot start systemctl"));
                    }
                    let old = net.watch.as_ref().is_some_and(|p| p.exists());
                    net.events.push(format!("show:old_layout={old}"));
                    exec(0, &net.main_pid, "")
                }
                _ => {
                    net.events.push(format!("active:{last}"));
                    match net.active_script.pop_front() {
                        Some((code, said)) => exec(code, &said, ""),
                        None => exec(0, "active\n", ""),
                    }
                }
            },
            "curl" => {
                let page: usize = last.rsplit("page=").next().unwrap().parse().unwrap();
                let body = if page == 1 {
                    format!("[{}]", net.releases.join(","))
                } else {
                    "[]".to_owned()
                };
                exec(
                    0,
                    &format!("HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{body}"),
                    "",
                )
            }
            "gh" => {
                if net.broken.contains("download-fails") {
                    return exec(1, "", "error connecting");
                }
                net.events.push("download".to_owned());
                let patterns: Vec<String> = cmd
                    .args
                    .windows(2)
                    .filter(|w| w[0] == "--pattern")
                    .map(|w| w[1].clone())
                    .collect();
                let dir = PathBuf::from(
                    cmd.args
                        .windows(2)
                        .find(|w| w[0] == "--dir")
                        .map(|w| w[1].clone())
                        .unwrap(),
                );
                let Some(files) = net.assets.get(&cmd.args[2]) else {
                    return exec(1, "", "release not found");
                };
                for asset in patterns {
                    let Some(bytes) = files.get(&asset) else {
                        return exec(1, "", "no assets match");
                    };
                    if net
                        .skip_files
                        .iter()
                        .any(|end| asset.ends_with(end.as_str()))
                    {
                        continue;
                    }
                    fs::write(dir.join(&asset), bytes).unwrap();
                }
                exec(0, "", "")
            }
            "dpkg-query" | "rpm"
                if net.broken.contains("no-database")
                    && !cmd
                        .args
                        .iter()
                        .any(|a| a == "--package" || a == "--upgrade") =>
            {
                Err(io::Error::other("cannot start the package database tool"))
            }
            "dpkg" | "rpm"
                if net.broken.contains("no-install-tool")
                    && !cmd.args.iter().any(|a| a == "--query") =>
            {
                Err(io::Error::other("cannot start the installer"))
            }
            "dpkg-deb" | "rpm"
                if net.broken.contains("no-inspect-tool")
                    && cmd.args.iter().any(|a| a == "--package" || a == "--field") =>
            {
                Err(io::Error::other("cannot start the inspector"))
            }
            "dpkg-deb" | "rpm"
                if net.broken.contains("inspect-fails")
                    && cmd.args.iter().any(|a| a == "--package" || a == "--field") =>
            {
                exec(2, "", "not a package")
            }
            "dpkg-query" => {
                net.events.push("query".to_owned());
                match &net.installed {
                    Some(v) => exec(0, &format!("ii  {v}\n"), ""),
                    None => exec(1, "", "no packages found matching kbf"),
                }
            }
            "dpkg-deb" if cmd.args[0] == "--extract" => {
                net.events.push("unpack".to_owned());
                if net.broken.contains("unpack-fails") {
                    return exec(2, "", "dpkg-deb: error: not a package");
                }
                let into = PathBuf::from(&cmd.args[2]);
                let bin = into.join("usr/bin");
                let units = into.join("usr/lib/systemd/system");
                fs::create_dir_all(&bin).unwrap();
                fs::create_dir_all(&units).unwrap();
                fs::write(bin.join("kbf"), b"the package's kbf").unwrap();
                fs::set_permissions(bin.join("kbf"), fs::Permissions::from_mode(0o755)).unwrap();
                fs::write(
                    units.join("kbf-storage-and-daemon.service"),
                    "[Service]\nUser=kbf\nStateDirectory=kbf\nStateDirectoryMode=0750\n",
                )
                .unwrap();
                exec(0, "", "")
            }
            "dpkg-deb" => {
                net.events.push("inspect".to_owned());
                let (package, version, arch) = fields_of(&fs::read(&cmd.args[1]).unwrap());
                exec(
                    0,
                    &format!("Package: {package}\nVersion: {version}\nArchitecture: {arch}\n"),
                    "",
                )
            }
            "dpkg" => net.install(&last, &cmd.args),
            "rpm" if cmd.args.iter().any(|a| a == "--upgrade") => net.install(&last, &cmd.args),
            "rpm" if cmd.args.iter().any(|a| a == "--package") => {
                net.events.push("inspect".to_owned());
                let (package, version, arch) = fields_of(&fs::read(&last).unwrap());
                let pair = rpm_pair(&version);
                let (v, r) = pair.split_once(' ').unwrap();
                exec(
                    0,
                    &format!(
                        "Package: {package}\nVersion: {v}\nRelease: {r}\nArchitecture: {arch}\n"
                    ),
                    "",
                )
            }
            "rpm" => {
                net.events.push("query".to_owned());
                match &net.installed {
                    Some(v) => exec(0, &format!("{}\n", rpm_pair(v)), ""),
                    None => exec(1, "package kbf is not installed\n", ""),
                }
            }
            other => panic!("unscripted program {other}"),
        }
    }
}

/// The backend of a rig, lent to the core, which owns the one it is given.
pub(crate) struct Borrowed<'a>(&'a PackageHost);

impl Backend for Borrowed<'_> {
    fn lock_file(&self) -> PathBuf {
        self.0.lock_file()
    }
    fn node_id(&self) -> Option<String> {
        self.0.node_id()
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        self.0.current()
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        self.0.pending()
    }
    fn has(&self, commit: &str) -> bool {
        self.0.has(commit)
    }
    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        self.0.stage(version, channel)
    }
    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        self.0.switch(commit, window_secs, now)
    }
    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        self.0.confirm(pending)
    }
    fn revert(&self, pending: &Pending) -> Result<(), Fault> {
        self.0.revert(pending)
    }
}

/// A host that runs package A, with a channel `stable` that offers whatever a test offers.
pub(crate) struct Rig {
    pub(crate) dir: TestDir,
    pub(crate) root: TestDir,
    pub(crate) state_dir: PathBuf,
    pub(crate) format: Format,
    pub(crate) backend: PackageHost,
    pub(crate) net: Rc<RefCell<Net>>,
    pub(crate) calls: Rc<RefCell<Vec<Cmd>>>,
    pub(crate) installer: Rc<RefCell<Vec<Spawn>>>,
    /// How `gh attestation verify` ends.
    pub(crate) gh_exit: Rc<RefCell<i32>>,
}

impl Rig {
    pub(crate) fn new(name: &str, format: Format) -> Self {
        let root = TestDir::under(std::env::temp_dir(), &format!("hi-pkg-root-{name}"));
        let layout = Layout::new(Root::new(&*root).unwrap(), Platform::Linux);
        let dir = TestDir::under(std::env::temp_dir(), &format!("hi-pkg-host-{name}"));
        fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
        let uid = fs::metadata(&*dir).unwrap().uid();
        let state_dir = dir.join("state");
        fs::create_dir(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let host = Host::for_test(&dir, Trust::owned_by(uid, &dir));
        put(
            &dir.join("releases.toml"),
            "[channels.stable]\nsource = \"github\"\n",
        );
        put(&host.node_config, "[node]\nid = \"st1\"\n");

        let mut trust = trusted_host_on(Platform::Linux);
        let who = me();
        trust.entries.insert(
            HostPath::new(SETTINGS_PATH).unwrap(),
            Entry::File {
                perms: Perms::new(0o644),
                owner: Owner { uid: 0, gid: 0 },
                bytes: format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", who.uid, who.gid).into_bytes(),
            },
        );

        let net = Rc::new(RefCell::new(Net {
            format,
            state_dir: state_dir.clone(),
            releases: Vec::new(),
            assets: BTreeMap::new(),
            installed: Some(a_version()),
            events: Vec::new(),
            broken: BTreeSet::new(),
            restart_fails_once: BTreeSet::new(),
            install_fails: 0,
            restart_never: BTreeSet::new(),
            reload_fails: 0,
            main_pid: "4242\n".to_owned(),
            kbf_commit: None,
            watch: None,
            skip_files: Vec::new(),
            active_script: VecDeque::new(),
            kbf_refuses: None,
            watch_dir: None,
            narrow_on_restart: None,
            narrow_on_install: None,
            remove_on_restart: None,
        }));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let gh_exit = Rc::new(RefCell::new(0));
        let installer_runner = {
            let (gh_exit, net) = (Rc::clone(&gh_exit), Rc::clone(&net));
            FakeRunner::new(move |_| {
                net.borrow_mut().events.push("attest".to_owned());
                Ok(ran(*gh_exit.borrow(), "", "no matching attestation"))
            })
        };
        let installer = installer_runner.log();
        let platform = format!("linux-{}", arch_name());
        let backend = PackageHost {
            format,
            layout,
            trust: Box::new(trust),
            store: Store::new(
                state_dir.clone(),
                host.trust.clone(),
                &platform,
                format.asset().extension(),
            ),
            host,
            allowlist: dir.join("releases.toml"),
            units: vec![
                Unit::service("kbf-daemon.service").unwrap(),
                Unit::service("kbf-frontend@a.service").unwrap(),
            ],
            restart_wait: Wait {
                timeout: Duration::from_millis(40),
                interval: Duration::from_millis(1),
            },
            pace: crate::pace::fake::Fake::new().pace(),
            runner: Box::new(Sim {
                net: Rc::clone(&net),
                calls: Rc::clone(&calls),
            }),
            installer_runner: Box::new(installer_runner),
            tools: Tools {
                curl: dir.join("curl"),
                tool: Duration::from_secs(5),
                download: Duration::from_secs(5),
                tarball_max: 64 * 1024,
                image_max: 64 * 1024,
            },
        };
        let rig = Self {
            dir,
            root,
            state_dir,
            format,
            backend,
            net,
            calls,
            installer,
            gh_exit,
        };
        rig.offer(COMMIT, &new_version());
        rig.offer(A, &a_version());
        rig.offer(C, &c_version());
        rig
    }

    pub(crate) fn file_name(&self, commit: &str) -> String {
        self.net.borrow().file_name(commit)
    }

    /// The channel's GitHub offers the package of `commit`, which says it is `version`.
    pub(crate) fn offer(&self, commit: &str, version: &str) {
        self.offer_bytes(commit, &bytes_of("kbf", version, self.format_arch()));
    }

    pub(crate) fn format_arch(&self) -> &'static str {
        self.format.arch_name(Arch::current())
    }

    /// The release of `commit` holds a package file with these bytes, listed in its sums.
    pub(crate) fn offer_bytes(&self, commit: &str, bytes: &[u8]) {
        let file = self.file_name(commit);
        let tag = format!("v-{}", &commit[..8]);
        let sums = format!("{}  {file}\n", sha256(bytes));
        let assets = format!(
            "{{\"name\":\"{file}\",\"size\":{}}},{{\"name\":\"{file}.sigstore.json\",\"size\":30}},{{\"name\":\"SHA256SUMS\",\"size\":{}}}",
            bytes.len(),
            sums.len()
        );
        let release = format!("{{\"tag_name\":\"{tag}\",\"draft\":false,\"assets\":[{assets}]}}");
        let mut net = self.net.borrow_mut();
        net.releases.retain(|r| !r.contains(&format!("\"{tag}\"")));
        net.releases.push(release);
        let files = net.assets.entry(tag).or_default();
        files.insert(file.clone(), bytes.to_vec());
        files.insert(
            format!("{file}.sigstore.json"),
            b"{\"mediaType\":\"bundle\"}\n".to_vec(),
        );
        files.insert("SHA256SUMS".to_owned(), sums.into_bytes());
    }

    pub(crate) fn set_sums(&self, commit: &str, text: &str) {
        let tag = format!("v-{}", &commit[..8]);
        self.net
            .borrow_mut()
            .assets
            .get_mut(&tag)
            .unwrap()
            .insert("SHA256SUMS".to_owned(), text.as_bytes().to_vec());
    }

    pub(crate) fn events(&self) -> Vec<String> {
        self.net.borrow().events.clone()
    }

    pub(crate) fn installs(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|e| e.starts_with("install:"))
            .collect()
    }

    pub(crate) fn count(&self, event: &str) -> usize {
        self.events().iter().filter(|e| *e == event).count()
    }

    pub(crate) fn installed(&self) -> Option<String> {
        self.net.borrow().installed.clone()
    }

    pub(crate) fn restarts(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|e| e.starts_with("restart:"))
            .collect()
    }

    pub(crate) fn record(&self) -> Record {
        self.backend.store.load().unwrap()
    }

    pub(crate) fn kept_files(&self) -> BTreeSet<String> {
        fs::read_dir(self.state_dir.join("packages"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn kept(&self, commit: &str) -> PathBuf {
        self.state_dir.join("packages").join(self.file_name(commit))
    }

    pub(crate) fn work(&self) -> Vec<String> {
        fs::read_dir(self.root.join("var/lib/kbf-installer/work"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn stage(&self, text: &str) -> Result<String, Fault> {
        self.backend.stage(&version(text), &stable())
    }

    pub(crate) fn ab(&self, clock: Rc<std::cell::Cell<u64>>) -> Ab<Borrowed<'_>> {
        let uid = fs::metadata(&*self.dir).unwrap().uid();
        Ab::new(
            Borrowed(&self.backend),
            self.dir.join("state.json"),
            Trust::owned_by(uid, &self.dir),
            600,
            Box::new(move || clock.get()),
        )
    }

    /// Nothing was installed, restarted, kept or recorded.
    pub(crate) fn assert_nothing_changed(&self) {
        let events = self.events();
        assert!(
            !events
                .iter()
                .any(|e| e.starts_with("install:") || e.starts_with("restart:")),
            "something ran: {events:?}"
        );
        assert_eq!(self.installed(), Some(a_version()));
        assert!(self.kept_files().is_empty(), "{:?}", self.kept_files());
        assert_eq!(self.record(), Record::default());
        assert!(self.work().is_empty(), "scratch left: {:?}", self.work());
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&*self.root);
    }
}

pub(crate) fn both(test: impl Fn(Format)) {
    test(Format::Deb);
    test(Format::Rpm);
}

pub(crate) fn refused(code: Code) -> impl Fn(&Fault) -> bool {
    move |fault| fault.refused && fault.code == code
}

/// The package is fetched as the unprivileged user (the file, its bundle and the release's
/// sums), checked offline by `gh attestation verify` for the full commit, the farm's
/// workflow on hosted runners and the channel's branch, read by the package manager, and kept
/// root's alone with its digest. The package that runs, the units and the work directory are
/// as they were.
#[test]
fn a_package_is_fetched_verified_offline_and_kept_and_nothing_else_changes() {
    both(|format| {
        let rig = Rig::new("stage", format);
        assert_eq!(rig.stage(&new_version()).unwrap(), COMMIT);

        let verify: Vec<String> = rig.installer.borrow()[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has =
            |flag: &str, value: &str| verify.windows(2).any(|w| w[0] == flag && w[1] == value);
        assert!(has("--source-digest", COMMIT));
        assert!(has(
            "--signer-workflow",
            "komira-ai/kbf/.github/workflows/artifacts.yml"
        ));
        assert!(has("--source-ref", "refs/heads/main"));
        assert!(has("--repo", "komira-ai/kbf"));
        assert!(verify.contains(&"--deny-self-hosted-runners".to_owned()));
        assert_eq!(rig.installer.borrow().len(), 1);

        let download = rig
            .calls
            .borrow()
            .iter()
            .find(|c| c.args.first().map(String::as_str) == Some("release"))
            .cloned()
            .unwrap();
        assert!(download.args.contains(&rig.file_name(COMMIT)));
        assert!(
            download
                .args
                .contains(&format!("{}.sigstore.json", rig.file_name(COMMIT)))
        );
        assert!(download.args.contains(&"SHA256SUMS".to_owned()));
        let who = me();
        assert_eq!(
            download.run_as.map(|i| (i.uid, i.gid)),
            Some((who.uid, who.gid)),
            "fetched as the unprivileged user"
        );
        assert!(!download.env.iter().any(|(k, _)| k == "GH_TOKEN"));

        let bytes = bytes_of("kbf", &new_version(), rig.format_arch());
        assert_eq!(fs::read(rig.kept(COMMIT)).unwrap(), bytes);
        assert_eq!(
            fs::metadata(rig.kept(COMMIT)).unwrap().mode() & 0o777,
            0o600
        );
        let record = rig.record();
        let kept = &record.files[COMMIT];
        assert_eq!(kept.version, new_version());
        assert_eq!(kept.sha256, sha256(&bytes));
        assert_eq!(kept.channel, "stable");
        assert_eq!(record.pending, None);

        assert_eq!(rig.installed(), Some(a_version()));
        assert!(rig.installs().is_empty() && rig.restarts().is_empty());
        assert!(
            rig.work().is_empty(),
            "the scratch is gone: {:?}",
            rig.work()
        );
    });
}

/// A package that fails any check is never kept and nothing changes: not the package that runs,
/// not a unit, not the records. The checks are the sums, the attestation, and what the package
/// manager reads from the file: its name, its version and its CPU.
#[test]
fn an_unverified_or_wrong_package_changes_nothing() {
    type Break = Box<dyn Fn(&Rig)>;
    let cases: Vec<(&str, Break, Code)> = vec![
        (
            "the-attestation-fails",
            Box::new(|rig| *rig.gh_exit.borrow_mut() = 1),
            Code::VerifyFailed,
        ),
        (
            "the-sums-list-other-bytes",
            Box::new(|rig| {
                rig.set_sums(
                    COMMIT,
                    &format!("{}  {}\n", sha256(b"other"), rig.file_name(COMMIT)),
                )
            }),
            Code::VerifyFailed,
        ),
        (
            "the-sums-do-not-list-the-package",
            Box::new(|rig| rig.set_sums(COMMIT, &format!("{}  elsewhere\n", sha256(b"x")))),
            Code::VerifyFailed,
        ),
        (
            "the-sums-are-malformed",
            Box::new(|rig| rig.set_sums(COMMIT, "not a sums file")),
            Code::VerifyFailed,
        ),
        (
            "another-version",
            Box::new(|rig| {
                rig.offer_bytes(
                    COMMIT,
                    &bytes_of("kbf", &build(COMMIT, 7, 99), rig.format_arch()),
                );
            }),
            Code::VerifyFailed,
        ),
        (
            "another-package",
            Box::new(|rig| {
                rig.offer_bytes(
                    COMMIT,
                    &bytes_of("kbf-installer", &new_version(), rig.format_arch()),
                );
            }),
            Code::VerifyFailed,
        ),
        (
            "another-cpu",
            Box::new(|rig| {
                rig.offer_bytes(COMMIT, &bytes_of("kbf", &new_version(), "s390x"));
            }),
            Code::VerifyFailed,
        ),
        (
            "no-such-release",
            Box::new(|rig| rig.net.borrow_mut().releases.clear()),
            Code::UnknownRelease,
        ),
        (
            "the-download-fails",
            Box::new(|rig| {
                rig.net.borrow_mut().broken.insert("download-fails");
            }),
            Code::FetchFailed,
        ),
        (
            "the-package-is-larger-than-the-limit",
            Box::new(|rig| {
                rig.offer_bytes(COMMIT, &vec![b'x'; 70 * 1024]);
            }),
            Code::FetchFailed,
        ),
    ];
    for (name, break_it, code) in cases {
        both(|format| {
            let rig = Rig::new(&format!("bad-{name}"), format);
            break_it(&rig);
            let fault = rig.stage(&new_version()).unwrap_err();
            assert!(fault.refused && fault.code == code, "{name}: {fault:?}");
            rig.assert_nothing_changed();
        });
    }
}

/// A package whose bytes are not the ones the sums list, listed under another file's digest
/// or cut short, is not kept, and a release with no sums at all is not fetched from.
#[test]
fn a_release_without_sums_or_bundle_is_not_taken() {
    both(|format| {
        let rig = Rig::new("no-sums", format);
        let tag = format!("v-{}", &COMMIT[..8]);
        let listing = rig.net.borrow().releases[0].clone();
        let without = |name: &str| {
            let at = listing.find(&format!("{{\"name\":\"{name}")).unwrap();
            let end = listing[at..].find('}').unwrap() + at + 1;
            let mut cut = listing.clone();
            cut.replace_range(at..end, "{\"name\":\"unrelated\",\"size\":1}");
            cut
        };
        for gone in [
            "SHA256SUMS",
            &format!("{}.sigstore.json", rig.file_name(COMMIT)),
        ] {
            rig.net.borrow_mut().releases[0] = without(gone);
            let fault = rig.stage(&new_version()).unwrap_err();
            assert_eq!(fault.code, Code::FetchFailed, "{gone}: {fault:?}");
            rig.assert_nothing_changed();
        }
        // The files themselves missing from the release.
        rig.net.borrow_mut().releases[0] = listing;
        rig.net
            .borrow_mut()
            .assets
            .get_mut(&tag)
            .unwrap()
            .remove("SHA256SUMS");
        assert_eq!(
            rig.stage(&new_version()).unwrap_err().code,
            Code::FetchFailed
        );
        rig.assert_nothing_changed();
    });
}

/// A deb or rpm of a channel for a trusted repository is listed from, downloaded from and
/// attested for that repository and its workflow, once; the same channel without the
/// repository's listing is refused before anything is fetched.
#[test]
fn a_channel_of_another_repository_signs_its_packages_by_its_own_workflow() {
    both(|format| {
        let channel = |trusted: &str| {
            format!(
                "{trusted}[channels.stable]\nsource = \"github\"\nrepository = \"owner/other\"\nsigner_workflow = \"owner/other/.github/workflows/release.yml\"\n"
            )
        };
        let rig = Rig::new("other-repo-refused", format);
        put(&rig.dir.join("releases.toml"), channel(""));
        let fault = rig.stage(&new_version()).unwrap_err();
        assert!(refused(Code::ChannelNotAllowed)(&fault), "{fault:?}");
        assert!(fault.detail.contains("owner/other"), "{}", fault.detail);
        assert!(rig.calls.borrow().is_empty());
        assert!(rig.installer.borrow().is_empty());

        let rig = Rig::new("other-repo", format);
        put(
            &rig.dir.join("releases.toml"),
            channel("trusted_workflows = [\"owner/other/.github/workflows/release.yml\"]\n"),
        );
        rig.stage(&new_version()).unwrap();
        let calls = rig.calls.borrow();
        let listing = calls.iter().find(|c| c.program.ends_with("curl")).unwrap();
        assert!(
            listing
                .args
                .last()
                .unwrap()
                .starts_with("https://api.github.com/repos/owner/other/releases?")
        );
        let download = calls
            .iter()
            .find(|c| c.args.first().map(String::as_str) == Some("release"))
            .unwrap();
        let at = download.args.iter().position(|a| a == "--repo").unwrap();
        assert_eq!(download.args[at + 1], "owner/other");
        assert!(download.args.contains(&rig.file_name(COMMIT)));

        assert_eq!(rig.installer.borrow().len(), 1);
        let verify: Vec<String> = rig.installer.borrow()[0]
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has =
            |flag: &str, value: &str| verify.windows(2).any(|w| w[0] == flag && w[1] == value);
        assert!(has("--repo", "owner/other"));
        assert!(has(
            "--signer-workflow",
            "owner/other/.github/workflows/release.yml"
        ));
        assert!(has("--source-digest", COMMIT));
        assert!(verify.contains(&"--deny-self-hosted-runners".to_owned()));
    });
}

/// The channel's token reaches the download as `GH_TOKEN` and no argument; a version without a
/// commit and a channel the allowlist does not name are refused before any fetch.
#[test]
fn what_the_allowlist_and_the_version_do_not_allow_is_refused_before_a_fetch() {
    both(|format| {
        let rig = Rig::new("allow", format);
        let token = rig.dir.join("token");
        put(&token, "ghp_secret-token\n");
        put(
            &rig.dir.join("releases.toml"),
            format!(
                "[channels.stable]\nsource = \"github\"\ntoken_file = \"{}\"\n",
                token.display()
            ),
        );
        rig.stage(&new_version()).unwrap();
        let download = rig
            .calls
            .borrow()
            .iter()
            .find(|c| c.args.first().map(String::as_str) == Some("release"))
            .cloned()
            .unwrap();
        assert!(
            download
                .env
                .contains(&("GH_TOKEN".to_owned(), "ghp_secret-token".to_owned()))
        );
        assert!(!download.args.iter().any(|a| a.contains("ghp_")));

        let rig = Rig::new("allow2", format);
        let cases: Vec<(&str, Code)> = vec![("1.2.3-1", Code::UnknownRelease)];
        for (text, code) in cases {
            let fault = rig.stage(text).unwrap_err();
            assert!(fault.refused && fault.code == code, "{text}: {fault:?}");
        }
        put(
            &rig.dir.join("releases.toml"),
            "[channels.other]\nsource = \"github\"\n",
        );
        assert!(refused(Code::ChannelNotAllowed)(
            &rig.stage(&new_version()).unwrap_err()
        ));
        put(
            &rig.dir.join("releases.toml"),
            "[channels.stable]\ndir = \"/srv/releases\"\n",
        );
        assert!(refused(Code::ConfigInvalid)(
            &rig.stage(&new_version()).unwrap_err()
        ));
        fs::remove_file(rig.dir.join("releases.toml")).unwrap();
        assert!(refused(Code::ConfigInvalid)(
            &rig.stage(&new_version()).unwrap_err()
        ));
        put(
            &rig.dir.join("releases.toml"),
            "[channels.stable]\nsource = \"github\"\nsource_ref = \"refs/heads/other\"\n",
        );
        assert!(refused(Code::ConfigInvalid)(
            &rig.stage(&new_version()).unwrap_err()
        ));
        put(
            &rig.dir.join("releases.toml"),
            "[channels.stable]\nsource = \"github\"\ntoken_file = \"/nonexistent/token\"\n",
        );
        assert!(rig.stage(&new_version()).unwrap_err().refused);
        assert_eq!(rig.count("download"), 0, "nothing was fetched");
        rig.assert_nothing_changed();
    });
}

/// The pinned `gh` and the trusted root must be root's: with others' they are not believed and
/// nothing is fetched.
#[test]
fn untrusted_checking_tools_stop_the_fetch() {
    both(|format| {
        let mut rig = Rig::new("untrusted", format);
        let mut trust = trusted_host_on(Platform::Linux);
        let who = me();
        trust.entries.insert(
            HostPath::new(SETTINGS_PATH).unwrap(),
            Entry::File {
                perms: Perms::new(0o644),
                owner: Owner { uid: 0, gid: 0 },
                bytes: format!("RUN_AS_UID={}\nRUN_AS_GID={}\n", who.uid, who.gid).into_bytes(),
            },
        );
        trust.entries.insert(
            HostPath::new(kbf_installer::fixtures::GH_PATH).unwrap(),
            Entry::File {
                perms: Perms::new(0o777),
                owner: Owner { uid: 0, gid: 0 },
                bytes: Vec::new(),
            },
        );
        rig.backend.trust = Box::new(trust);
        let fault = rig.stage(&new_version()).unwrap_err();
        assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
        assert_eq!(rig.count("download"), 0);
        rig.assert_nothing_changed();
    });
}

/// One update, through the core: the order is verify, record the package that runs, install,
/// restart, confirm. At the moment the package manager installs the new file, the trial is on
/// disk and a verified copy of the previous package is kept (it is fetched and checked like any
/// other, because this host held none); the units restart after it, one by one; a confirm keeps
/// the new package.
#[test]
fn the_order_is_verify_record_previous_install_restart_confirm() {
    both(|format| {
        let rig = Rig::new("order", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(Rc::clone(&now));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let v = new_version();

        assert!(matches!(
            ask(&format!("install kbf={v} from stable")),
            Answer::Staged { current: false, .. }
        ));
        assert!(
            rig.installs().is_empty(),
            "an install verb installs nothing"
        );
        assert!(matches!(
            ask(&format!("switch kbf={v}")),
            Answer::Switched {
                deadline_unix: 1_600,
                ..
            }
        ));
        let marks: Vec<String> = rig
            .events()
            .into_iter()
            .filter(|e| e == "attest" || e.starts_with("install:") || e.starts_with("restart:"))
            .collect();
        assert_eq!(
            marks,
            [
                "attest".to_owned(),
                "attest".to_owned(),
                format!(
                    "install:{v}:trial=true:prev_kept=true:old={}",
                    format == Format::Rpm
                ),
                "restart:kbf-daemon.service".to_owned(),
                "restart:kbf-frontend@a.service".to_owned(),
            ]
        );
        assert_eq!(rig.installed(), Some(v.clone()));
        let trial = rig.record().pending.unwrap();
        assert_eq!((trial.commit.as_str(), trial.from.as_str()), (COMMIT, A));
        assert_eq!(trial.deadline_unix, 1_600);
        assert!(rig.record().files.contains_key(A));

        assert!(matches!(
            ask(&format!("confirm kbf={v}")),
            Answer::Confirmed { .. }
        ));
        assert_eq!(rig.record().pending, None);
        let Answer::Queried(view) = ask("query") else {
            panic!("not a query")
        };
        assert_eq!(view.commit.as_deref(), Some(COMMIT));
        assert_eq!(view.version.as_deref(), Some(v.as_str()));
        assert_eq!(view.node_id.as_deref(), Some("st1"));
        assert!(view.pending.is_none());
        assert_eq!(view.last.unwrap().outcome, "succeeded");
    });
}

/// An update that is not confirmed by its deadline puts the recorded previous package back:
/// the package manager installs the kept file of it, the units restart, and the trial is over.
/// A host whose agent restarted finds the trial on disk and does the same.
#[test]
fn the_deadline_reinstalls_the_previous_package_and_a_restart_finds_the_trial() {
    both(|format| {
        let rig = Rig::new("deadline", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(Rc::clone(&now));
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let v = new_version();
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        assert_eq!(rig.installed(), Some(v.clone()));

        now.set(1_599);
        assert_eq!(ab.tick(), None);
        assert_eq!(rig.installed(), Some(v.clone()));
        let restarts_before = rig.restarts().len();

        // The agent and the host went down; a new agent finds the trial past its deadline.
        drop(ab);
        let now = Rc::new(std::cell::Cell::new(1_600_u64));
        let ab = rig.ab(now);
        assert!(matches!(ab.tick(), Some(Answer::Reverted { .. })));
        assert_eq!(rig.installed(), Some(a_version()));
        assert_eq!(rig.record().pending, None);
        let installs = rig.installs();
        assert_eq!(installs.len(), 2, "{installs:?}");
        assert!(installs[1].starts_with(&format!("install:{}:", a_version())));
        assert_eq!(
            rig.restarts().len(),
            restarts_before + 2,
            "both units restart"
        );
        let Answer::Queried(view) = ab.handle(&LocalRequest::Query) else {
            panic!("not a query")
        };
        assert_eq!(view.last.unwrap().reason, Some("timeout"));
        assert_eq!(rig.kept_files(), BTreeSet::from([rig.file_name(A)]));
        // Nothing left to time out.
        assert_eq!(ab.tick(), None);
    });
}

/// A rollback is an installation of an older version and is never refused: dpkg downgrades,
/// and rpm is told `--oldpackage`. The older package this host kept is not fetched again.
#[test]
fn an_older_version_installs_and_a_kept_copy_is_not_fetched_again() {
    both(|format| {
        let rig = Rig::new("older", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(now);
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let (a, v) = (a_version(), new_version());
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        ask(&format!("confirm kbf={v}"));
        let downloads = rig.count("download");

        // Back to the older one, through the same verbs.
        assert!(matches!(
            ask(&format!("install kbf={a} from stable")),
            Answer::Staged { current: false, .. }
        ));
        assert_eq!(rig.count("download"), downloads, "the kept copy is used");
        assert!(matches!(
            ask(&format!("switch kbf={a}")),
            Answer::Switched { .. }
        ));
        assert_eq!(rig.installed(), Some(a.clone()));
        let last = rig.installs().pop().unwrap();
        assert!(
            last.starts_with(&format!("install:{a}:trial=true")),
            "{last}"
        );
        let flagged = rig
            .calls
            .borrow()
            .iter()
            .filter(|c| c.program.ends_with("rpm") && c.args.iter().any(|x| x == "--upgrade"))
            .all(|c| c.args.iter().any(|x| x == "--oldpackage"));
        assert!(flagged);
        assert!(matches!(
            ask(&format!("confirm kbf={a}")),
            Answer::Confirmed { .. }
        ));
    });
}

/// A unit that does not come up on the new package puts the previous one back at once, not at
/// the deadline: the fault says the switch failed, the host runs the package it ran, the units
/// were restarted on it, and no trial is left.
#[test]
fn a_package_whose_unit_does_not_come_up_is_replaced_by_the_previous_at_once() {
    both(|format| {
        let rig = Rig::new("unit", format);
        rig.stage(&new_version()).unwrap();
        rig.net
            .borrow_mut()
            .restart_fails_once
            .insert("kbf-frontend@a.service".to_owned());
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(
            !fault.refused && fault.code == Code::SwitchFailed,
            "{fault:?}"
        );
        assert_eq!(rig.installed(), Some(a_version()));
        assert_eq!(rig.record().pending, None);
        assert_eq!(
            rig.installs()
                .iter()
                .map(|e| e.split(':').nth(1).unwrap().to_owned())
                .collect::<Vec<_>>(),
            [new_version(), a_version()]
        );
        // daemon, frontend (fails), then both again on the previous package.
        assert_eq!(rig.restarts().len(), 4);
    });
}

/// An installation that fails goes back the same way. When going back fails too, the trial
/// stays on disk, so the next try finds it, and the next try is the one that works.
#[test]
fn a_failed_install_goes_back_and_a_failed_going_back_keeps_the_trial() {
    both(|format| {
        let rig = Rig::new("install-fails", format);
        rig.stage(&new_version()).unwrap();
        rig.net.borrow_mut().install_fails = 1;
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert_eq!(fault.code, Code::SwitchFailed, "{fault:?}");
        assert_eq!(rig.installed(), Some(a_version()));
        assert_eq!(rig.record().pending, None);

        rig.stage(&new_version()).unwrap();
        rig.net.borrow_mut().install_fails = 2;
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
        assert!(fault.detail.contains("going back"));
        assert!(rig.record().pending.is_some(), "the trial is still there");

        let pending = rig.backend.pending().unwrap().unwrap();
        assert_eq!(pending.from, A);
        rig.backend.revert(&pending).unwrap();
        assert_eq!(rig.installed(), Some(a_version()));
        assert_eq!(rig.record().pending, None);
    });
}

/// A going back whose units do not restart still ends the trial (the package is back) and says
/// the restart failed; a package that cannot be read from the database is a failure, not a
/// guess.
#[test]
fn a_revert_that_cannot_restart_ends_the_trial_and_says_so() {
    both(|format| {
        let rig = Rig::new("revert-restart", format);
        rig.stage(&new_version()).unwrap();
        rig.backend.switch(COMMIT, 600, 1_000).unwrap();
        rig.net
            .borrow_mut()
            .restart_fails_once
            .insert("kbf-daemon.service".to_owned());
        let pending = rig.backend.pending().unwrap().unwrap();
        let fault = rig.backend.revert(&pending).unwrap_err();
        assert_eq!(fault.code, Code::RevertFailed);
        assert!(fault.detail.contains("back on the package that ran"));
        assert_eq!(rig.installed(), Some(a_version()));
        assert_eq!(rig.record().pending, None);
    });
}

/// A kept file that is not the file that was verified is never installed: the switch is
/// refused with nothing installed and no trial written.
#[test]
fn a_kept_file_that_changed_is_never_installed() {
    both(|format| {
        let rig = Rig::new("changed", format);
        rig.stage(&new_version()).unwrap();
        fs::write(rig.kept(COMMIT), b"kbf|9.9.9|other").unwrap();
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
        assert!(rig.installs().is_empty() && rig.restarts().is_empty());
        assert_eq!(rig.record().pending, None);
        assert_eq!(rig.installed(), Some(a_version()));

        fs::remove_file(rig.kept(COMMIT)).unwrap();
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(refused(Code::VerifyFailed)(&fault), "{fault:?}");
        assert!(!rig.backend.has(COMMIT));
    });
}

/// The kept copy of the previous package is gone when the revert needs it: it is fetched and
/// checked again, the same way, and then installed.
#[test]
fn a_lost_copy_of_the_previous_package_is_fetched_again_for_the_revert() {
    both(|format| {
        let rig = Rig::new("lost", format);
        rig.stage(&new_version()).unwrap();
        rig.backend.switch(COMMIT, 600, 1_000).unwrap();
        fs::remove_file(rig.kept(A)).unwrap();
        let downloads = rig.count("download");
        let pending = rig.backend.pending().unwrap().unwrap();
        rig.backend.revert(&pending).unwrap();
        assert_eq!(rig.count("download"), downloads + 1);
        assert_eq!(rig.installed(), Some(a_version()));

        // Changed instead of gone.
        rig.stage(&new_version()).unwrap();
        rig.backend.switch(COMMIT, 600, 1_000).unwrap();
        fs::write(rig.kept(A), b"kbf|0.0.1|x").unwrap();
        let pending = rig.backend.pending().unwrap().unwrap();
        rig.backend.revert(&pending).unwrap();
        assert_eq!(rig.installed(), Some(a_version()));

        // And when it cannot be fetched either, nothing is installed and the trial stays.
        rig.stage(&new_version()).unwrap();
        rig.backend.switch(COMMIT, 600, 1_000).unwrap();
        fs::remove_file(rig.kept(A)).unwrap();
        rig.net.borrow_mut().broken.insert("download-fails");
        let installs = rig.installs().len();
        let pending = rig.backend.pending().unwrap().unwrap();
        let fault = rig.backend.revert(&pending).unwrap_err();
        assert_eq!(fault.code, Code::RevertFailed);
        assert_eq!(rig.installs().len(), installs);
        assert!(rig.record().pending.is_some());
    });
}

/// A confirm keeps the new package and the one before it, and forgets older files; a revert
/// keeps the package that is back and nothing else.
#[test]
fn a_confirm_forgets_files_older_than_the_previous_one_and_a_revert_keeps_one() {
    both(|format| {
        let rig = Rig::new("prune", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(now);
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        for (text, kept, held) in [
            (new_version(), [COMMIT, A], 2),
            (c_version(), [C, COMMIT], 3),
        ] {
            ask(&format!("install kbf={text} from stable"));
            ask(&format!("switch kbf={text}"));
            assert_eq!(rig.kept_files().len(), held);
            ask(&format!("confirm kbf={text}"));
            let want: BTreeSet<String> = kept.iter().map(|c| rig.file_name(c)).collect();
            assert_eq!(rig.kept_files(), want);
            assert_eq!(
                rig.record().files.keys().cloned().collect::<Vec<_>>().len(),
                2
            );
        }
        let pending_free = rig.record();
        assert_eq!(pending_free.pending, None);

        ask(&format!("install kbf={} from stable", a_version()));
        ask(&format!("switch kbf={}", a_version()));
        assert!(matches!(ask("revert"), Answer::Reverted { .. }));
        assert_eq!(rig.kept_files(), BTreeSet::from([rig.file_name(C)]));
        assert_eq!(rig.installed(), Some(c_version()));
    });
}

/// The switch says no, with nothing changed, when the package asked for is not kept, is the one
/// that runs, no package is installed to go back to, or the running package names no build (so
/// no copy of it can be fetched).
#[test]
fn a_switch_that_cannot_go_back_is_refused_before_anything_runs() {
    both(|format| {
        let rig = Rig::new("refusals", format);
        let nothing_ran = |rig: &Rig| {
            assert!(rig.installs().is_empty() && rig.restarts().is_empty());
            assert_eq!(rig.record().pending, None);
        };
        // Not kept.
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(refused(Code::NotStaged)(&fault), "{fault:?}");
        nothing_ran(&rig);

        rig.stage(&new_version()).unwrap();
        // It is the one that runs.
        rig.net.borrow_mut().installed = Some(new_version());
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(refused(Code::NotStaged)(&fault), "{fault:?}");
        nothing_ran(&rig);
        // Nothing is installed.
        rig.net.borrow_mut().installed = None;
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert!(refused(Code::NoCurrent)(&fault), "{fault:?}");
        assert_eq!(rig.backend.current().unwrap(), None);
        nothing_ran(&rig);
        // A package that names no build, or whose version is not one this program reads (an
        // rpm's release always names its build).
        let odd_versions: &[&str] = if format == Format::Deb {
            &["1.0-1", "1:2.0-1"]
        } else {
            &[]
        };
        for odd in odd_versions {
            rig.net.borrow_mut().installed = Some((*odd).to_owned());
            let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
            assert!(refused(Code::NoCurrent)(&fault), "{odd}: {fault:?}");
            nothing_ran(&rig);
        }
    });
}

/// `current` is the commit when this host kept the package, else the 12 digits the version
/// carries, else the version text itself.
#[test]
fn the_current_release_is_named_as_well_as_the_host_can() {
    both(|format| {
        let rig = Rig::new("current", format);
        assert_eq!(rig.backend.current().unwrap().as_deref(), Some(&A[..12]));
        rig.stage(&a_version()).unwrap();
        assert_eq!(rig.backend.current().unwrap().as_deref(), Some(A));
        if format == Format::Deb {
            rig.net.borrow_mut().installed = Some("1.0-1".to_owned());
            assert_eq!(rig.backend.current().unwrap().as_deref(), Some("1.0-1"));
        }
    });
}

/// A package manager that cannot be asked, or that prints nonsense, is a failure of the
/// records, not a guess; so is a record that is not root's or does not parse.
#[test]
fn an_unreadable_database_or_record_is_unavailable() {
    both(|format| {
        let rig = Rig::new("unavailable", format);
        // A database that prints no version, and one that cannot be asked.
        rig.net.borrow_mut().installed = Some(String::new());
        assert_eq!(
            rig.backend.current().unwrap_err().code,
            Code::StateUnavailable
        );
        rig.net.borrow_mut().installed = Some(a_version());
        rig.net.borrow_mut().broken.insert("no-database");
        assert_eq!(
            rig.backend.current().unwrap_err().code,
            Code::StateUnavailable
        );
        rig.net.borrow_mut().broken.remove("no-database");
        rig.net.borrow_mut().installed = Some(a_version());
        fs::write(rig.state_dir.join("package.json"), "{not json").unwrap();
        fs::set_permissions(
            rig.state_dir.join("package.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        for fault in [
            rig.backend.pending().unwrap_err(),
            rig.backend.current().unwrap_err(),
            rig.stage(&new_version()).unwrap_err(),
        ] {
            assert_eq!(fault.code, Code::StateUnavailable, "{fault:?}");
        }
        assert!(!rig.backend.has(COMMIT));
        fs::write(rig.state_dir.join("package.json"), "{}").unwrap();
        fs::set_permissions(
            rig.state_dir.join("package.json"),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert_eq!(
            rig.backend.pending().unwrap_err().code,
            Code::StateUnavailable
        );
    });
}

/// The host reports its node and its lock as the other layout does.
#[test]
fn the_host_reports_its_node_and_its_lock() {
    let rig = Rig::new("host", Format::Deb);
    assert_eq!(rig.backend.node_id().as_deref(), Some("st1"));
    assert_eq!(rig.backend.lock_file(), rig.dir.join("install.lock"));
}

/// The package host as the program builds it: the layout's trust files, the allowlist of the
/// channels, the units, a store in the state directory and the real tools.
#[test]
fn the_production_host_is_the_package_layout_and_the_programs_of_a_stage() {
    let host = super::package::production(
        vec![Unit::service("kbf-daemon.service").unwrap()],
        Format::Rpm,
        Path::new("/var/lib/kbf-host-install"),
    );
    assert_eq!(host.format, Format::Rpm);
    assert_eq!(
        host.allowlist,
        Path::new("/etc/kbf/host-install-releases.toml")
    );
    assert_eq!(host.units.len(), 1);
    assert_eq!(host.tools.curl, Path::new("/usr/bin/curl"));
    assert_eq!(host.tools.tarball_max, 512 * 1024 * 1024);
    assert_eq!(
        host.layout.join(&host.layout.gh()),
        Path::new("/usr/share/kbf/trust/gh")
    );
}

/// A check directory that cannot be made is a failure of the stage, with nothing kept.
#[test]
fn a_check_directory_that_cannot_be_made_fails_the_stage() {
    both(|format| {
        let rig = Rig::new("check-dir", format);
        let work = rig.root.join("var/lib/kbf-installer/work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join(format!("{COMMIT}.check")), b"in the way").unwrap();
        let fault = rig.stage(&new_version()).unwrap_err();
        assert!(
            !fault.refused && fault.code == Code::StageFailed,
            "{fault:?}"
        );
        assert!(rig.kept_files().is_empty());
        assert_eq!(rig.record(), Record::default());
    });
}

/// A download that leaves out any of the three files, though `gh` said it worked, is a refused
/// fetch and keeps nothing.
#[test]
fn a_download_that_leaves_a_file_out_is_refused() {
    both(|format| {
        let own = format!(".{}", format.asset().extension());
        for missing in [own.as_str(), ".sigstore.json", "SHA256SUMS"] {
            let rig = Rig::new("left-out", format);
            rig.net.borrow_mut().skip_files.push(missing.to_owned());
            let fault = rig.stage(&new_version()).unwrap_err();
            assert!(refused(Code::FetchFailed)(&fault), "{missing}: {fault:?}");
            assert!(rig.kept_files().is_empty());
            assert_eq!(rig.record(), Record::default());
            assert!(rig.work().is_empty());
        }
    });
}

/// Tools that cannot be started or fail on the file are refusals of the package, and a package
/// manager that cannot be started to install is a failure that goes back the same way.
#[test]
fn tools_that_cannot_run_are_failures_not_guesses() {
    both(|format| {
        for flag in ["no-inspect-tool", "inspect-fails"] {
            let rig = Rig::new("tools-inspect", format);
            rig.net.borrow_mut().broken.insert(flag);
            let fault = rig.stage(&new_version()).unwrap_err();
            assert!(refused(Code::VerifyFailed)(&fault), "{flag}: {fault:?}");
            rig.assert_nothing_changed();
        }
        let rig = Rig::new("tools-install", format);
        rig.stage(&new_version()).unwrap();
        rig.net.borrow_mut().broken.insert("no-install-tool");
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert_eq!(fault.code, Code::RevertFailed, "{fault:?}");
        assert!(
            fault.detail.contains("cannot start the installer"),
            "{fault:?}"
        );
        assert!(rig.record().pending.is_some());
    });
}

/// A kept copy that went missing since the update that confirmed it is fetched again when the
/// next update needs it as the package to go back to.
#[test]
fn a_running_packages_lost_copy_is_fetched_again_for_the_next_update() {
    both(|format| {
        let rig = Rig::new("lost-running", format);
        let now = Rc::new(std::cell::Cell::new(1_000_u64));
        let ab = rig.ab(now);
        let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
        let (v, c) = (new_version(), c_version());
        ask(&format!("install kbf={v} from stable"));
        ask(&format!("switch kbf={v}"));
        ask(&format!("confirm kbf={v}"));
        fs::remove_file(rig.kept(COMMIT)).unwrap();
        let downloads = rig.count("download");
        ask(&format!("install kbf={c} from stable"));
        assert!(matches!(
            ask(&format!("switch kbf={c}")),
            Answer::Switched { .. }
        ));
        assert_eq!(
            rig.count("download"),
            downloads + 2,
            "the new one and the lost one"
        );
        assert!(rig.kept(COMMIT).exists());
        assert_eq!(rig.installed(), Some(c));
    });
}

/// A going back with no trial is refused; a trial that names no kept copy, or whose kept copy is
/// gone and whose channel is unreadable, cannot be gone back from and says so; so does a staged
/// package with an unreadable channel on a switch.
#[test]
fn a_record_that_cannot_be_gone_back_from_is_a_failure_that_says_so() {
    both(|format| {
        let rig = Rig::new("record", format);
        let pending = Pending {
            commit: COMMIT.to_owned(),
            from: A.to_owned(),
            deadline_unix: Some(1),
            activation: "x".to_owned(),
        };
        let fault = rig.backend.revert(&pending).unwrap_err();
        assert!(refused(Code::NothingPending)(&fault), "{fault:?}");

        let kept = |channel: &str, version: String| super::package_store::Kept {
            version,
            sha256: sha256(b"x"),
            channel: channel.to_owned(),
        };
        let trial = super::package_store::Trial {
            commit: COMMIT.to_owned(),
            from: A.to_owned(),
            deadline_unix: 1,
            activation: "x".to_owned(),
        };
        // No copy kept.
        let mut record = Record {
            pending: Some(trial.clone()),
            ..Record::default()
        };
        rig.backend.store.save(&record).unwrap();
        let fault = rig.backend.revert(&pending).unwrap_err();
        assert!(
            fault.detail.contains("no copy of the package that ran"),
            "{fault:?}"
        );
        // A copy that is not there, kept through a channel that is not a name.
        record
            .files
            .insert(A.to_owned(), kept("Not A Channel", a_version()));
        rig.backend.store.save(&record).unwrap();
        let fault = rig.backend.revert(&pending).unwrap_err();
        assert!(
            fault.detail.contains("kept channel is unreadable"),
            "{fault:?}"
        );
        // A staged package kept through such a channel cannot be switched to.
        record.pending = None;
        record
            .files
            .insert(COMMIT.to_owned(), kept("Not A Channel", new_version()));
        rig.backend.store.save(&record).unwrap();
        fs::create_dir_all(rig.state_dir.join("packages")).unwrap();
        fs::write(rig.kept(COMMIT), b"x").unwrap();
        let fault = rig.backend.switch(COMMIT, 600, 1_000).unwrap_err();
        assert_eq!(fault.code, Code::StateUnavailable, "{fault:?}");
        assert!(rig.installs().is_empty());
    });
}

/// The store's own failures: a record that cannot be written, a directory of kept files that
/// cannot be made, a source that is gone or changed while it is kept, a kept file that is not a
/// file or is too big, and a prune with nothing to prune.
#[test]
fn the_store_says_what_it_could_not_do() {
    let rig = Rig::new("store", Format::Deb);
    let store = &rig.backend.store;

    // The record cannot be written.
    fs::create_dir(rig.state_dir.join("package.json.new")).unwrap();
    assert_eq!(
        store.save(&Record::default()).unwrap_err().code,
        Code::StateUnavailable
    );
    fs::remove_dir(rig.state_dir.join("package.json.new")).unwrap();
    store.save(&Record::default()).unwrap();

    // Nothing to prune from, and nothing kept.
    let mut record = Record::default();
    store.prune(&mut record, &[A]);
    assert!(!store.has(A));

    // The directory of kept files cannot be made.
    let source = rig.dir.join("source");
    fs::write(&source, b"kbf|1|x").unwrap();
    fs::write(rig.state_dir.join("packages"), b"in the way").unwrap();
    let fault = store
        .keep(A, &source, &sha256(b"kbf|1|x"), 1024)
        .unwrap_err();
    assert_eq!(fault.code, Code::StageFailed, "{fault:?}");
    fs::remove_file(rig.state_dir.join("packages")).unwrap();

    // The source is gone, or is not the bytes that were verified.
    let fault = store
        .keep(A, &rig.dir.join("nothing"), &sha256(b"x"), 1024)
        .unwrap_err();
    assert_eq!(fault.code, Code::StageFailed, "{fault:?}");
    let fault = store.keep(A, &source, &sha256(b"other"), 1024).unwrap_err();
    assert!(
        fault.detail.contains("changed while it was kept"),
        "{fault:?}"
    );
    assert!(!store.has(A));
    assert!(
        rig.kept_files().is_empty(),
        "no half-kept file: {:?}",
        rig.kept_files()
    );

    // A kept file that is a directory, or is over the limit, is not the verified file.
    let kept = super::package_store::Kept {
        version: a_version(),
        sha256: sha256(b"kbf|1|x"),
        channel: "stable".to_owned(),
    };
    store.keep(A, &source, &kept.sha256, 1024).unwrap();
    assert!(store.verified(A, &kept, 1024).is_ok());
    assert!(refused(Code::VerifyFailed)(
        &store.verified(A, &kept, 3).unwrap_err()
    ));
    fs::remove_file(store.path(A)).unwrap();
    fs::create_dir(store.path(A)).unwrap();
    assert!(refused(Code::VerifyFailed)(
        &store.verified(A, &kept, 1024).unwrap_err()
    ));
    assert!(!store.has(A));
}

/// A kept package is reused for the very version, and only while the file is there: a newer
/// build of the same commit, or a kept file that went missing, is fetched.
#[test]
fn a_kept_package_is_reused_only_for_its_version_and_while_it_is_there() {
    both(|format| {
        let rig = Rig::new("reuse", format);
        rig.stage(&new_version()).unwrap();
        rig.stage(&new_version()).unwrap();
        assert_eq!(
            rig.count("download"),
            1,
            "the second stage used the kept file"
        );

        // The same commit, built again with another run number.
        let again = build(COMMIT, 7, 99);
        rig.offer(COMMIT, &again);
        assert_eq!(rig.stage(&again).unwrap(), COMMIT);
        assert_eq!(rig.count("download"), 2);
        assert_eq!(rig.record().files[COMMIT].version, again);

        // The record says kept; the file is not there.
        fs::remove_file(rig.kept(COMMIT)).unwrap();
        rig.stage(&again).unwrap();
        assert_eq!(rig.count("download"), 3);
        assert!(rig.kept(COMMIT).exists());
    });
}

/// A record that cannot be read for a reason other than being absent (here a link) is
/// unavailable, and an absent one is empty.
#[test]
fn a_record_that_is_a_link_is_unavailable_and_an_absent_one_is_empty() {
    let rig = Rig::new("record-link", Format::Deb);
    assert_eq!(rig.record(), Record::default());
    std::os::unix::fs::symlink("elsewhere", rig.state_dir.join("package.json")).unwrap();
    assert_eq!(
        rig.backend.pending().unwrap_err().code,
        Code::StateUnavailable
    );
}

/// A `SHA256SUMS` over the limit is not taken, whatever else the release holds.
#[test]
fn sums_over_the_limit_are_not_taken() {
    both(|format| {
        let rig = Rig::new("big-sums", format);
        let listing = rig.net.borrow().releases[0].clone();
        rig.net.borrow_mut().releases[0] = listing.replace(
            "\"SHA256SUMS\",\"size\":",
            "\"SHA256SUMS\",\"size\":99999999",
        );
        let fault = rig.stage(&new_version()).unwrap_err();
        assert!(refused(Code::FetchFailed)(&fault), "{fault:?}");
        rig.assert_nothing_changed();
    });
}

// ---- a deb of the farm: `<semver>+<12 hex>`, with no `g` ----

/// The version text a kbf deb carries today: the semver and the first 12 digits of the commit.
/// The binaries of the same build say `+g<12 hex>`; the deb does not.
pub(crate) fn deb_build(commit: &str, seq: u32) -> String {
    format!("0.1.{seq}+{}", &commit[..12])
}

/// A host whose package database and releases all use the deb's real version shape.
fn deb_rig(name: &str) -> Rig {
    let rig = Rig::new(name, Format::Deb);
    for (commit, seq) in [(COMMIT, 7), (A, 6), (C, 8)] {
        rig.offer(commit, &deb_build(commit, seq));
    }
    rig.net.borrow_mut().installed = Some(deb_build(A, 6));
    rig
}

/// The commit of a deb that no record names is read from its version, `+<12 hex>`, so a host
/// that never staged the package that runs still says which release it is.
#[test]
fn the_current_release_of_a_deb_host_is_read_from_the_debs_version() {
    let rig = deb_rig("deb-current");
    assert_eq!(rig.backend.current().unwrap().as_deref(), Some(&A[..12]));
}

/// A package update on a host with a deb of the real version shape: the version the coordinator
/// names is accepted, the release is fetched and checked against it, the package that runs
/// (named by its own deb version) is kept for the way back, and the new one is installed and
/// confirmed. This is the path a coordinator-driven update takes.
#[test]
fn a_deb_update_stages_keeps_the_running_package_switches_and_confirms() {
    let rig = deb_rig("deb-update");
    let now = Rc::new(std::cell::Cell::new(1_000_u64));
    let ab = rig.ab(now);
    let ask = |line: &str| ab.handle(&LocalRequest::parse(line).unwrap());
    let (a, v) = (deb_build(A, 6), deb_build(COMMIT, 7));

    assert!(matches!(
        ask(&format!("install kbf={v} from stable")),
        Answer::Staged { current: false, .. }
    ));
    assert!(rig.kept(COMMIT).exists());
    assert!(matches!(
        ask(&format!("switch kbf={v}")),
        Answer::Switched { .. }
    ));
    assert!(rig.kept(A).exists(), "the package that ran is kept");
    assert_eq!(rig.installed(), Some(v.clone()));
    assert!(matches!(
        ask(&format!("confirm kbf={v}")),
        Answer::Confirmed { .. }
    ));

    // Back to the first deb, from the copy kept, with no new download.
    let downloads = rig.count("download");
    assert!(matches!(
        ask(&format!("install kbf={a} from stable")),
        Answer::Staged { current: false, .. }
    ));
    assert_eq!(rig.count("download"), downloads, "the kept copy is used");
    assert_eq!(rig.record().files[COMMIT].version, v);
    assert_eq!(rig.record().files[A].version, a);
}

/// A deb whose commit is not the one the request names is refused: the 12 digits are compared
/// exactly, whether or not there is a `g`.
#[test]
fn a_deb_version_of_another_commit_is_not_the_release_asked_for() {
    let rig = deb_rig("deb-other");
    // The release of COMMIT holds a package that says it is the build of C.
    rig.offer(COMMIT, &deb_build(C, 8));
    let fault = rig.stage(&deb_build(COMMIT, 7)).unwrap_err();
    assert!(fault.refused, "{}", fault.detail);
    assert!(fault.detail.contains("not 0.1.7+"), "{}", fault.detail);
    assert!(!rig.kept(COMMIT).exists());
    assert!(rig.installs().is_empty());
}
